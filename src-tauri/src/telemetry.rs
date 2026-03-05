use crate::{api, audio, config};
use sysinfo::System;
use std::time::Duration;
use tauri::AppHandle;

pub fn get_telemetry() -> serde_json::Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory() as f64 / 1_048_576.0;
    let used_mem = sys.used_memory() as f64 / 1_048_576.0;

    let player = audio::player().lock().ok();
    let is_playing = player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
    let current_track_id = player
        .as_ref()
        .and_then(|p| p.current_track().map(|t| t.track_id.clone()));

    serde_json::json!({
        "cpuUsage": sys.global_cpu_usage(),
        "ramUsage": used_mem,
        "ramTotal": total_mem,
        "diskFree": get_disk_free(),
        "diskTotal": get_disk_total(),
        "isPlaying": is_playing,
        "currentTrackId": current_track_id,
        "appVersion": env!("CARGO_PKG_VERSION"),
    })
}

fn get_disk_free() -> f64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    // Use root "/" (Mac/Linux) or "C:\" (Windows) to avoid APFS multi-volume double-counting
    disks.iter()
        .find(|d| {
            let mp = d.mount_point().to_string_lossy();
            mp == "/" || mp == "C:\\"
        })
        .map(|d| d.available_space() as f64 / 1_073_741_824.0)
        .unwrap_or_else(|| disks.iter().map(|d| d.available_space() as f64 / 1_073_741_824.0).fold(0.0_f64, f64::max))
}

fn get_disk_total() -> f64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    disks.iter()
        .find(|d| {
            let mp = d.mount_point().to_string_lossy();
            mp == "/" || mp == "C:\\"
        })
        .map(|d| d.total_space() as f64 / 1_073_741_824.0)
        .unwrap_or_else(|| disks.iter().map(|d| d.total_space() as f64 / 1_073_741_824.0).fold(0.0_f64, f64::max))
}

pub async fn start_telemetry_loop(_handle: AppHandle) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        let cfg = config::AppConfig::load();
        if !cfg.is_paired() {
            continue;
        }

        // Skip HTTP telemetry when WS is connected (WS sends its own)
        if crate::ws::is_ws_connected() {
            continue;
        }

        let device_id = cfg.device_id.clone().unwrap();
        let device_token = cfg.device_token.clone().unwrap();
        let telemetry = get_telemetry();

        match api::send_telemetry(&device_id, &device_token, &telemetry).await {
            Ok(resp) => {
                // Handle pending commands
                if let Some(cmd_raw) = resp.get("pendingCommand").and_then(|c| c.as_str()) {
                    // pendingCommand is JSON string: {"command":"pause","value":null}
                    if let Ok(cmd_obj) = serde_json::from_str::<serde_json::Value>(cmd_raw) {
                        let cmd = cmd_obj.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        handle_command(cmd, &cmd_obj);
                    } else {
                        // Fallback: treat as plain command string
                        handle_command(cmd_raw, &resp);
                    }
                }
            }
            Err(e) => log::error!("Telemetry error: {}", e),
        }
    }
}

fn handle_command(cmd: &str, resp: &serde_json::Value) {
    log::info!("Executing remote command: {}", cmd);
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(_) => return,
    };

    match cmd {
        "play" => player.resume(),
        "pause" => player.pause(),
        "setVolume" => {
            if let Some(val) = resp.get("value").or_else(|| resp.get("commandValue")).and_then(|v| v.as_u64()) {
                player.set_volume(val as u8);
            }
        }
        "skipTrack" => {
            let _ = player.skip_track();
        }
        "forceSync" => {
            crate::sync::trigger_sync();
        }
        _ => log::warn!("Unknown command: {}", cmd),
    }
}
