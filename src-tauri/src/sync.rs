use crate::{api, audio, config, db};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use chrono::{Datelike, Timelike, Utc};
use chrono_tz;
use tokio::sync::Notify;
use std::sync::{Mutex, OnceLock};

/// Connection status: Online, Offline (cached), Emergency
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionStatus {
    Online,
    Offline,
    Emergency,
}

static CONNECTION_STATUS: OnceLock<Mutex<ConnectionStatus>> = OnceLock::new();

fn conn_status() -> &'static Mutex<ConnectionStatus> {
    CONNECTION_STATUS.get_or_init(|| Mutex::new(ConnectionStatus::Online))
}

pub fn get_connection_status() -> ConnectionStatus {
    *conn_status().lock().unwrap_or_else(|e| e.into_inner())
}

fn set_connection_status(status: ConnectionStatus, handle: &AppHandle) {
    let mut s = conn_status().lock().unwrap_or_else(|e| e.into_inner());
    if *s != status {
        log::info!("Connection status: {:?} → {:?}", *s, status);
        *s = status;
    }
    let status_str = match status {
        ConnectionStatus::Online => "online",
        ConnectionStatus::Offline => "offline",
        ConnectionStatus::Emergency => "emergency",
    };
    let _ = handle.emit("connection-status", serde_json::json!({ "status": status_str }));
}

/// Hash a string to a positive integer (same as web player's hashCode)
fn hash_code(s: &str) -> u32 {
    let mut hash: i32 = 0;
    for c in s.chars() {
        hash = ((hash << 5).wrapping_sub(hash)).wrapping_add(c as i32);
        hash |= 0;
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

static SYNC_TRIGGER: OnceLock<Notify> = OnceLock::new();

fn sync_trigger() -> &'static Notify {
    SYNC_TRIGGER.get_or_init(|| Notify::new())
}

/// Call this to trigger an immediate sync (e.g. after pairing)
pub fn trigger_sync() {
    log::info!("Sync triggered manually");
    sync_trigger().notify_one();
}

/// Track the current playlist ID to avoid restarting playback on re-sync
static CURRENT_PLAYLIST_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn current_playlist_id() -> &'static Mutex<Option<String>> {
    CURRENT_PLAYLIST_ID.get_or_init(|| Mutex::new(None))
}

pub async fn start_sync_loop(handle: AppHandle) {
    log::info!("Sync loop started");

    // Init DB on first run
    let _ = db::db();

    loop {
        let cfg = config::AppConfig::load();
        if cfg.is_paired() {
            let device_id = cfg.device_id.clone().unwrap();
            let device_token = cfg.device_token.clone().unwrap();

            match do_sync(&handle, &device_id, &device_token).await {
                Ok(_) => log::info!("Sync completed successfully"),
                Err(e) => {
                    log::error!("Sync error: {}", e);
                    // Try offline fallback
                    handle_offline_fallback(&handle, &cfg);
                }
            }

            // After sync, try to flush pending play reports
            flush_pending_reports(&device_id, &device_token).await;

            // Run LRU cache cleanup
            db::cleanup_cache();
            db::cleanup_sent_reports();
        } else {
            log::info!("Not paired, waiting...");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        // Wait for either 300s or a manual trigger
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(300)) => {},
            _ = sync_trigger().notified() => {
                log::info!("Sync woken by trigger");
            },
        }
    }
}

/// Start batch report flusher (every 5 minutes)
pub async fn start_report_flusher() {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let cfg = config::AppConfig::load();
        if cfg.is_paired() {
            let device_id = cfg.device_id.clone().unwrap();
            let device_token = cfg.device_token.clone().unwrap();
            flush_pending_reports(&device_id, &device_token).await;
        }
    }
}

async fn flush_pending_reports(device_id: &str, device_token: &str) {
    let reports = db::get_pending_reports();
    if reports.is_empty() { return; }

    log::info!("Flushing {} pending play reports", reports.len());
    let ids: Vec<i64> = reports.iter().map(|r| r.0).collect();
    let report_data: Vec<serde_json::Value> = reports.iter().map(|r| {
        serde_json::json!({
            "trackId": r.1,
            "zoneId": r.2,
            "startedAt": r.3,
            "durationSecs": r.4,
        })
    }).collect();

    match api::report_plays_batch(device_id, device_token, report_data).await {
        Ok(_) => {
            db::mark_reports_sent(&ids);
            log::info!("Flushed {} play reports successfully", ids.len());
        }
        Err(e) => {
            log::warn!("Failed to flush play reports (will retry): {}", e);
        }
    }
}

fn handle_offline_fallback(handle: &AppHandle, cfg: &config::AppConfig) {
    let zone_id = cfg.zone_id.as_deref().unwrap_or("");

    // Check if we already have a playlist playing
    if let Ok(player) = audio::player().lock() {
        if player.is_playing() && player.playlist_len() > 0 {
            // Already playing, just mark offline
            set_connection_status(ConnectionStatus::Offline, handle);
            return;
        }
    }

    // Try to load cached schedule
    let tz_str = cfg.timezone.as_deref().unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    let now = Utc::now().with_timezone(&tz);
    // API uses luxon: 0=Mon..6=Sun. chrono: num_days_from_monday() gives 0=Mon..6=Sun
    let day_of_week = now.weekday().num_days_from_monday();

    let cached_slots = db::load_schedule(zone_id, day_of_week);
    let current_time = now.format("%H:%M").to_string();

    // Find matching slot
    let mut found_tracks = false;
    for slot in &cached_slots {
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");
        if current_time.as_str() >= start && current_time.as_str() < end {
            if let Some(tracks) = slot.get("playlist").and_then(|p| p.get("tracks")).and_then(|t| t.as_array()) {
                if !tracks.is_empty() {
                    log::info!("Offline: using cached schedule with {} tracks", tracks.len());
                    set_connection_status(ConnectionStatus::Offline, handle);
                    // Load cached tracks into player
                    load_cached_tracks_into_player(tracks, zone_id, &now);
                    found_tracks = true;
                    break;
                }
            }
        }
    }

    if !found_tracks {
        // Emergency mode: shuffle all cached tracks
        enter_emergency_mode(handle);
    }
}

fn load_cached_tracks_into_player(tracks: &[serde_json::Value], zone_id: &str, now: &chrono::DateTime<chrono_tz::Tz>) {
    let cache_dir = config::AppConfig::cache_dir();
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();

    for track_val in tracks {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let file_path = cache_dir.join(format!("{}.mp3", track_id));
        if file_path.exists() {
            playlist.push(audio::TrackInfo {
                track_id: track_id.to_string(),
                title: track_val.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
                artist: track_val.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
                file_path: file_path.to_string_lossy().to_string(),
                duration: track_val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                artwork_url: track_val.get("artworkUrl").and_then(|v| v.as_str()).map(|s| s.to_string()),
            });
        }
    }

    if !playlist.is_empty() {
        // Apply seeded shuffle
        let date_str = now.format("%Y-%m-%d").to_string();
        let seed = format!("{}-{}-00:00", zone_id, date_str);
        playlist = seeded_shuffle(&playlist, &seed);

        if let Ok(mut player) = audio::player().lock() {
            player.set_playlist(playlist);
            let _ = player.play_current();
        }
    }
}

fn enter_emergency_mode(handle: &AppHandle) {
    log::warn!("Entering EMERGENCY mode — shuffling all cached tracks");
    set_connection_status(ConnectionStatus::Emergency, handle);

    let cached = db::get_all_cached_tracks();
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();

    for (id, title, artist, artwork_url, duration, file_path) in &cached {
        if std::path::Path::new(file_path).exists() {
            playlist.push(audio::TrackInfo {
                track_id: id.clone(),
                title: title.clone(),
                artist: artist.clone(),
                file_path: file_path.clone(),
                duration: *duration,
                artwork_url: artwork_url.clone(),
            });
        }
    }

    if playlist.is_empty() {
        log::error!("Emergency mode: NO cached tracks available!");
        return;
    }

    // Random shuffle for emergency
    use rand::seq::SliceRandom;
    let mut rng = rand::thread_rng();
    playlist.shuffle(&mut rng);

    log::info!("Emergency mode: playing {} cached tracks on shuffle", playlist.len());
    if let Ok(mut player) = audio::player().lock() {
        player.set_playlist(playlist);
        let _ = player.play_current();
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

    // We're online!
    set_connection_status(ConnectionStatus::Online, handle);

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

    // Parse crossfade config from zone
    let crossfade_enabled = sync_data.get("zone")
        .and_then(|z| z.get("crossfadeEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let crossfade_duration = sync_data.get("zone")
        .and_then(|z| z.get("crossfadeDuration"))
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as u32;

    config::update_and_save_global(|c| {
        c.timezone = Some(tz_owned.clone());
        c.crossfade_enabled = crossfade_enabled;
        c.crossfade_duration = crossfade_duration;
    });

    // Apply crossfade duration to audio player
    if let Ok(mut player) = audio::player().lock() {
        player.set_crossfade_duration(crossfade_duration as f32);
    }

    // Parse and download spots
    let spots = sync_data.get("spots").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !spots.is_empty() {
        log::info!("Sync contains {} spots", spots.len());
        download_and_save_spots(&spots).await;
    } else {
        // Clear spots if none in sync
        db::save_spot_schedules(&[]);
    }

    // Parse schedule and save to SQLite
    let schedule = sync_data.get("schedule").cloned().unwrap_or(serde_json::json!([]));
    let slots = schedule.as_array().cloned().unwrap_or_default();

    let zone_id = config::AppConfig::load().zone_id.clone().unwrap_or_default();
    db::save_schedule(&zone_id, &slots);

    // API uses luxon: 0=Mon..6=Sun. chrono: num_days_from_monday() gives 0=Mon..6=Sun
    let day_of_week = now.weekday().num_days_from_monday();
    let current_time = now.format("%H:%M").to_string();

    log::info!("Looking for schedule: dayOfWeek={}, time={}", day_of_week, current_time);

    let mut current_tracks: Vec<serde_json::Value> = Vec::new();
    let mut playlist_id: Option<String> = None;
    let mut slot_start_time: Option<String> = None;

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
        let _ = handle.emit("now-playing", serde_json::json!({
            "title": "Sin programación",
            "artist": "No hay música programada en este horario",
            "duration": 0,
            "position": 0,
            "artworkUrl": null,
        }));
        if let Ok(mut player) = audio::player().lock() {
            if player.is_playing() {
                player.stop();
            }
        }
        *current_playlist_id().lock().unwrap() = None;
        return Ok(());
    }

    // Check if playlist changed
    let same_playlist = {
        let cur = current_playlist_id().lock().unwrap();
        cur.as_deref() == playlist_id.as_deref()
    };

    if same_playlist {
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

        let percent = ((i as f64 / total_tracks as f64) * 100.0) as u32;
        let _ = handle.emit("sync-progress", serde_json::json!({
            "phase": "downloading",
            "current": i + 1,
            "total": total_tracks,
            "trackName": title,
            "percent": percent
        }));

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
            let fp_str = file_path.to_string_lossy().to_string();
            // Save track to SQLite
            db::upsert_track(track_id, title, artist, artwork_url.as_deref(), duration, &fp_str);

            playlist.push(audio::TrackInfo {
                track_id: track_id.to_string(),
                title: title.to_string(),
                artist: artist.to_string(),
                file_path: fp_str,
                duration,
                artwork_url,
            });
        }
    }

    if !playlist.is_empty() {
        log::info!("All downloads complete, updating playlist with {} tracks", playlist.len());

        let start_t = slot_start_time.as_deref().unwrap_or("00:00");
        let parts: Vec<&str> = start_t.split(':').collect();
        let start_h: u32 = parts.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
        let start_m: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let slot_start_secs = (start_h * 3600 + start_m * 60) as f64;
        let now_secs = (now.hour() * 3600 + now.minute() * 60 + now.second()) as f64;
        let elapsed_secs = if now_secs >= slot_start_secs { now_secs - slot_start_secs } else { 0.0 };

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

async fn download_and_save_spots(spots: &[serde_json::Value]) {
    let cache_dir = config::AppConfig::data_dir().join("cache").join("spots");
    let _ = std::fs::create_dir_all(&cache_dir);

    let mut spots_with_paths: Vec<serde_json::Value> = Vec::new();

    for spot in spots {
        let id = spot.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let audio_url = spot.get("audioUrl").and_then(|v| v.as_str()).unwrap_or("");
        let name = spot.get("name").and_then(|v| v.as_str()).unwrap_or("?");

        let file_path = cache_dir.join(format!("{}.mp3", id));

        if !file_path.exists() && !audio_url.is_empty() {
            log::info!("Downloading spot: {} - {}", id, name);
            match api::download_track(audio_url, &file_path).await {
                Ok(_) => log::info!("Downloaded spot: {}", name),
                Err(e) => {
                    log::error!("Failed to download spot {}: {}", name, e);
                    // Still save without file_path
                    spots_with_paths.push(spot.clone());
                    continue;
                }
            }
        }

        let mut s = spot.clone();
        if file_path.exists() {
            if let Some(obj) = s.as_object_mut() {
                obj.insert("_filePath".to_string(), serde_json::json!(file_path.to_string_lossy().to_string()));
            }
        }
        spots_with_paths.push(s);
    }

    db::save_spot_schedules(&spots_with_paths);
}

/// Check if a spot should play based on schedule rules
fn find_eligible_spot(tz: &chrono_tz::Tz) -> Option<String> {
    let now = Utc::now().with_timezone(tz);
    let day_of_week = now.weekday().num_days_from_monday();
    let current_time = now.format("%H:%M").to_string();
    let current_date = now.format("%Y-%m-%d").to_string();

    let spots = db::load_spot_schedules();

    for (id, days_json, start_time, end_time, track_freq, _freq, start_date, end_date, file_path) in &spots {
        // Check file exists
        if !std::path::Path::new(file_path).exists() {
            continue;
        }

        // Check track frequency
        if let Ok(player) = audio::player().lock() {
            if player.tracks_since_last_spot < *track_freq as usize {
                continue;
            }
        }

        // Check day of week
        let days: Vec<u32> = serde_json::from_str(days_json).unwrap_or_default();
        if !days.is_empty() && !days.contains(&day_of_week) {
            continue;
        }

        // Check time range
        if current_time.as_str() < start_time.as_str() || current_time.as_str() >= end_time.as_str() {
            continue;
        }

        // Check date range
        if let Some(sd) = start_date {
            if !sd.is_empty() && current_date.as_str() < sd.as_str() {
                continue;
            }
        }
        if let Some(ed) = end_date {
            if !ed.is_empty() && current_date.as_str() > ed.as_str() {
                continue;
            }
        }

        log::info!("Spot eligible: {} (file: {})", id, file_path);
        return Some(file_path.clone());
    }
    None
}

/// Called from main.rs every second to check if track ended and advance
pub fn check_track_advancement(handle: &AppHandle) {
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(poisoned) => {
            log::error!("Audio mutex poisoned! Recovering...");
            poisoned.into_inner()
        },
    };

    if !player.is_playing() {
        return;
    }

    // Update crossfade volumes if active
    player.update_crossfade();

    // Check crossfade trigger: if near end of track and crossfade enabled
    let cfg = config::AppConfig::load();
    if cfg.crossfade_enabled && !player.crossfade_active && !player.playing_spot {
        // Clone track info to avoid borrow conflicts
        let track_info = player.current_track().cloned();
        let next_info = player.peek_next().cloned();
        if let (Some(track), Some(next)) = (track_info, next_info) {
            let position = player.get_position();
            let crossfade_point = track.duration - cfg.crossfade_duration as f32;
            if crossfade_point > 0.0 && position >= crossfade_point && !player.is_finished() {
                log::info!("Starting crossfade at {:.1}s (track duration {:.1}s)", position, track.duration);
                if let Err(e) = player.start_crossfade(&next) {
                    log::error!("Crossfade failed: {}", e);
                } else {
                    // Record play report for outgoing track
                    let zone_id = cfg.zone_id.as_deref().unwrap_or("");
                    let started_at = Utc::now().to_rfc3339();
                    db::save_play_report(&track.track_id, zone_id, &started_at, position as f64);
                    db::touch_track(&track.track_id);
                    // Advance index
                    player.advance();
                    player.tracks_since_last_spot += 1;
                    player.reset_position();
                    // Emit now-playing for new track
                    if let Some(t) = player.current_track() {
                        let _ = handle.emit("now-playing", serde_json::json!({
                            "title": t.title,
                            "artist": t.artist,
                            "duration": t.duration,
                            "position": 0.0,
                            "artworkUrl": t.artwork_url,
                        }));
                    }
                    return;
                }
            }
        }
    }

    if !player.is_finished() {
        if player.get_position() > 5.0 {
            player.consecutive_skips = 0;
        }
        // Log position every 30s, and every second in the last 10s of track
        let pos = player.get_position();
        let dur = player.current_track().map(|t| t.duration).unwrap_or(0.0);
        let near_end = dur > 1.0 && pos >= dur - 10.0;
        if pos > 0.0 && ((pos as u32) % 30 == 0 || near_end) {
            log::info!("Playing {:.0}s / {:.0}s (finished={})", pos, dur, player.is_finished());
        }
        return;
    }

    let position = player.get_position();

    // If a spot just finished, resume normal playback
    if player.playing_spot {
        log::info!("Spot finished, resuming normal playback");
        player.playing_spot = false;
        player.tracks_since_last_spot = 0;
        if let Err(e) = player.play_current() {
            log::error!("Error resuming after spot: {}", e);
        }
        if let Some(track) = player.current_track() {
            let _ = handle.emit("now-playing", serde_json::json!({
                "title": track.title,
                "artist": track.artist,
                "duration": track.duration,
                "position": 0.0,
                "artworkUrl": track.artwork_url,
            }));
        }
        return;
    }

    // Record play report for finished track
    if let Some(track) = player.current_track() {
        if position > 3.0 {
            let zone_id = cfg.zone_id.as_deref().unwrap_or("");
            let started_at = Utc::now().to_rfc3339();
            db::save_play_report(&track.track_id, zone_id, &started_at, position as f64);
            db::touch_track(&track.track_id);
        }

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

    // Increment track counter
    player.tracks_since_last_spot += 1;

    // Check if a spot should play before next track
    let tz_str = cfg.timezone.as_deref().unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    if let Some(spot_path) = find_eligible_spot(&tz) {
        log::info!("Playing spot before next track (tracks since last: {})", player.tracks_since_last_spot);
        match player.play_spot_file(&spot_path) {
            Ok(_) => {
                let _ = handle.emit("now-playing", serde_json::json!({
                    "title": "📢 Spot",
                    "artist": "Anuncio",
                    "duration": 0,
                    "position": 0.0,
                    "artworkUrl": null,
                }));
                return;
            }
            Err(e) => {
                log::error!("Failed to play spot: {}", e);
                // Fall through to normal track advancement
            }
        }
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
