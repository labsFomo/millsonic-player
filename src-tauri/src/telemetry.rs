use crate::{api, audio, config};
use sysinfo::System;
use std::time::Duration;
use tauri::AppHandle;

pub fn get_telemetry() -> serde_json::Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory() as f64 / 1_048_576.0;
    let used_mem = sys.used_memory() as f64 / 1_048_576.0;

    // Use try_lock to never block audio thread
    let player = audio::player().try_lock().ok();
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
    let mut consecutive_failures: u32 = 0;

    loop {
        // Exponential backoff interval: 60s base, up to 120s on failures
        let interval = match consecutive_failures {
            0 => 60,
            1 => 60,
            2 => 80,
            3 => 100,
            _ => 120,
        };
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let cfg = config::AppConfig::load();
        if !cfg.is_paired() {
            consecutive_failures = 0;
            continue;
        }

        let device_id = cfg.device_id.clone().unwrap();
        let device_token = cfg.device_token.clone().unwrap();
        let telemetry = get_telemetry();

        // Wrap in timeout — NEVER block
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            api::send_telemetry(&device_id, &device_token, &telemetry),
        ).await;

        match result {
            Ok(Ok(resp)) => {
                consecutive_failures = 0;
                if let Some(pending) = resp.get("pendingCommand") {
                    let (command, cmd_data) = if let Some(cmd_obj) = pending.as_object() {
                        let cmd = cmd_obj.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        (cmd.to_string(), pending.clone())
                    } else if let Some(cmd_str) = pending.as_str() {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(cmd_str) {
                            let cmd = parsed.get("command").and_then(|c| c.as_str()).unwrap_or(cmd_str);
                            (cmd.to_string(), parsed)
                        } else {
                            (cmd_str.to_string(), resp.clone())
                        }
                    } else {
                        (String::new(), resp.clone())
                    };
                    if !command.is_empty() {
                        handle_command(&command, &cmd_data);
                    }
                }
            }
            Ok(Err(e)) => {
                consecutive_failures += 1;
                log::error!("Telemetry error: {} (failures: {})", e, consecutive_failures);
            }
            Err(_) => {
                consecutive_failures += 1;
                log::warn!("Telemetry request timed out (failures: {})", consecutive_failures);
            }
        }
    }
}

fn handle_command(cmd: &str, resp: &serde_json::Value) {
    log::info!("Executing remote command: {}", cmd);
    let mut player = match audio::player().try_lock() {
        Ok(p) => p,
        Err(_) => {
            log::warn!("Could not lock audio player for command '{}' (busy)", cmd);
            return;
        }
    };

    let cmd_lower = cmd.to_lowercase();
    match cmd_lower.as_str() {
        "play" => player.resume(),
        "pause" => player.pause(),
        "setvolume" | "volume" => {
            if let Some(val) = resp.get("value").or_else(|| resp.get("commandValue")).and_then(|v| v.as_u64()) {
                player.set_volume(val as u8);
            }
        }
        "skiptrack" | "next" | "skip" => {
            let _ = player.skip_track();
        }
        "forcesync" | "force_sync" => {
            crate::sync::trigger_sync();
        }
        _ => log::warn!("Unknown command: {}", cmd),
    }
}
