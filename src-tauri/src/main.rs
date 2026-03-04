#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod sync;
mod config;
mod telemetry;
mod api;

use tauri::Manager;

#[tauri::command]
fn get_status() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "status": "ready"
    })
}

#[tauri::command]
async fn pair_device(code: String) -> Result<serde_json::Value, String> {
    api::pair_with_code(&code).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_volume(volume: u8) -> Result<(), String> {
    audio::set_volume(volume).map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_playback() -> Result<String, String> {
    audio::toggle().map_err(|e| e.to_string())
}

fn main() {
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
        ])
        .setup(|app| {
            // Start background tasks
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Start sync loop
                sync::start_sync_loop(handle).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
