use crate::{api, audio, config};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use chrono::Datelike;
use chrono_tz;

/// Track the current playlist ID to avoid restarting playback on re-sync
static CURRENT_PLAYLIST_ID: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();

fn current_playlist_id() -> &'static std::sync::Mutex<Option<String>> {
    CURRENT_PLAYLIST_ID.get_or_init(|| std::sync::Mutex::new(None))
}

pub async fn start_sync_loop(handle: AppHandle) {
    log::info!("Sync loop started");
    loop {
        let cfg = config::AppConfig::load();
        if cfg.is_paired() {
            let device_id = cfg.device_id.clone().unwrap();
            let device_token = cfg.device_token.clone().unwrap();

            match do_sync(&handle, &device_id, &device_token).await {
                Ok(_) => log::info!("Sync completed successfully"),
                Err(e) => log::error!("Sync error: {}", e),
            }
        } else {
            log::info!("Not paired, waiting...");
        }

        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

async fn do_sync(
    handle: &AppHandle,
    device_id: &str,
    device_token: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sync_data = api::sync_device(device_id, device_token).await?;

    // Check for API error
    if sync_data.get("statusCode").is_some() {
        let msg = sync_data.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
        return Err(format!("Sync API error: {}", msg).into());
    }

    // Apply volume from server if present
    if let Some(device) = sync_data.get("device") {
        if let Some(vol) = device.get("volume").and_then(|v| v.as_u64()) {
            let _ = audio::set_volume(vol as u8);
        }
    }

    // Get timezone from sync response
    let tz_str = sync_data.get("timezone").and_then(|v| v.as_str()).unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    let now = chrono::Utc::now().with_timezone(&tz);

    // Save timezone in config for offline use
    let tz_owned = tz_str.to_string();
    config::update_and_save_global(|c| {
        c.timezone = Some(tz_owned.clone());
    });

    // Parse schedule and find current slot
    let schedule = sync_data.get("schedule").cloned().unwrap_or(serde_json::json!([]));
    let slots = schedule.as_array().cloned().unwrap_or_default();

    // API uses 0=Sunday, 1=Monday...6=Saturday
    let day_of_week = now.weekday().num_days_from_sunday(); // 0=Sun, 1=Mon...6=Sat
    let current_time = now.format("%H:%M").to_string();

    log::info!("Looking for schedule: dayOfWeek={}, time={}", day_of_week, current_time);

    let mut current_tracks: Vec<serde_json::Value> = Vec::new();
    let mut playlist_id: Option<String> = None;

    for slot in &slots {
        let slot_day = slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(99) as u32;
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");

        if slot_day == day_of_week && current_time.as_str() >= start && current_time.as_str() < end {
            log::info!("Matched slot: day={} {}-{}", slot_day, start, end);
            if let Some(playlist) = slot.get("playlist") {
                playlist_id = playlist.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(tracks) = playlist.get("tracks").and_then(|t| t.as_array()) {
                    current_tracks = tracks.clone();
                    log::info!("Found {} tracks in playlist '{}'",
                        tracks.len(),
                        playlist.get("name").and_then(|n| n.as_str()).unwrap_or("?"));
                }
            }
            break;
        }
    }

    if current_tracks.is_empty() {
        log::info!("No tracks scheduled for current time slot");
        // Emit "no schedule" to frontend
        let _ = handle.emit("now-playing", serde_json::json!({
            "title": "Sin programación",
            "artist": "No hay música programada en este horario",
            "duration": 0,
            "position": 0,
            "artworkUrl": null,
        }));
        // Stop playback if playing
        if let Ok(mut player) = audio::player().lock() {
            if player.is_playing() {
                player.stop();
            }
        }
        *current_playlist_id().lock().unwrap() = None;
        return Ok(());
    }

    // Check if playlist changed — if same playlist is already playing, just refresh cache (URLs)
    let same_playlist = {
        let cur = current_playlist_id().lock().unwrap();
        cur.as_deref() == playlist_id.as_deref()
    };

    if same_playlist {
        // Same playlist — just re-download any missing tracks (URLs refreshed)
        log::info!("Same playlist still active, refreshing track cache only");
        refresh_track_cache(&current_tracks).await;
        return Ok(());
    }

    // Different playlist or first sync — download and start playing
    let cache_dir = config::AppConfig::cache_dir();
    std::fs::create_dir_all(&cache_dir)?;

    let mut playlist: Vec<audio::TrackInfo> = Vec::new();

    for track_val in &current_tracks {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let title = track_val.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let artist = track_val.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let duration = track_val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let artwork_url = track_val.get("artworkUrl").and_then(|v| v.as_str()).map(|s| s.to_string());
        let stream_url = track_val.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("");

        let file_path = cache_dir.join(format!("{}.mp3", track_id));

        // Download if not cached
        if !file_path.exists() && !stream_url.is_empty() {
            log::info!("Downloading track: {} - {}", track_id, title);
            match api::download_track(stream_url, &file_path).await {
                Ok(_) => log::info!("Downloaded: {}", title),
                Err(e) => {
                    log::error!("Failed to download {}: {}", title, e);
                    continue;
                }
            }
        }

        if file_path.exists() {
            playlist.push(audio::TrackInfo {
                track_id: track_id.to_string(),
                title: title.to_string(),
                artist: artist.to_string(),
                file_path: file_path.to_string_lossy().to_string(),
                duration,
                artwork_url,
            });
        }
    }

    // Log playlist file status
    for t in &playlist {
        let exists = std::path::Path::new(&t.file_path).exists();
        let size = std::fs::metadata(&t.file_path).map(|m| m.len()).unwrap_or(0);
        log::info!("Playlist track: '{}' | file={} | exists={} | size={} bytes", t.title, t.file_path, exists, size);
    }

    if !playlist.is_empty() {
        log::info!("Starting playback with {} tracks", playlist.len());
        let mut player = audio::player().lock().map_err(|e| e.to_string())?;
        player.set_playlist(playlist);
        if let Err(e) = player.play_current() {
            log::error!("Failed to start playback: {}", e);
        }
        // Update current playlist ID
        *current_playlist_id().lock().unwrap() = playlist_id;
        // Notify frontend
        let _ = handle.emit("status-change", serde_json::json!({"playing": true}));
    }

    Ok(())
}

async fn refresh_track_cache(tracks: &[serde_json::Value]) {
    let cache_dir = config::AppConfig::cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);

    for track_val in tracks {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let stream_url = track_val.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("");
        let file_path = cache_dir.join(format!("{}.mp3", track_id));

        if !file_path.exists() && !stream_url.is_empty() {
            let title = track_val.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            log::info!("Downloading missing track: {}", title);
            if let Err(e) = api::download_track(stream_url, &file_path).await {
                log::error!("Failed to download {}: {}", title, e);
            }
        }
    }
}

/// Called from main.rs every second to check if track ended and advance
pub fn check_track_advancement(handle: &AppHandle) {
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(_) => return,
    };

    if !player.is_playing() {
        return;
    }

    if !player.is_finished() {
        // Track is still playing — reset consecutive_skips after 5s of real playback
        if player.get_position() > 5.0 {
            player.consecutive_skips = 0;
        }
        return;
    }

    // Track finished - check if it played for a reasonable time
    let position = player.get_position();
    if let Some(track) = player.current_track() {
        if position < 3.0 && track.duration > 5.0 {
            log::error!("Track '{}' finished after only {:.1}s (expected {:.0}s) - file may be corrupt: {}",
                track.title, position, track.duration, track.file_path);
            if let Ok(meta) = std::fs::metadata(&track.file_path) {
                log::error!("File size: {} bytes", meta.len());
            } else {
                log::error!("File does NOT exist: {}", track.file_path);
            }
            player.consecutive_skips += 1;
        } else {
            player.consecutive_skips = 0;
        }
    }

    // Check if all tracks failed consecutively
    if player.consecutive_skips >= player.playlist_len() && player.playlist_len() > 0 {
        log::error!("All {} tracks failed to play. Stopping.", player.playlist_len());
        player.stop();
        let _ = handle.emit("now-playing", serde_json::json!({
            "title": "Error de reproducción",
            "artist": "No se pudieron reproducir las pistas",
            "duration": 0,
            "position": 0,
            "artworkUrl": null,
        }));
        return;
    }

    log::info!("Track finished (pos={:.1}s), advancing...", position);
    if player.advance() {
        if let Some(track) = player.current_track() {
            log::info!("Next track: '{}' by '{}' ({})", track.title, track.artist, track.file_path);
        }
        if let Err(e) = player.play_current() {
            log::error!("Error playing next track: {}", e);
            player.consecutive_skips += 1;
        }
    }

    // Emit now-playing update
    if let Some(track) = player.current_track() {
        let _ = handle.emit("now-playing", serde_json::json!({
            "title": track.title,
            "artist": track.artist,
            "duration": track.duration,
            "position": 0.0,
            "artworkUrl": track.artwork_url,
        }));
    }
}
