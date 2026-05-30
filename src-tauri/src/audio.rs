use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct TrackInfo {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub file_path: String,
    pub duration: f32,
    pub artwork_url: Option<String>,
}

pub struct AudioPlayer {
    volume: f32,
    is_playing: bool,
    pub current_index: usize,
    playlist: Vec<TrackInfo>,
    play_started_at: Option<Instant>,
    pause_elapsed: f32,
    // rodio handles - initialized lazily
    sink: Option<rodio::Sink>,
    _stream: Option<rodio::OutputStream>,
    _stream_handle: Option<rodio::OutputStreamHandle>,
    audio_available: bool,
    pub consecutive_skips: usize,
    // Crossfade support
    crossfade_sink: Option<rodio::Sink>,
    pub crossfade_active: bool,
    crossfade_start: Option<Instant>,
    crossfade_duration_secs: f32,
    // Spot playback
    pub playing_spot: bool,
    pub tracks_since_last_spot: usize,
    // SonicBox: forced next track (a winning vote) + its bookkeeping
    forced_next: Option<TrackInfo>,
    forced_vote_id: Option<String>,
    pub playing_sonicbox: bool,
    sonicbox_current: Option<TrackInfo>,
    sonicbox_vote_id: Option<String>,
}

// Safety: OutputStream is !Send but we only access from one thread via Mutex
unsafe impl Send for AudioPlayer {}

impl AudioPlayer {
    pub fn new() -> Self {
        let (stream, handle, sink, available) = match rodio::OutputStream::try_default() {
            Ok((stream, handle)) => {
                match rodio::Sink::try_new(&handle) {
                    Ok(sink) => {
                        sink.set_volume(0.8);
                        (Some(stream), Some(handle), Some(sink), true)
                    }
                    Err(e) => {
                        log::warn!("Could not create audio sink: {}", e);
                        (None, None, None, false)
                    }
                }
            }
            Err(e) => {
                log::warn!("No audio output available: {}", e);
                (None, None, None, false)
            }
        };

        Self {
            volume: 0.8,
            is_playing: false,
            current_index: 0,
            playlist: Vec::new(),
            play_started_at: None,
            pause_elapsed: 0.0,
            sink,
            _stream: stream,
            _stream_handle: handle,
            audio_available: available,
            consecutive_skips: 0,
            crossfade_sink: None,
            crossfade_active: false,
            crossfade_start: None,
            crossfade_duration_secs: 3.0,
            playing_spot: false,
            tracks_since_last_spot: 0,
            forced_next: None,
            forced_vote_id: None,
            playing_sonicbox: false,
            sonicbox_current: None,
            sonicbox_vote_id: None,
        }
    }

    // ── SonicBox forced-track API ───────────────────────────────────────────
    /// Store/refresh the winning vote as the forced next track. Called by the
    /// SonicBox watcher (background pre-cache + late authoritative re-check).
    pub fn set_sonicbox_next(&mut self, track: TrackInfo, vote_id: String) {
        self.forced_next = Some(track);
        self.forced_vote_id = Some(vote_id);
    }

    /// Drop any pending forced track (no active winning vote anymore).
    pub fn clear_sonicbox_next(&mut self) {
        self.forced_next = None;
        self.forced_vote_id = None;
    }

    pub fn forced_vote_id(&self) -> Option<&str> {
        self.forced_vote_id.as_deref()
    }

    /// The SonicBox track currently playing (for crossfade duration/reporting).
    pub fn sonicbox_current(&self) -> Option<&TrackInfo> {
        self.sonicbox_current.as_ref()
    }

    pub fn sonicbox_vote_id(&self) -> Option<&str> {
        self.sonicbox_vote_id.as_deref()
    }

    pub fn has_sonicbox_next(&self) -> bool {
        self.forced_next.is_some()
    }

    /// Consume the forced track + its vote id (clears the slot).
    pub fn take_sonicbox_next(&mut self) -> Option<(TrackInfo, String)> {
        let vote = self.forced_vote_id.take();
        self.forced_next.take().map(|t| (t, vote.unwrap_or_default()))
    }

    /// Mark that a SonicBox track is now playing; remembers the track (for its
    /// real duration / id) and vote id so we can report the completion to
    /// /player/play-report when it finishes.
    pub fn set_playing_sonicbox(&mut self, track: TrackInfo, vote_id: String) {
        self.playing_sonicbox = true;
        self.sonicbox_current = Some(track);
        self.sonicbox_vote_id = Some(vote_id);
    }

    /// Finish the SonicBox track: returns (track, vote_id) for reporting.
    pub fn clear_playing_sonicbox(&mut self) -> Option<(TrackInfo, String)> {
        self.playing_sonicbox = false;
        let track = self.sonicbox_current.take();
        let vote = self.sonicbox_vote_id.take();
        match (track, vote) {
            (Some(t), Some(v)) => Some((t, v)),
            _ => None,
        }
    }

    pub fn play_file(&mut self, track: &TrackInfo) -> Result<(), String> {
        self.play_file_at(track, 0.0)
    }

    /// Play a track, optionally seeking `seek_secs` into it (U5 — start at the
    /// exact second the schedule timeline dictates, matching the web player).
    /// The seek is done by decoding-and-discarding the head (one-time cost,
    /// reliable for mp3 regardless of decoder seek support).
    pub fn play_file_at(&mut self, track: &TrackInfo, seek_secs: f32) -> Result<(), String> {
        use rodio::Source;
        let seek = seek_secs.max(0.0);
        log::info!("play_file: '{}' at {} (seek={:.0}s)", track.title, track.file_path, seek);

        if !self.audio_available {
            log::warn!("Audio not available, simulating playback of: {}", track.title);
            self.is_playing = true;
            self.play_started_at = Some(Instant::now());
            self.pause_elapsed = seek;
            return Ok(());
        }

        let metadata = std::fs::metadata(&track.file_path)
            .map_err(|e| format!("Cannot stat file {}: {}", track.file_path, e))?;
        log::info!("File size: {} bytes", metadata.len());
        if metadata.len() < 1000 {
            return Err(format!("File too small ({} bytes), likely corrupt: {}", metadata.len(), track.file_path));
        }

        let file = std::fs::File::open(&track.file_path)
            .map_err(|e| format!("Cannot open file {}: {}", track.file_path, e))?;
        let reader = std::io::BufReader::new(file);
        let source = rodio::Decoder::new(reader)
            .map_err(|e| format!("Cannot decode audio {}: {}", track.file_path, e))?;
        log::info!("Audio decoded OK");

        if let Some(ref sink) = self.sink {
            sink.stop();
        }
        // Drop old crossfade sink if any
        self.crossfade_sink = None;
        self.crossfade_active = false;
        self.crossfade_start = None;

        if let Some(ref handle) = self._stream_handle {
            match rodio::Sink::try_new(handle) {
                Ok(new_sink) => {
                    new_sink.set_volume(self.volume);
                    // Only seek when meaningfully into the track (>0.5s) and not
                    // past its end, to avoid wasted work / overshoot.
                    if seek > 0.5 && (track.duration <= 0.0 || seek < track.duration - 1.0) {
                        new_sink.append(source.skip_duration(std::time::Duration::from_secs_f32(seek)));
                        log::info!("Sink created with seek to {:.0}s, playing", seek);
                    } else {
                        new_sink.append(source);
                        log::info!("Sink created and source appended, playing");
                    }
                    self.sink = Some(new_sink);
                }
                Err(e) => return Err(format!("Cannot create sink: {}", e)),
            }
        } else {
            return Err("No audio stream handle available".to_string());
        }

        self.is_playing = true;
        self.play_started_at = Some(Instant::now());
        self.pause_elapsed = seek;
        Ok(())
    }

    /// Start crossfade: begin fading out current sink, create new sink with next track fading in
    pub fn start_crossfade(&mut self, next_track: &TrackInfo) -> Result<(), String> {
        if !self.audio_available {
            return Ok(());
        }

        let file = std::fs::File::open(&next_track.file_path)
            .map_err(|e| format!("Cannot open file {}: {}", next_track.file_path, e))?;
        let reader = std::io::BufReader::new(file);
        let source = rodio::Decoder::new(reader)
            .map_err(|e| format!("Cannot decode audio {}: {}", next_track.file_path, e))?;

        if let Some(ref handle) = self._stream_handle {
            match rodio::Sink::try_new(handle) {
                Ok(new_sink) => {
                    new_sink.set_volume(0.0); // Start silent, will fade in
                    new_sink.append(source);
                    // Move current sink to crossfade_sink (will fade out)
                    self.crossfade_sink = self.sink.take();
                    self.sink = Some(new_sink);
                    self.crossfade_active = true;
                    self.crossfade_start = Some(Instant::now());
                    log::info!("Crossfade started for '{}'", next_track.title);
                    Ok(())
                }
                Err(e) => Err(format!("Cannot create crossfade sink: {}", e)),
            }
        } else {
            Err("No audio stream handle".to_string())
        }
    }

    /// Update crossfade volumes. Returns true if crossfade is complete.
    pub fn update_crossfade(&mut self) -> bool {
        if !self.crossfade_active {
            return false;
        }
        let elapsed = match self.crossfade_start {
            Some(start) => start.elapsed().as_secs_f32(),
            None => return false,
        };
        let progress = (elapsed / self.crossfade_duration_secs).min(1.0);

        // Fade in new sink
        if let Some(ref sink) = self.sink {
            sink.set_volume(self.volume * progress);
        }
        // Fade out old sink
        if let Some(ref old_sink) = self.crossfade_sink {
            old_sink.set_volume(self.volume * (1.0 - progress));
        }

        if progress >= 1.0 {
            // Crossfade complete, drop old sink
            if let Some(old) = self.crossfade_sink.take() {
                old.stop();
            }
            self.crossfade_active = false;
            self.crossfade_start = None;
            log::info!("Crossfade complete");
            return true;
        }
        false
    }

    pub fn reset_position(&mut self) {
        self.play_started_at = Some(Instant::now());
        self.pause_elapsed = 0.0;
    }

    pub fn set_crossfade_duration(&mut self, secs: f32) {
        self.crossfade_duration_secs = secs;
    }

    /// Play a spot audio file (one-shot, not part of playlist)
    pub fn play_spot_file(&mut self, file_path: &str) -> Result<(), String> {
        log::info!("Playing spot: {}", file_path);
        self.playing_spot = true;
        // Reset position timer so is_finished() doesn't immediately trigger
        self.play_started_at = Some(Instant::now());
        self.pause_elapsed = 0.0;

        if !self.audio_available {
            self.is_playing = true;
            return Ok(());
        }

        let metadata = std::fs::metadata(file_path)
            .map_err(|e| format!("Cannot stat spot file {}: {}", file_path, e))?;
        if metadata.len() < 500 {
            self.playing_spot = false;
            return Err(format!("Spot file too small: {}", file_path));
        }

        let file = std::fs::File::open(file_path)
            .map_err(|e| format!("Cannot open spot file {}: {}", file_path, e))?;
        let reader = std::io::BufReader::new(file);
        let source = rodio::Decoder::new(reader)
            .map_err(|e| format!("Cannot decode spot {}: {}", file_path, e))?;

        if let Some(ref sink) = self.sink {
            sink.stop();
        }
        if let Some(ref handle) = self._stream_handle {
            match rodio::Sink::try_new(handle) {
                Ok(new_sink) => {
                    new_sink.set_volume(self.volume);
                    new_sink.append(source);
                    self.sink = Some(new_sink);
                }
                Err(e) => {
                    self.playing_spot = false;
                    return Err(format!("Cannot create sink for spot: {}", e));
                }
            }
        }

        self.is_playing = true;
        self.play_started_at = Some(Instant::now());
        self.pause_elapsed = 0.0;
        Ok(())
    }

    pub fn pause(&mut self) {
        if let Some(ref sink) = self.sink {
            sink.pause();
        }
        if self.is_playing {
            if let Some(started) = self.play_started_at {
                self.pause_elapsed += started.elapsed().as_secs_f32();
            }
            self.play_started_at = None;
        }
        self.is_playing = false;
    }

    pub fn resume(&mut self) {
        if let Some(ref sink) = self.sink {
            sink.play();
        }
        self.play_started_at = Some(Instant::now());
        self.is_playing = true;
    }

    pub fn stop(&mut self) {
        if let Some(ref sink) = self.sink {
            sink.stop();
        }
        if let Some(ref old) = self.crossfade_sink {
            old.stop();
        }
        self.crossfade_sink = None;
        self.crossfade_active = false;
        self.is_playing = false;
        self.play_started_at = None;
        self.pause_elapsed = 0.0;
        self.playing_spot = false;
    }

    pub fn set_volume(&mut self, vol: u8) {
        self.volume = (vol.min(100) as f32) / 100.0;
        if let Some(ref sink) = self.sink {
            sink.set_volume(self.volume);
        }
    }

    pub fn get_volume(&self) -> u8 {
        (self.volume * 100.0) as u8
    }

    pub fn is_finished(&self) -> bool {
        // Position-based fallback: if position >= duration - 0.5s, consider finished
        // This handles cases where rodio sink.empty() doesn't return true reliably
        // Skip for spots (spot duration unknown, rely on sink.empty() only).
        // For a SonicBox track use its own duration, not the playlist track's.
        if self.playing_sonicbox {
            if let Some(track) = &self.sonicbox_current {
                if track.duration > 1.0 && self.get_position() >= track.duration - 0.5 {
                    return true;
                }
            }
        } else if !self.playing_spot {
            if let Some(track) = self.current_track() {
                if track.duration > 1.0 && self.get_position() >= track.duration - 0.5 {
                    return true;
                }
            }
        }

        if !self.audio_available {
            if self.playing_sonicbox {
                if let Some(track) = &self.sonicbox_current {
                    return self.get_position() >= track.duration;
                }
            }
            if let Some(track) = self.current_track() {
                return self.get_position() >= track.duration;
            }
            return true;
        }
        match &self.sink {
            Some(sink) => sink.empty(),
            None => true,
        }
    }

    pub fn get_position(&self) -> f32 {
        let live = match self.play_started_at {
            Some(started) => started.elapsed().as_secs_f32(),
            None => 0.0,
        };
        self.pause_elapsed + live
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing
    }

    pub fn set_playlist(&mut self, tracks: Vec<TrackInfo>) {
        self.playlist = tracks;
        self.current_index = 0;
    }

    pub fn current_track(&self) -> Option<&TrackInfo> {
        self.playlist.get(self.current_index)
    }

    pub fn play_current(&mut self) -> Result<(), String> {
        if let Some(track) = self.playlist.get(self.current_index).cloned() {
            self.play_file(&track)
        } else {
            Err("No track at current index".into())
        }
    }

    /// Play the current track seeking `seek_secs` in (U5, used at startup).
    pub fn play_current_at(&mut self, seek_secs: f32) -> Result<(), String> {
        if let Some(track) = self.playlist.get(self.current_index).cloned() {
            self.play_file_at(&track, seek_secs)
        } else {
            Err("No track at current index".into())
        }
    }

    /// R-10: if the player drifted to a DIFFERENT song than the server says is
    /// current, jump to the right one (+ seek). No-op if already on the right
    /// song or if a spot / SonicBox track is mid-interrupt. Returns the current
    /// track id (for the caller to know what's playing) and whether it jumped.
    pub fn current_track_id(&self) -> Option<String> {
        self.playlist.get(self.current_index).map(|t| t.track_id.clone())
    }

    pub fn resync_to(&mut self, track_id: &str, seek_secs: f32) -> bool {
        if self.playing_spot || self.playing_sonicbox {
            return false;
        }
        let cur = self.playlist.get(self.current_index).map(|t| t.track_id.as_str());
        if cur == Some(track_id) {
            return false; // already on the right song — never jump mid-song
        }
        if let Some(idx) = self.playlist.iter().position(|t| t.track_id == track_id) {
            self.current_index = idx;
            let _ = self.play_current_at(seek_secs);
            return true;
        }
        false
    }

    pub fn advance(&mut self) -> bool {
        if self.current_index + 1 < self.playlist.len() {
            self.current_index += 1;
            true
        } else {
            self.current_index = 0;
            !self.playlist.is_empty()
        }
    }

    /// Get the next track without advancing
    pub fn peek_next(&self) -> Option<&TrackInfo> {
        let next = if self.current_index + 1 < self.playlist.len() {
            self.current_index + 1
        } else {
            0
        };
        self.playlist.get(next)
    }

    pub fn playlist_len(&self) -> usize {
        self.playlist.len()
    }

    pub fn skip_track(&mut self) -> Result<(), String> {
        self.advance();
        self.play_current()
    }
}

static PLAYER: OnceLock<Mutex<AudioPlayer>> = OnceLock::new();

pub fn player() -> &'static Mutex<AudioPlayer> {
    PLAYER.get_or_init(|| Mutex::new(AudioPlayer::new()))
}

pub fn set_volume(vol: u8) -> Result<(), String> {
    player().lock().map_err(|e| e.to_string())?.set_volume(vol);
    Ok(())
}

// P0-2: audio pre-flight. Returns false when no output device could be opened
// at startup (no ALSA/Pulse/PipeWire sink) so the UI can warn instead of
// silently "playing" into the void.
pub fn is_available() -> bool {
    player()
        .lock()
        .map(|p| p.audio_available)
        .unwrap_or(false)
}

pub fn toggle() -> Result<String, String> {
    let mut p = player().lock().map_err(|e| e.to_string())?;
    if p.is_playing() {
        p.pause();
        Ok("paused".into())
    } else {
        if p.playlist.is_empty() {
            log::warn!("Toggle play ignored: no playlist loaded");
            return Ok("paused".into());
        }
        p.resume();
        Ok("playing".into())
    }
}

#[cfg(test)]
mod sonicbox_tests {
    use super::*;

    fn track(id: &str, dur: f32) -> TrackInfo {
        TrackInfo {
            track_id: id.to_string(),
            title: format!("Track {id}"),
            artist: "Tester".to_string(),
            file_path: format!("/tmp/{id}.mp3"),
            duration: dur,
            artwork_url: None,
        }
    }

    // On the CI/headless host there is no audio device, so AudioPlayer runs in
    // simulated mode (audio_available=false): play_file() succeeds without
    // touching the file and is_finished() is driven purely by position vs the
    // track's own duration. That's exactly what we need to test the SonicBox
    // state machine deterministically.

    #[test]
    fn staging_and_consuming_a_vote() {
        let mut p = AudioPlayer::new();
        assert!(!p.has_sonicbox_next());

        p.set_sonicbox_next(track("voted-1", 180.0), "vote-abc".to_string());
        assert!(p.has_sonicbox_next());
        assert_eq!(p.forced_vote_id(), Some("vote-abc"));

        let (t, vote) = p.take_sonicbox_next().expect("forced track present");
        assert_eq!(t.track_id, "voted-1");
        assert_eq!(vote, "vote-abc");
        assert!(!p.has_sonicbox_next(), "slot cleared after take");
    }

    #[test]
    fn clearing_an_absent_vote_is_safe() {
        let mut p = AudioPlayer::new();
        p.set_sonicbox_next(track("voted-1", 10.0), "v1".to_string());
        p.clear_sonicbox_next();
        assert!(!p.has_sonicbox_next());
        assert!(p.take_sonicbox_next().is_none());
    }

    #[test]
    fn voted_track_finishes_on_its_own_duration_then_resumes_grid() {
        let mut p = AudioPlayer::new();
        // Grid: two normal tracks, currently on index 0.
        p.set_playlist(vec![track("grid-0", 200.0), track("grid-1", 200.0)]);
        assert_eq!(p.current_index, 0);

        // A vote is staged and then consumed at the (simulated) end of grid-0.
        p.set_sonicbox_next(track("voted-x", 1.5), "vote-x".to_string());
        let (sb, vote) = p.take_sonicbox_next().unwrap();
        p.play_file(&sb).expect("simulated play ok");
        p.set_playing_sonicbox(sb, vote);
        assert!(p.playing_sonicbox);

        // Right after start it is NOT finished (uses the SB track's 1.5s, not
        // the grid track's 200s).
        assert!(!p.is_finished(), "voted track should still be playing");

        // After its own duration elapses, it IS finished.
        std::thread::sleep(std::time::Duration::from_millis(1700));
        assert!(p.is_finished(), "voted track should be finished by its own duration");

        // Closing the loop hands back the vote id for /player/play-report.
        let (done_track, done_vote) = p.clear_playing_sonicbox().expect("sb bookkeeping");
        assert_eq!(done_track.track_id, "voted-x");
        assert_eq!(done_vote, "vote-x");
        assert!(!p.playing_sonicbox);

        // Grid resumes: advance moves to grid-1 (the vote did NOT consume a
        // grid slot — index was still 0 during the interrupt).
        assert!(p.advance());
        assert_eq!(p.current_index, 1);
        assert_eq!(p.current_track().unwrap().track_id, "grid-1");
    }
}
