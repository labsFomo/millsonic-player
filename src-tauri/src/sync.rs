use crate::{api, audio, config};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use chrono::{Datelike, Timelike};
use chrono_tz;
use tokio::sync::Notify;

/// Hash a string to a positive integer (same as web player's hashCode)
fn hash_code(s: &str) -> u32 {
    let mut hash: i32 = 0;
    for c in s.chars() {
        hash = ((hash << 5).wrapping_sub(hash)).wrapping_add(c as i32);
        hash |= 0; // Convert to 32-bit integer
    }
    hash.unsigned_abs()
}

/// Deterministic seeded shuffle matching the web player's seededShuffle
fn seeded_shuffle<T: Clone>(arr: &[T], seed: &str) -> Vec<T> {
    let mut result: Vec<T> = arr.to_vec();
    let mut s = hash_code(seed) as u64;
    for i in (1..result.len()).rev() {
        s = (s.wrapping_mul(1664525).wrapping_add(1013904223)) & 0x7fffffff;
        let j = (s % (i as u64 + 1)) as usize;
        result.swap(i, j);
    }
    result
}

static SYNC_TRIGGER: std::sync::OnceLock<Notify> = std::sync::OnceLock::new();

fn sync_trigger() -> &'static Notify {
    SYNC_TRIGGER.get_or_init(|| Notify::new())
}

/// Call this to trigger an immediate sync (e.g. after pairing)
pub fn trigger_sync() {
    log::info!("Sync triggered manually");
    sync_trigger().notify_one();
}

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
            // When not paired, check every 5s so we pick up pairing quickly
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        // Wait for either 300s or a manual trigger (e.g. after pairing)
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(300)) => {},
            _ = sync_trigger().notified() => {
                log::info!("Sync woken by trigger");
            },
        }
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
    let mut slot_start_time: Option<String> = None;

    // Get zone_id for seeded shuffle
    let zone_id = config::AppConfig::load().zone_id.clone().unwrap_or_default();

    for slot in &slots {
        let slot_day = slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(99) as u32;
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");

        if slot_day == day_of_week && current_time.as_str() >= start && current_time.as_str() < end {
            log::info!("Matched slot: day={} {}-{}", slot_day, start, end);
            slot_start_time = Some(start.to_string());
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

    // Apply seeded shuffle to match web player order
    if !current_tracks.is_empty() {
        let date_str = now.format("%Y-%m-%d").to_string();
        let start_t = slot_start_time.as_deref().unwrap_or("00:00");
        let seed = format!("{}-{}-{}", zone_id, date_str, start_t);
        log::info!("Shuffling {} tracks with seed: {}", current_tracks.len(), seed);
        current_tracks = seeded_shuffle(&current_tracks, &seed);
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

    let total_tracks = current_tracks.len();
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();
    let mut first_track_started = false;

    // Emit connecting phase
    let _ = handle.emit("sync-progress", serde_json::json!({
        "phase": "downloading",
        "current": 0,
        "total": total_tracks,
        "trackName": "",
        "percent": 0
    }));

    for (i, track_val) in current_tracks.iter().enumerate() {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let title = track_val.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let artist = track_val.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let duration = track_val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let artwork_url = track_val.get("artworkUrl").and_then(|v| v.as_str()).map(|s| s.to_string());
        let stream_url = track_val.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("");

        let file_path = cache_dir.join(format!("{}.mp3", track_id));

        // Emit progress
        let percent = ((i as f64 / total_tracks as f64) * 100.0) as u32;
        let _ = handle.emit("sync-progress", serde_json::json!({
            "phase": "downloading",
            "current": i + 1,
            "total": total_tracks,
            "trackName": title,
            "percent": percent
        }));

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

            // We no longer start from the first track — we wait until all are downloaded
            // to calculate the correct schedule position
        }
    }

    // Update playlist with ALL downloaded tracks and calculate schedule position
    if !playlist.is_empty() {
        log::info!("All downloads complete, updating playlist with {} tracks", playlist.len());

        // Calculate current position in schedule
        let start_t = slot_start_time.as_deref().unwrap_or("00:00");
        let parts: Vec<&str> = start_t.split(':').collect();
        let start_h: u32 = parts.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
        let start_m: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let slot_start_secs = (start_h * 3600 + start_m * 60) as f64;
        let now_secs = (now.hour() * 3600 + now.minute() * 60 + now.second()) as f64;
        let elapsed_secs = if now_secs >= slot_start_secs { now_secs - slot_start_secs } else { 0.0 };

        // Build timeline durations to find current track
        let mut total_duration: f64 = 0.0;
        for t in &playlist {
            total_duration += t.duration as f64;
        }

        let start_index = if total_duration > 0.0 {
            let looped_elapsed = elapsed_secs % total_duration;
            let mut accumulated = 0.0;
            let mut idx = 0;
            for (i, t) in playlist.iter().enumerate() {
                accumulated += t.duration as f64;
                if accumulated > looped_elapsed {
                    idx = i;
                    break;
                }
            }
            log::info!("Schedule position: elapsed={:.0}s, total_duration={:.0}s, starting at track {} of {}",
                elapsed_secs, total_duration, idx, playlist.len());
            idx
        } else {
            0
        };

        let mut player = audio::player().lock().map_err(|e| e.to_string())?;
        player.set_playlist(playlist);
        player.current_index = start_index;
        if let Err(e) = player.play_current() {
            log::error!("Failed to start playback: {}", e);
        } else {
            first_track_started = true;
        }
        *current_playlist_id().lock().unwrap() = playlist_id;
        let _ = handle.emit("status-change", serde_json::json!({"playing": true}));
    }

    // Emit final ready (only if we have tracks and started playing)
    if first_track_started {
        let _ = handle.emit("sync-progress", serde_json::json!({
            "phase": "ready",
            "current": total_tracks,
            "total": total_tracks,
            "trackName": "",
            "percent": 100
        }));
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
