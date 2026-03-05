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
        }
    }

    pub fn play_file(&mut self, track: &TrackInfo) -> Result<(), String> {
        log::info!("play_file: '{}' at {}", track.title, track.file_path);

        if !self.audio_available {
            log::warn!("Audio not available, simulating playback of: {}", track.title);
            self.is_playing = true;
            self.play_started_at = Some(Instant::now());
            self.pause_elapsed = 0.0;
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
                    new_sink.append(source);
                    log::info!("Sink created and source appended, playing");
                    self.sink = Some(new_sink);
                }
                Err(e) => return Err(format!("Cannot create sink: {}", e)),
            }
        } else {
            return Err("No audio stream handle available".to_string());
        }

        self.is_playing = true;
        self.play_started_at = Some(Instant::now());
        self.pause_elapsed = 0.0;
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

        if !self.audio_available {
            self.is_playing = true;
            self.play_started_at = Some(Instant::now());
            self.pause_elapsed = 0.0;
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
        if !self.audio_available {
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
