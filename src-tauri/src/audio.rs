use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use serde::Serialize;

// We keep rodio types behind an Option so the struct is Send.
// OutputStream is !Send, so we store a flag and recreate as needed.
// For a headless VPS build, we make audio optional.

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
    current_index: usize,
    playlist: Vec<TrackInfo>,
    play_started_at: Option<Instant>,
    pause_elapsed: f32,
    // rodio handles - initialized lazily
    sink: Option<rodio::Sink>,
    _stream: Option<rodio::OutputStream>,
    _stream_handle: Option<rodio::OutputStreamHandle>,
    audio_available: bool,
}

// Safety: OutputStream is !Send but we only access from one thread via Mutex
unsafe impl Send for AudioPlayer {}

impl AudioPlayer {
    pub fn new() -> Self {
        // Try to init audio output
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
        }
    }

    pub fn play_file(&mut self, track: &TrackInfo) -> Result<(), String> {
        if !self.audio_available {
            log::info!("Audio not available, simulating playback of: {}", track.title);
            self.is_playing = true;
            self.play_started_at = Some(Instant::now());
            self.pause_elapsed = 0.0;
            return Ok(());
        }

        let file = std::fs::File::open(&track.file_path)
            .map_err(|e| format!("Cannot open file {}: {}", track.file_path, e))?;
        let reader = std::io::BufReader::new(file);
        let source = rodio::Decoder::new(reader)
            .map_err(|e| format!("Cannot decode audio: {}", e))?;

        if let Some(ref sink) = self.sink {
            sink.stop();
            // Need to recreate sink after stop
        }
        // Recreate sink
        if let Some(ref handle) = self._stream_handle {
            match rodio::Sink::try_new(handle) {
                Ok(new_sink) => {
                    new_sink.set_volume(self.volume);
                    new_sink.append(source);
                    self.sink = Some(new_sink);
                }
                Err(e) => return Err(format!("Cannot create sink: {}", e)),
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
        self.is_playing = false;
        self.play_started_at = None;
        self.pause_elapsed = 0.0;
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
            // Simulate: track ends after its duration
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
            // Loop back to start
            self.current_index = 0;
            !self.playlist.is_empty()
        }
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
        p.resume();
        Ok("playing".into())
    }
}
