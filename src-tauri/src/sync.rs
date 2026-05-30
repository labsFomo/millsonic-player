use crate::{api, audio, config, db};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use chrono::{Datelike, Timelike, Utc};
use chrono_tz;
use tokio::sync::Notify;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;

// ── SonicBox anti-repeat (R-12) ─────────────────────────────────────────────
// A voted track must NEVER loop back-to-back. It may play again, but only after
// at least SONICBOX_MIN_GAP_TRACKS other tracks have played. This guard is the
// HARD guarantee: it holds even fully offline and even if the play-report (and
// therefore the server-side markPlayed) never succeeds and the API keeps
// returning the same vote/trackId.
const SONICBOX_MIN_GAP_TRACKS: u64 = 4;

// Monotonic count of tracks that have finished playing (grid + spot + sonicbox).
static SB_TRACKS_PLAYED: AtomicU64 = AtomicU64::new(0);
// trackId -> value of SB_TRACKS_PLAYED at the moment it last played via SonicBox.
static SB_LAST_PLAYED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn sb_last_played() -> &'static Mutex<HashMap<String, u64>> {
    SB_LAST_PLAYED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Increment the global "tracks played" counter (call once per finished track).
fn sb_bump_tracks_played() -> u64 {
    SB_TRACKS_PLAYED.fetch_add(1, Ordering::Relaxed) + 1
}

/// Remember that `track_id` just played via SonicBox (at the current count).
fn sb_note_played(track_id: &str) {
    let now = SB_TRACKS_PLAYED.load(Ordering::Relaxed);
    let mut map = match sb_last_played().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    map.insert(track_id.to_string(), now);
    // Prune entries well past the gap window to keep the map tiny.
    map.retain(|_, &mut last| now.saturating_sub(last) <= SONICBOX_MIN_GAP_TRACKS * 4);
}

/// Pure decision: should we skip staging this track because it played too
/// recently via SonicBox? `last` is the recorded count, `now` the current.
fn sb_should_skip_repeat(last: Option<u64>, now: u64, gap: u64) -> bool {
    match last {
        Some(last) => now.saturating_sub(last) < gap,
        None => false,
    }
}

/// Has `track_id` played via SonicBox within the last SONICBOX_MIN_GAP_TRACKS?
fn sb_recently_played(track_id: &str) -> bool {
    let now = SB_TRACKS_PLAYED.load(Ordering::Relaxed);
    let map = match sb_last_played().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    sb_should_skip_repeat(map.get(track_id).copied(), now, SONICBOX_MIN_GAP_TRACKS)
}

// ── Spot anti-repeat (R-06) ──────────────────────────────────────────────────
// Remember the last spot we played so we don't fire the same announcement twice
// in a row when several are eligible. People hate hearing the same ad on loop.
static LAST_SPOT_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn last_spot_id() -> &'static Mutex<Option<String>> {
    LAST_SPOT_ID.get_or_init(|| Mutex::new(None))
}

/// Pick a spot id from the eligible list that isn't the one we just played.
/// Pure + testable: rotates past `last`; if the only eligible spot IS the last
/// one, plays it anyway (can't leave dead air / skip a due ad).
fn pick_spot<'a>(eligible: &'a [(String, String)], last: Option<&str>) -> Option<&'a (String, String)> {
    if eligible.is_empty() {
        return None;
    }
    eligible
        .iter()
        .find(|(id, _)| Some(id.as_str()) != last)
        .or_else(|| eligible.first())
}

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

pub fn set_connection_status(status: ConnectionStatus, handle: &AppHandle) {
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

/// R-09: called right before an update restart so the upcoming hard restart
/// doesn't lose queued play reports or cut audio mid-buffer. Flushes pending
/// reports and stops playback cleanly.
pub async fn flush_reports_before_exit() {
    let cfg = config::AppConfig::load();
    if let (Some(id), Some(token)) = (cfg.device_id.clone(), cfg.device_token.clone()) {
        flush_pending_reports(&id, &token).await;
    }
    if let Ok(mut p) = audio::player().lock() {
        p.stop();
    }
}

/// R-04: remove any leftover partial download files (`*.part`) from the cache
/// on startup, so a download interrupted by a crash/power-loss can't linger.
pub fn sweep_partial_downloads() {
    let dir = config::AppConfig::cache_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("part") {
                let _ = std::fs::remove_file(&p);
                log::info!("Swept partial download: {}", p.display());
            }
        }
    }
}

/// Track the current playlist ID to avoid restarting playback on re-sync
static CURRENT_PLAYLIST_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn current_playlist_id() -> &'static Mutex<Option<String>> {
    CURRENT_PLAYLIST_ID.get_or_init(|| Mutex::new(None))
}

// U6: fingerprint of the currently-loaded track list (ordered track ids). Lets
// us detect when the SONGS of a playlist changed even though the playlist_id is
// the same (admin edited its contents), so we reload instead of ignoring it.
static CURRENT_TRACKS_FP: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn current_tracks_fp() -> &'static Mutex<Option<String>> {
    CURRENT_TRACKS_FP.get_or_init(|| Mutex::new(None))
}

fn tracks_fingerprint(tracks: &[serde_json::Value]) -> String {
    let ids: Vec<&str> = tracks
        .iter()
        .map(|t| t.get("id").and_then(|v| v.as_str()).unwrap_or(""))
        .collect();
    ids.join(",")
}

pub async fn start_sync_loop(handle: AppHandle) {
    log::info!("Sync loop started");

    // R-04: clean any partial downloads left by a previous crash/power-loss.
    sweep_partial_downloads();

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
        // R-01: nothing cached at all (e.g. paired but never synced, then went
        // offline). We can't conjure audio, but we must NOT give up silently:
        // keep nudging a sync so the instant any content arrives, music starts.
        log::error!("Emergency mode: NO cached tracks available — forcing sync to recover ASAP");
        trigger_sync();
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

    // Extract zone name and location name from sync response
    let zone_name = sync_data.get("zone")
        .and_then(|z| z.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let location_name = sync_data.get("zone")
        .and_then(|z| z.get("location"))
        .and_then(|l| l.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| sync_data.get("locationName").and_then(|v| v.as_str()).map(|s| s.to_string()));

    if zone_name.is_some() || location_name.is_some() {
        config::update_and_save_global(|c| {
            if let Some(ref zn) = zone_name { c.zone_name = Some(zn.clone()); }
            if let Some(ref ln) = location_name { c.location_name = Some(ln.clone()); }
        });
        // Emit event so frontend updates immediately
        let _ = handle.emit("zone-info", serde_json::json!({
            "zoneName": zone_name,
            "locationName": location_name,
        }));
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
        // Also clean spot cache files
        let spots_cache = config::AppConfig::data_dir().join("cache").join("spots");
        if let Ok(entries) = std::fs::read_dir(&spots_cache) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        log::info!("Cleared spot cache (no spots in sync)");
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

    // Check if the playlist changed. U6: "same" requires BOTH the same
    // playlist_id AND the same track list — if the songs were edited (added/
    // removed/reordered) under the same id, we must reload, not just re-cache.
    let new_fp = tracks_fingerprint(&current_tracks);
    let same_playlist = {
        let cur_id = current_playlist_id().lock().unwrap();
        let cur_fp = current_tracks_fp().lock().unwrap();
        cur_id.as_deref() == playlist_id.as_deref()
            && cur_fp.as_deref() == Some(new_fp.as_str())
    };

    if same_playlist {
        log::info!("Same playlist + same tracks, refreshing cache only");
        refresh_track_cache(&current_tracks).await;
        return Ok(());
    }
    if current_playlist_id().lock().unwrap().as_deref() == playlist_id.as_deref() {
        log::info!("Same playlist id but TRACK LIST CHANGED — reloading playlist (U6)");
    }

    // Different playlist (first sync / new slot / U6 content change) — (re)build
    // the playlist and start playing. U1: download only the start track first,
    // play it, then download the rest in the background.
    let cache_dir = config::AppConfig::cache_dir();
    std::fs::create_dir_all(&cache_dir)?;

    let total_tracks = current_tracks.len();
    let mut first_track_started = false;

    // Build the full playlist metadata up front. file_path is the predetermined
    // cache location even if the file isn't downloaded yet (progressive).
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();
    let mut dl_info: Vec<(String, String, std::path::PathBuf)> = Vec::new(); // (id, stream_url, path)
    for track_val in current_tracks.iter() {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let title = track_val.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
        let artist = track_val.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
        let duration = track_val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let artwork_url = track_val.get("artworkUrl").and_then(|v| v.as_str()).map(|s| s.to_string());
        let stream_url = track_val.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let file_path = cache_dir.join(format!("{}.mp3", track_id));
        db::upsert_track(&track_id, &title, &artist, artwork_url.as_deref(), duration, &file_path.to_string_lossy());
        dl_info.push((track_id.clone(), stream_url, file_path.clone()));
        playlist.push(audio::TrackInfo {
            track_id,
            title,
            artist,
            file_path: file_path.to_string_lossy().to_string(),
            duration,
            artwork_url,
        });
    }
    if playlist.is_empty() {
        return Ok(());
    }

    // ── U5: decide WHERE to start — which track AND which second. Authoritative
    // source = server now-playing (currentTrack.id + seekPosition), exactly like
    // the web player. Offline / on failure, fall back to a local time-based calc.
    let mut start_index = 0usize;
    let mut seek_secs = 0.0f32;
    let mut from_server = false;
    if let Ok(np) = api::fetch_zone_now_playing(&zone_id).await {
        if let Some(ct) = np.get("currentTrack") {
            let is_spot = ct.get("isSpot").and_then(|v| v.as_bool()).unwrap_or(false);
            let cur_id = ct.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if !is_spot && !cur_id.is_empty() {
                if let Some(idx) = playlist.iter().position(|t| t.track_id == cur_id) {
                    start_index = idx;
                    seek_secs = ct.get("seekPosition").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    from_server = true;
                    log::info!("Start from server now-playing: track {} of {}, seek {:.0}s (U5)",
                        idx, playlist.len(), seek_secs);
                }
            }
        }
    }
    if !from_server {
        let start_t = slot_start_time.as_deref().unwrap_or("00:00");
        let parts: Vec<&str> = start_t.split(':').collect();
        let start_h: u32 = parts.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
        let start_m: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let slot_start_secs = (start_h * 3600 + start_m * 60) as f64;
        let now_secs = (now.hour() * 3600 + now.minute() * 60 + now.second()) as f64;
        let elapsed_secs = if now_secs >= slot_start_secs { now_secs - slot_start_secs } else { 0.0 };
        let total_duration: f64 = playlist.iter().map(|t| t.duration as f64).sum();
        if total_duration > 0.0 {
            let looped = elapsed_secs % total_duration;
            let mut acc = 0.0f64;
            for (i, t) in playlist.iter().enumerate() {
                let next = acc + t.duration as f64;
                if next > looped {
                    start_index = i;
                    seek_secs = (looped - acc) as f32;
                    break;
                }
                acc = next;
            }
        }
        log::info!("Start from local fallback: track {} of {}, seek {:.0}s", start_index, playlist.len(), seek_secs);
    }

    // ── U1: download ONLY the start track, begin playing, then background the
    // rest. If the start track can't be cached, fall back to the first cached
    // one so we NEVER block into silence.
    {
        let start = &dl_info[start_index];
        if !start.2.exists() && !start.1.is_empty() {
            log::info!("Downloading start track first (progressive): {}", playlist[start_index].title);
            if let Err(e) = api::download_track(&start.1, &start.2).await {
                log::error!("Start track download failed: {}", e);
            }
        }
    }
    if !dl_info[start_index].2.exists() {
        if let Some(idx) = dl_info.iter().position(|d| d.2.exists()) {
            log::warn!("Start track not cached — falling back to first cached track");
            start_index = idx;
            seek_secs = 0.0;
        }
    }

    {
        let mut player = audio::player().lock().map_err(|e| e.to_string())?;
        player.set_playlist(playlist);
        player.current_index = start_index;
        if let Err(e) = player.play_current_at(seek_secs) {
            log::error!("Failed to start playback: {}", e);
        } else {
            first_track_started = true;
        }
    }
    *current_playlist_id().lock().unwrap() = playlist_id;
    *current_tracks_fp().lock().unwrap() = Some(new_fp.clone());
    let _ = handle.emit("status-change", serde_json::json!({"playing": true}));

    // Background-download the remaining tracks in playhead order (next, then the
    // one after, wrapping around) so each is ready before it's needed (U1).
    {
        let n = dl_info.len();
        let order: Vec<(String, String, std::path::PathBuf)> =
            (1..n).map(|k| dl_info[(start_index + k) % n].clone()).collect();
        tauri::async_runtime::spawn(async move {
            for (id, url, path) in order {
                if path.exists() || url.is_empty() {
                    continue;
                }
                if let Err(e) = api::download_track(&url, &path).await {
                    log::error!("Background download failed for {}: {}", id, e);
                }
            }
            log::info!("Background download of remaining tracks complete");
        });
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
fn find_eligible_spot(tz: &chrono_tz::Tz, tracks_since_last_spot: usize) -> Option<String> {
    let now = Utc::now().with_timezone(tz);
    let day_of_week = now.weekday().num_days_from_monday();
    let current_time = now.format("%H:%M").to_string();
    let current_date = now.format("%Y-%m-%d").to_string();

    let spots = db::load_spot_schedules();

    // R-06: collect ALL eligible spots, then rotate past the last one played so
    // the same announcement doesn't repeat back-to-back.
    let mut eligible: Vec<(String, String)> = Vec::new();
    for (id, days_json, start_time, end_time, track_freq, _freq, start_date, end_date, file_path) in &spots {
        // Check file exists
        if !std::path::Path::new(file_path).exists() {
            continue;
        }

        // Check track frequency (passed in to avoid re-locking audio mutex)
        if tracks_since_last_spot < *track_freq as usize {
            continue;
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

        eligible.push((id.clone(), file_path.clone()));
    }

    let last = last_spot_id().lock().ok().and_then(|g| g.clone());
    let pick = pick_spot(&eligible, last.as_deref())?;
    log::info!("Spot eligible: {} (file: {})", pick.0, pick.1);
    if let Ok(mut g) = last_spot_id().lock() {
        *g = Some(pick.0.clone());
    }
    Some(pick.1.clone())
}

/// Called from main.rs every second to check if track ended and advance
/// SonicBox watcher — two tiers, mirroring how votes work in real life:
///   Nivel 1 (background, ~every 11s): discover the current winning vote early
///     and PRE-DOWNLOAD its audio so it's cached and ready in time.
///   Nivel 2 (late authoritative, last ~8s of the current track): one fresh
///     fetch so a last-moment vote change still gets in. Anything that changes
///     in the final couple of seconds is intentionally ignored.
/// The winner/queue ordering lives server-side (peekNext orders by credits);
/// here we just ask "what's next?" and stage it as the forced next track.
pub async fn start_sonicbox_loop(_handle: AppHandle) {
    use std::time::Instant;
    const TICK: Duration = Duration::from_secs(3);
    const BG_INTERVAL_SECS: f32 = 11.0;
    const LATE_WINDOW_SECS: f32 = 8.0;

    let mut last_full_fetch = Instant::now() - Duration::from_secs(60);
    let mut late_fetched_for: Option<String> = None;

    loop {
        tokio::time::sleep(TICK).await;

        let cfg = config::AppConfig::load();
        let zone_id = match cfg.zone_id.clone() {
            Some(z) if !z.is_empty() => z,
            _ => continue,
        };

        // Snapshot player state under a short lock.
        let (cur_id, remaining, busy) = {
            let p = match audio::player().lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            let cur_id = p.current_track().map(|t| t.track_id.clone()).unwrap_or_default();
            let dur = p.current_track().map(|t| t.duration).unwrap_or(0.0);
            let pos = p.get_position();
            let remaining = if dur > 0.0 { dur - pos } else { f32::INFINITY };
            // Don't stage a forced track while a spot or another voted track is
            // already mid-interrupt.
            let busy = p.playing_spot || p.playing_sonicbox || !p.is_playing();
            (cur_id, remaining, busy)
        };

        let in_late_window = remaining <= LATE_WINDOW_SECS && !busy;
        let due_background = last_full_fetch.elapsed().as_secs_f32() >= BG_INTERVAL_SECS;
        let due_late = in_late_window && late_fetched_for.as_deref() != Some(cur_id.as_str());

        if !(due_background || due_late) {
            continue;
        }
        if due_late {
            late_fetched_for = Some(cur_id.clone());
            log::info!("SonicBox: late authoritative re-check ({:.1}s left on current track)", remaining);
        }
        last_full_fetch = Instant::now();

        let json = match api::fetch_zone_now_playing(&zone_id).await {
            Ok(j) => j,
            Err(e) => {
                log::warn!("SonicBox: now-playing fetch failed: {}", e);
                continue;
            }
        };

        // Read the staged vote. The backend puts the voted entry in
        // nextTracks[0] (with isSonicBox:true) and a summary in sonicboxNext.
        let sb_entry = json
            .get("nextTracks")
            .and_then(|n| n.get(0))
            .filter(|e| e.get("isSonicBox").and_then(|v| v.as_bool()).unwrap_or(false));

        let sb_entry = match sb_entry {
            Some(e) => e,
            None => {
                // No active winning vote — drop any previously staged track.
                let mut p = match audio::player().lock() {
                    Ok(p) => p,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if p.has_sonicbox_next() {
                    log::info!("SonicBox: no active vote, clearing staged track");
                    p.clear_sonicbox_next();
                }
                continue;
            }
        };

        let track_id = sb_entry.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let vote_id = sb_entry
            .get("voteId")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("sonicboxNext").and_then(|s| s.get("voteId")).and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();
        if track_id.is_empty() {
            continue;
        }

        // R-12 (anti-repeat): if this track already played via SonicBox within
        // the last SONICBOX_MIN_GAP_TRACKS, do NOT re-stage it — even if the API
        // keeps returning the same vote because markPlayed hasn't landed yet.
        // This is what stops a voted track from looping back-to-back.
        if sb_recently_played(&track_id) {
            log::info!(
                "SonicBox: skipping repeat of track {} (played <{} tracks ago)",
                track_id, SONICBOX_MIN_GAP_TRACKS
            );
            let mut p = match audio::player().lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            if p.has_sonicbox_next() {
                p.clear_sonicbox_next();
            }
            continue;
        }

        let title = sb_entry.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let artist = sb_entry.get("artist").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let duration = sb_entry.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let artwork_url = sb_entry.get("coverUrl").and_then(|v| v.as_str()).map(|s| s.to_string());
        let stream_url = sb_entry.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Ensure the audio file is cached (pre-download). Same cache layout as
        // the playlist: cache_dir/<trackId>.mp3.
        let cache_dir = config::AppConfig::cache_dir();
        let _ = std::fs::create_dir_all(&cache_dir);
        let file_path = cache_dir.join(format!("{}.mp3", track_id));
        if !file_path.exists() {
            if stream_url.is_empty() {
                log::warn!("SonicBox: voted track {} not cached and no streamUrl", track_id);
                continue;
            }
            log::info!("SonicBox: pre-downloading voted track '{}'", title);
            if let Err(e) = api::download_track(&stream_url, &file_path).await {
                log::error!("SonicBox: failed to pre-download voted track: {}", e);
                continue;
            }
        }

        let track = audio::TrackInfo {
            track_id: track_id.clone(),
            title,
            artist,
            file_path: file_path.to_string_lossy().to_string(),
            duration,
            artwork_url,
        };

        let mut p = match audio::player().lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };
        p.set_sonicbox_next(track, vote_id.clone());
        log::info!("SonicBox: staged voted track {} (vote={}) as forced next", track_id, vote_id);
    }
}

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
    if cfg.crossfade_enabled && !player.crossfade_active && !player.playing_spot && !player.playing_sonicbox {
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
    let track_title = player.current_track().map(|t| t.title.clone()).unwrap_or_default();
    let track_dur = player.current_track().map(|t| t.duration).unwrap_or(0.0);
    log::info!("Track '{}' finished at {:.1}s (duration={:.1}s, playing_spot={})",
        track_title, position, track_dur, player.playing_spot);

    // R-12: count every finished track so the SonicBox anti-repeat gap can be
    // measured in "tracks since". Covers grid, spot and sonicbox tracks.
    sb_bump_tracks_played();

    // If a spot just finished, resume normal playback
    if player.playing_spot {
        log::info!("Spot finished, resuming normal playback");
        player.playing_spot = false;
        player.tracks_since_last_spot = 0;
        player.advance(); // Move to NEXT track (previous track already finished before spot)
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

    // SonicBox: a voted track just finished. Close the vote loop by reporting
    // the completion to the REAL player endpoint (/player/play-report), then
    // resume the normal grid from the track that would have played next.
    if player.playing_sonicbox {
        let sb_pos = player.get_position();
        if let Some((sb_track, vote_id)) = player.clear_playing_sonicbox() {
            // completed = played most of it (not skipped early). Backend only
            // closes the loop when completed && !skipped.
            let completed = sb_track.duration > 1.0 && sb_pos >= sb_track.duration * 0.8;
            log::info!(
                "SonicBox track '{}' finished at {:.1}s/{:.1}s (vote={}, completed={})",
                sb_track.title, sb_pos, sb_track.duration, vote_id, completed
            );
            // R-12 (hard guarantee): remember this track played via SonicBox so
            // the vigía won't re-stage it for the next SONICBOX_MIN_GAP_TRACKS,
            // even if the play-report below never reaches the server.
            sb_note_played(&sb_track.track_id);
            let cfg_sb = config::AppConfig::load();
            if let Some(token) = cfg_sb.device_token.clone() {
                let track_id = sb_track.track_id.clone();
                let started_at =
                    (Utc::now() - chrono::Duration::seconds(sb_pos as i64)).to_rfc3339();
                let dur = sb_pos.max(0.0) as i64;
                tauri::async_runtime::spawn(async move {
                    let play = serde_json::json!({
                        "trackId": track_id,
                        "startedAt": started_at,
                        "duration": dur,
                        "completed": completed,
                        "skipped": !completed,
                    });
                    // R-12 (soft cleanup): retry so markPlayed closes the vote
                    // server-side. The local guard already prevents a loop if
                    // every attempt fails.
                    let mut sent = false;
                    for attempt in 1..=3u32 {
                        match api::report_player_play(&token, vec![play.clone()]).await {
                            Ok(_) => {
                                log::info!("SonicBox: play-report sent (completed={}, attempt={})", completed, attempt);
                                sent = true;
                                break;
                            }
                            Err(e) => {
                                log::warn!("SonicBox: play-report attempt {}/3 failed: {}", attempt, e);
                                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                            }
                        }
                    }
                    if !sent {
                        log::error!("SonicBox: play-report failed after 3 attempts — local anti-repeat guard still active");
                    }
                });
            } else {
                log::warn!("SonicBox: no device_token, cannot close vote loop");
            }
        }
        // Resume the grid: advance to the track that follows the one the vote
        // interrupted, and play it.
        player.advance();
        if let Err(e) = player.play_current() {
            log::error!("Error resuming grid after SonicBox track: {}", e);
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

    // Collect data for DB calls, then drop audio lock to avoid deadlock
    let report_data = player.current_track().map(|track| {
        let track_id = track.track_id.clone();
        let file_path = track.file_path.clone();
        let track_title = track.title.clone();
        let track_dur = track.duration;
        (track_id, file_path, track_title, track_dur)
    });

    if let Some((ref track_id, ref file_path, ref track_title, track_dur)) = report_data {
        if position < 3.0 && track_dur > 5.0 {
            log::error!("Track '{}' finished after only {:.1}s (expected {:.0}s) - file may be corrupt: {}",
                track_title, position, track_dur, file_path);
            player.consecutive_skips += 1;
        } else {
            player.consecutive_skips = 0;
        }
    }

    // Drop audio lock BEFORE DB operations to prevent deadlock
    let consecutive_skips = player.consecutive_skips;
    let playlist_len = player.playlist_len();
    drop(player);

    // Now safe to call DB without holding audio lock
    if let Some((track_id, _, _, _)) = &report_data {
        if position > 3.0 {
            let zone_id = cfg.zone_id.as_deref().unwrap_or("");
            let started_at = Utc::now().to_rfc3339();
            db::save_play_report(track_id, zone_id, &started_at, position as f64);
            db::touch_track(track_id);
        }
    }

    // Re-acquire audio lock for the rest of advancement
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(poisoned) => poisoned.into_inner(),
    };

    if consecutive_skips >= playlist_len && playlist_len > 0 {
        // R-02: NEVER hard-stop into dead air. Every track in the current grid
        // failed (corrupt/missing cache) — recover instead: force a re-sync to
        // re-download, and meanwhile fall back to an emergency shuffle of ALL
        // cached tracks (other slots/playlists may have working files). Reset
        // the skip counter so playback keeps trying rather than freezing.
        log::error!(
            "All {} grid tracks failed — recovering (re-sync + emergency shuffle), NOT stopping",
            playlist_len
        );
        player.consecutive_skips = 0;
        drop(player);
        trigger_sync();
        enter_emergency_mode(handle);
        return;
    }

    // ── SonicBox: a winning vote takes the slot right after the current track
    // ends, ahead of any spot. We play it as an interrupt (like a spot): the
    // playlist index is NOT advanced here — it advances when the voted track
    // finishes, so the grid resumes exactly where it left off.
    if player.has_sonicbox_next() {
        if let Some((sb_track, vote_id)) = player.take_sonicbox_next() {
            match player.play_file(&sb_track) {
                Ok(_) => {
                    log::info!(
                        "SonicBox: playing voted track '{}' by '{}' (vote={})",
                        sb_track.title, sb_track.artist, vote_id
                    );
                    player.set_playing_sonicbox(sb_track.clone(), vote_id);
                    // R-12: mark as played the moment it STARTS, so a background
                    // poll mid-playback can't re-stage the same track and cause
                    // a back-to-back loop.
                    sb_note_played(&sb_track.track_id);
                    let _ = handle.emit("now-playing", serde_json::json!({
                        "title": sb_track.title,
                        "artist": sb_track.artist,
                        "duration": sb_track.duration,
                        "position": 0.0,
                        "artworkUrl": sb_track.artwork_url,
                        "isSonicBox": true,
                    }));
                    return;
                }
                Err(e) => {
                    log::error!(
                        "SonicBox: failed to play voted track '{}': {} — falling back to grid",
                        sb_track.title, e
                    );
                    // fall through to normal spot/advance below
                }
            }
        }
    }

    // Increment track counter
    player.tracks_since_last_spot += 1;

    // Check if a spot should play before next track
    let tz_str = cfg.timezone.as_deref().unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    if let Some(spot_path) = find_eligible_spot(&tz, player.tracks_since_last_spot) {
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

#[cfg(test)]
mod sonicbox_repeat_tests {
    use super::*;

    #[test]
    fn never_skips_a_track_not_seen_before() {
        assert!(!sb_should_skip_repeat(None, 100, SONICBOX_MIN_GAP_TRACKS));
    }

    #[test]
    fn skips_within_the_gap_window() {
        // played at 10, only 1 track later → must skip (would be a near-loop)
        assert!(sb_should_skip_repeat(Some(10), 11, 4));
        // played at 10, 3 tracks later → still inside gap of 4 → skip
        assert!(sb_should_skip_repeat(Some(10), 13, 4));
    }

    #[test]
    fn allows_again_once_the_gap_has_passed() {
        // exactly gap tracks later → eligible again (Pablo: "después de 3-4 vuelve")
        assert!(!sb_should_skip_repeat(Some(10), 14, 4));
        assert!(!sb_should_skip_repeat(Some(10), 20, 4));
    }

    #[test]
    fn spot_rotation_avoids_repeating_the_last() {
        let eligible = vec![
            ("spot-a".to_string(), "/a.mp3".to_string()),
            ("spot-b".to_string(), "/b.mp3".to_string()),
        ];
        // last was A → must pick B
        assert_eq!(pick_spot(&eligible, Some("spot-a")).unwrap().0, "spot-b");
        // last was B → must pick A
        assert_eq!(pick_spot(&eligible, Some("spot-b")).unwrap().0, "spot-a");
        // none played yet → first
        assert_eq!(pick_spot(&eligible, None).unwrap().0, "spot-a");
    }

    #[test]
    fn spot_plays_anyway_if_its_the_only_eligible() {
        let only = vec![("spot-a".to_string(), "/a.mp3".to_string())];
        // even though A was the last, it's the only due ad → play it (no dead air)
        assert_eq!(pick_spot(&only, Some("spot-a")).unwrap().0, "spot-a");
        // empty → nothing
        assert!(pick_spot(&[], Some("x")).is_none());
    }

    #[test]
    fn global_guard_blocks_then_releases() {
        // Self-contained against the shared global: use a unique id and relative
        // reasoning so the starting counter value doesn't matter.
        let id = "unit-test-track-xyz";
        sb_note_played(id);
        assert!(sb_recently_played(id), "just played → must be blocked");
        for _ in 0..SONICBOX_MIN_GAP_TRACKS {
            sb_bump_tracks_played();
        }
        assert!(!sb_recently_played(id), "after the gap → eligible again");
    }
}
