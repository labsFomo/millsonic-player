#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod db;
mod sync;
mod config;
mod telemetry;
mod api;
mod ws;

use tauri::{Manager, Emitter};
use std::time::Duration;
use simplelog::{CombinedLogger, WriteLogger, LevelFilter, Config as LogConfig};
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicU64, Ordering};

#[tauri::command]
fn get_status() -> serde_json::Value {
    let cfg = config::AppConfig::load();
    let player = audio::player().try_lock().ok();
    let is_playing = player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
    let volume = player.as_ref().map(|p| p.get_volume()).unwrap_or(80);
    let track = player.as_ref().and_then(|p| p.current_track().map(|t| t.title.clone()));
    let artist = player.as_ref().and_then(|p| p.current_track().map(|t| t.artist.clone()));
    let conn_status = match sync::get_connection_status() {
        sync::ConnectionStatus::Online => "online",
        sync::ConnectionStatus::Offline => "offline",
        sync::ConnectionStatus::Emergency => "emergency",
    };

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "paired": cfg.is_paired(),
        "playing": is_playing,
        "volume": volume,
        "track": track,
        "artist": artist,
        "zoneName": cfg.zone_name,
        "connectionStatus": conn_status,
    })
}

#[tauri::command]
async fn pair_device(code: String) -> Result<serde_json::Value, String> {
    log::info!("pair_device called with code: {}", code);
    log::info!("pair_device called with code: {}", code);
    let resp = api::pair_with_code(&code).await.map_err(|e| {
        log::error!("pair_device API error: {}", e);
        e.to_string()
    })?;

    // Check for error in response
    if resp.get("statusCode").is_some() {
        return Ok(resp); // API error, return as-is for frontend to handle
    }

    // Save pairing info to config
    // API returns: { deviceId, deviceToken, zoneId, tenantId, config }
    let device_id = resp.get("deviceId").and_then(|v| v.as_str())
        .or_else(|| resp.get("id").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let device_token = resp.get("deviceToken").and_then(|v| v.as_str()).map(|s| s.to_string());
    let zone_id = resp.get("zoneId").and_then(|v| v.as_str())
        .or_else(|| resp.get("zone").and_then(|z| z.get("id")).and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let zone_name = resp.get("zoneName").and_then(|v| v.as_str())
        .or_else(|| resp.get("zone").and_then(|z| z.get("name")).and_then(|v| v.as_str()))
        .map(|s| s.to_string());

    if device_id.is_some() && device_token.is_some() {
        // Get unpairPin from API response (generated server-side)
        let pin = resp.get("unpairPin").and_then(|v| v.as_str())
            .unwrap_or("PIANO").to_string();
        log::info!("Pairing successful! device={} zone={} unpairPin={}",
            device_id.as_deref().unwrap_or("?"), zone_name.as_deref().unwrap_or("?"), pin);
        config::AppConfig::update_and_save(|cfg| {
            cfg.device_id = device_id;
            cfg.device_token = device_token;
            cfg.zone_id = zone_id;
            cfg.zone_name = zone_name;
            cfg.paired = true;
            cfg.unpair_pin = Some(pin.clone());
        })?;
        // Trigger immediate sync after successful pairing
        log::info!("Triggering immediate sync...");
        sync::trigger_sync();
    } else {
        log::warn!("Pairing response missing device_id or token: {:?}", resp);
    }

    Ok(resp)
}

#[tauri::command]
async fn unpair_device(pin: String) -> Result<(), String> {
    let cfg = config::AppConfig::load();
    let expected = cfg.unpair_pin.unwrap_or_default();
    if pin.trim().to_uppercase() != expected {
        return Err("PIN_MISMATCH".to_string());
    }
    log::info!("unpair_device called — PIN verified, clearing config and stopping playback");
    if let Ok(mut player) = audio::player().lock() {
        player.stop();
        player.set_playlist(vec![]);
    }
    config::AppConfig::update_and_save(|cfg| {
        cfg.device_id = None;
        cfg.device_token = None;
        cfg.zone_id = None;
        cfg.zone_name = None;
        cfg.paired = false;
        cfg.unpair_pin = None;
    })?;
    Ok(())
}

#[tauri::command]
fn set_volume(volume: u8) -> Result<(), String> {
    audio::set_volume(volume)?;
    config::AppConfig::update_and_save(|cfg| {
        cfg.volume = volume;
    })?;
    Ok(())
}

#[tauri::command]
fn toggle_playback() -> Result<String, String> {
    audio::toggle()
}

#[tauri::command]
fn get_now_playing() -> serde_json::Value {
    let player = match audio::player().try_lock() {
        Ok(p) => p,
        Err(_) => return serde_json::json!(null),
    };

    match player.current_track() {
        Some(track) => serde_json::json!({
            "title": track.title,
            "artist": track.artist,
            "duration": track.duration,
            "position": player.get_position(),
            "artworkUrl": track.artwork_url,
        }),
        None => serde_json::json!(null),
    }
}

#[tauri::command]
fn get_logs() -> String {
    let log_path = config::AppConfig::data_dir().join("millsonic.log");
    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = if lines.len() > 100 { lines.len() - 100 } else { 0 };
            lines[start..].join("\n")
        }
        Err(e) => format!("Cannot read log: {}", e),
    }
}

/// Global atomics for watchdog: track audio position (centiseconds)
static WATCHDOG_LAST_POSITION: AtomicU64 = AtomicU64::new(0);
static WATCHDOG_STUCK_COUNT: AtomicU64 = AtomicU64::new(0);
static WATCHDOG_PREV_POSITION: AtomicU64 = AtomicU64::new(u64::MAX);

#[tauri::command]
fn install_launch_agent() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let plist_content = include_str!("../../resources/com.millsonic.player.plist");
        let home = dirs::home_dir().ok_or("Cannot find home directory")?;
        let dest = home.join("Library/LaunchAgents/com.millsonic.player.plist");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&dest, plist_content).map_err(|e| e.to_string())?;
        // Load the agent
        let output = std::process::Command::new("launchctl")
            .args(["load", "-w", &dest.to_string_lossy()])
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok("LaunchAgent installed and loaded".to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(format!("LaunchAgent installed, launchctl: {}", stderr))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("LaunchAgent is only supported on macOS".to_string())
    }
}

fn setup_logging() {
    let log_dir = config::AppConfig::data_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("millsonic.log");

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("Cannot open log file");

    CombinedLogger::init(vec![
        WriteLogger::new(LevelFilter::Info, LogConfig::default(), file),
    ]).expect("Cannot init logger");

    log::info!("=== Millsonic Player started ===");
    log::info!("Log file: {}", log_path.display());
    log::info!("Version: {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
    setup_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .invoke_handler(tauri::generate_handler![
            get_status,
            pair_device,
            unpair_device,
            set_volume,
            toggle_playback,
            get_now_playing,
            get_logs,
            install_launch_agent,
        ])
        .setup(|app| {
            // Load config and set initial volume
            let cfg = config::AppConfig::load();
            log::info!("About to init audio...");
            let _ = audio::set_volume(cfg.volume);
            log::info!("Audio initialized OK");

            // Start sync loop
            log::info!("Setting up sync loop...");
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                sync::start_sync_loop(handle).await;
            });

            // Start batch report flusher
            tauri::async_runtime::spawn(async move {
                sync::start_report_flusher().await;
            });

            // Start telemetry loop (HTTP fallback, runs when WS is down)
            let handle2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                telemetry::start_telemetry_loop(handle2).await;
            });

            // Start WebSocket connection
            let handle_ws = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                ws::start_ws_loop(handle_ws).await;
            });

            // Start HTTP polling fallback (active when WS is disconnected)
            let handle_poll = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                ws::start_http_polling_loop(handle_poll).await;
            });

            // Start now-playing emitter + track advancement check (every 1s)
            // This is the ONLY task that does blocking .lock() on audio player
            let handle3 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;

                    // Wrap in catch_unwind so a panic doesn't kill the app
                    let handle_ref = &handle3;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // Check track advancement
                        sync::check_track_advancement(handle_ref);

                        // Emit now-playing + update watchdog position
                        if let Ok(player) = audio::player().lock() {
                            let pos = player.get_position();
                            WATCHDOG_LAST_POSITION.store((pos * 100.0) as u64, Ordering::Relaxed);

                            if player.is_playing() {
                                if let Some(track) = player.current_track() {
                                    let _ = handle_ref.emit("now-playing", serde_json::json!({
                                        "title": track.title,
                                        "artist": track.artist,
                                        "duration": track.duration,
                                        "position": pos,
                                        "artworkUrl": track.artwork_url,
                                    }));
                                }
                            }
                        }
                    }));

                    if let Err(e) = result {
                        log::error!("PANIC in audio advancement loop: {:?}", e);
                    }
                }
            });

            // Watchdog: checks every 30s if audio is progressing
            let handle_wd = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Give the app time to start up
                tokio::time::sleep(Duration::from_secs(60)).await;

                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;

                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let is_playing = audio::player().try_lock()
                            .map(|p| p.is_playing() && p.playlist_len() > 0)
                            .unwrap_or(false);

                        if !is_playing {
                            WATCHDOG_STUCK_COUNT.store(0, Ordering::Relaxed);
                            return;
                        }

                        let current_pos = WATCHDOG_LAST_POSITION.load(Ordering::Relaxed);
                        let prev_pos = WATCHDOG_PREV_POSITION.swap(current_pos, Ordering::Relaxed);

                        // If position changed, reset stuck counter
                        if current_pos != prev_pos {
                            WATCHDOG_STUCK_COUNT.store(0, Ordering::Relaxed);
                            return;
                        }

                        let stuck = WATCHDOG_STUCK_COUNT.fetch_add(1, Ordering::Relaxed);

                        if stuck >= 2 {
                            // Stuck for >60s (2 checks × 30s), force restart playback
                            log::error!("WATCHDOG: Audio stuck for >60s at position {}cs, force-restarting!", current_pos);
                            if let Ok(mut player) = audio::player().lock() {
                                // Force stop current playback
                                player.stop();
                                // Advance to next track
                                if player.advance() {
                                    if let Err(e) = player.play_current() {
                                        log::error!("WATCHDOG: Failed to restart playback: {}", e);
                                    } else {
                                        log::info!("WATCHDOG: Playback restarted successfully");
                                        if let Some(track) = player.current_track() {
                                            let _ = handle_wd.emit("now-playing", serde_json::json!({
                                                "title": track.title,
                                                "artist": track.artist,
                                                "duration": track.duration,
                                                "position": 0.0,
                                                "artworkUrl": track.artwork_url,
                                            }));
                                        }
                                    }
                                }
                            }
                            WATCHDOG_STUCK_COUNT.store(0, Ordering::Relaxed);
                        } else {
                            log::debug!("WATCHDOG: position={}cs, stuck_count={}", current_pos, stuck + 1);
                        }
                    }));

                    if let Err(e) = result {
                        log::error!("PANIC in watchdog: {:?}", e);
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
