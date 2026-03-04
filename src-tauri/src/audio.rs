use std::sync::Mutex;

pub struct AudioPlayer {
    volume: u8,
    is_playing: bool,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self { volume: 80, is_playing: false }
    }
}

static PLAYER: std::sync::OnceLock<Mutex<AudioPlayer>> = std::sync::OnceLock::new();

fn player() -> &'static Mutex<AudioPlayer> {
    PLAYER.get_or_init(|| Mutex::new(AudioPlayer::new()))
}

pub fn set_volume(vol: u8) -> Result<(), String> {
    let mut p = player().lock().map_err(|e| e.to_string())?;
    p.volume = vol.min(100);
    // TODO: apply to rodio sink
    Ok(())
}

pub fn toggle() -> Result<String, String> {
    let mut p = player().lock().map_err(|e| e.to_string())?;
    p.is_playing = !p.is_playing;
    // TODO: pause/resume rodio sink
    Ok(if p.is_playing { "playing".into() } else { "paused".into() })
}
