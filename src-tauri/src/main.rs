#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod sync;
mod config;
mod telemetry;
mod api;

use tauri::{Manager, Emitter};
use std::time::Duration;

#[tauri::command]
fn get_status() -> serde_json::Value {
    let cfg = config::AppConfig::load();
    let player = audio::player().lock().ok();
    let is_playing = player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
    let volume = player.as_ref().map(|p| p.get_volume()).unwrap_or(80);
    let track = player.as_ref().and_then(|p| p.current_track().map(|t| t.title.clone()));
    let artist = player.as_ref().and_then(|p| p.current_track().map(|t| t.artist.clone()));

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "paired": cfg.is_paired(),
        "playing": is_playing,
        "volume": volume,
        "track": track,
        "artist": artist,
        "zoneName": cfg.zone_name,
        "online": true,
    })
}

#[tauri::command]
async fn pair_device(code: String) -> Result<serde_json::Value, String> {
    let resp = api::pair_with_code(&code).await.map_err(|e| e.to_string())?;

    // Check for error in response
    if resp.get("statusCode").is_some() {
        return Ok(resp); // API error, return as-is for frontend to handle
    }

    // Save pairing info to config
    let device_id = resp.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let device_token = resp.get("deviceToken").and_then(|v| v.as_str()).map(|s| s.to_string());
    let zone_id = resp.get("zone").and_then(|z| z.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let zone_name = resp.get("zone").and_then(|z| z.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string());

    if device_id.is_some() && device_token.is_some() {
        config::AppConfig::update_and_save(|cfg| {
            cfg.device_id = device_id;
            cfg.device_token = device_token;
            cfg.zone_id = zone_id;
            cfg.zone_name = zone_name;
            cfg.paired = true;
        })?;
    }

    Ok(resp)
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
    let player = match audio::player().lock() {
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

fn main() {
    env_logger::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .invoke_handler(tauri::generate_handler![
            get_status,
            pair_device,
            set_volume,
            toggle_playback,
            get_now_playing,
        ])
        .setup(|app| {
            // Load config and set initial volume
            let cfg = config::AppConfig::load();
            let _ = audio::set_volume(cfg.volume);

            // Start sync loop
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                sync::start_sync_loop(handle).await;
            });

            // Start telemetry loop
            let handle2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                telemetry::start_telemetry_loop(handle2).await;
            });

            // Start now-playing emitter + track advancement check (every 1s)
            let handle3 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;

                    // Check track advancement
                    sync::check_track_advancement(&handle3);

                    // Emit now-playing
                    if let Ok(player) = audio::player().lock() {
                        if player.is_playing() {
                            if let Some(track) = player.current_track() {
                                let _ = handle3.emit("now-playing", serde_json::json!({
                                    "title": track.title,
                                    "artist": track.artist,
                                    "duration": track.duration,
                                    "position": player.get_position(),
                                    "artworkUrl": track.artwork_url,
                                }));
                            }
                        }
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
