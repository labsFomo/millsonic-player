use crate::{api, audio, config};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

pub async fn start_sync_loop(handle: AppHandle) {
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

    // Parse schedule and find current slot
    let schedule = sync_data.get("schedule").cloned().unwrap_or(serde_json::json!([]));
    let slots = schedule.as_array().cloned().unwrap_or_default();

    let now = chrono::Local::now();
    let day_of_week = now.format("%u").to_string().parse::<u32>().unwrap_or(1); // 1=Mon
    let current_time = now.format("%H:%M:%S").to_string();

    let mut current_tracks: Vec<serde_json::Value> = Vec::new();

    for slot in &slots {
        let slot_day = slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(0) as u32;
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59:59");

        if slot_day == day_of_week && current_time.as_str() >= start && current_time.as_str() < end {
            // Found matching slot - get playlist tracks
            if let Some(playlist) = slot.get("playlist") {
                if let Some(tracks) = playlist.get("tracks").and_then(|t| t.as_array()) {
                    current_tracks = tracks.clone();
                }
            }
            break;
        }
    }

    if current_tracks.is_empty() {
        log::info!("No tracks scheduled for current time slot");
        return Ok(());
    }

    // Download tracks and build playlist
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

    if !playlist.is_empty() {
        let mut player = audio::player().lock().map_err(|e| e.to_string())?;
        player.set_playlist(playlist);
        if let Err(e) = player.play_current() {
            log::error!("Failed to start playback: {}", e);
        }
        // Notify frontend
        let _ = handle.emit("status-change", serde_json::json!({"playing": true}));
    }

    Ok(())
}

/// Called from main.rs every second to check if track ended and advance
pub fn check_track_advancement(handle: &AppHandle) {
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(_) => return,
    };

    if player.is_playing() && player.is_finished() {
        log::info!("Track finished, advancing...");
        if player.advance() {
            if let Err(e) = player.play_current() {
                log::error!("Error playing next track: {}", e);
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
}
