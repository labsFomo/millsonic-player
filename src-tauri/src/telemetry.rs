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
    disks.iter().map(|d| d.available_space() as f64 / 1_073_741_824.0).sum()
}

fn get_disk_total() -> f64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    disks.iter().map(|d| d.total_space() as f64 / 1_073_741_824.0).sum()
}

pub async fn start_telemetry_loop(_handle: AppHandle) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        let cfg = config::AppConfig::load();
        if !cfg.is_paired() {
            continue;
        }

        let device_id = cfg.device_id.clone().unwrap();
        let device_token = cfg.device_token.clone().unwrap();
        let telemetry = get_telemetry();

        match api::send_telemetry(&device_id, &device_token, &telemetry).await {
            Ok(resp) => {
                // Handle pending commands
                if let Some(cmd) = resp.get("pendingCommand").and_then(|c| c.as_str()) {
                    handle_command(cmd, &resp);
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
            if let Some(val) = resp.get("commandValue").and_then(|v| v.as_u64()) {
                player.set_volume(val as u8);
            }
        }
        "skipTrack" => {
            let _ = player.skip_track();
        }
        "forceSync" => {
            // TODO: trigger immediate sync
            log::info!("Force sync requested");
        }
        _ => log::warn!("Unknown command: {}", cmd),
    }
}
