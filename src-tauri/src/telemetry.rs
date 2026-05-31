use crate::{api, audio, config};
use sysinfo::System;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

pub fn get_telemetry() -> serde_json::Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory() as f64 / 1_048_576.0;
    let used_mem = sys.used_memory() as f64 / 1_048_576.0;

    // Use try_lock to never block audio thread
    let player = audio::player().try_lock().ok();
    let is_playing = player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
    let volume = player.as_ref().map(|p| p.get_volume()).unwrap_or(80);
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
        "volume": volume,
        "currentTrackId": current_track_id,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "debugMode": config::AppConfig::load().debug_mode,
    })
}

fn get_disk_info() -> (f64, f64) {
    // Use statvfs for accurate disk info on macOS/Linux (avoids APFS multi-volume issues)
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let path = CString::new("/").unwrap();
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(path.as_ptr(), &mut stat) == 0 {
                let total = (stat.f_blocks as f64 * stat.f_frsize as f64) / 1_073_741_824.0;
                let free = (stat.f_bavail as f64 * stat.f_frsize as f64) / 1_073_741_824.0;
                return (free, total);
            }
        }
    }
    // Fallback to sysinfo
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    let disk = disks.iter().find(|d| {
        let mp = d.mount_point().to_string_lossy();
        mp == "/" || mp == "C:\\"
    });
    let free = disk.map(|d| d.available_space() as f64 / 1_073_741_824.0).unwrap_or(0.0);
    let total = disk.map(|d| d.total_space() as f64 / 1_073_741_824.0).unwrap_or(0.0);
    (free, total)
}

fn get_disk_free() -> f64 { get_disk_info().0 }
fn get_disk_total() -> f64 { get_disk_info().1 }

pub async fn start_telemetry_loop(handle: AppHandle) {
    let mut consecutive_failures: u32 = 0;

    loop {
        // Single command+telemetry poller (the redundant ws HTTP poller was
        // removed — it had an incomplete command handler that swallowed
        // UPDATE/SHOW_STATS in a race). 8s base keeps remote commands snappy;
        // back off on repeated failures to avoid hammering a struggling network.
        let interval = match consecutive_failures {
            0 => 8,
            1 => 8,
            2 => 20,
            3 => 40,
            _ => 60,
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
                crate::sync::set_connection_status(crate::sync::ConnectionStatus::Online, &handle);
                if let Some(pending) = resp.get("pendingCommand") {
                    // pendingCommand may be a single {command,value} object (legacy),
                    // an ARRAY of them (command queue), or a JSON string of either.
                    // Normalize to a list and execute each in order (FIFO). The API
                    // clears pendingCommand on read (clear-on-read), so no ack is
                    // needed — and acking would wipe any command queued in the
                    // meantime.
                    let items: Vec<serde_json::Value> = match pending {
                        serde_json::Value::Array(arr) => arr.clone(),
                        serde_json::Value::Object(_) => vec![pending.clone()],
                        serde_json::Value::String(s) => {
                            match serde_json::from_str::<serde_json::Value>(s) {
                                Ok(serde_json::Value::Array(arr)) => arr,
                                Ok(v @ serde_json::Value::Object(_)) => vec![v],
                                _ => vec![serde_json::json!({ "command": s })],
                            }
                        }
                        _ => vec![],
                    };
                    for item in &items {
                        let command = item
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !command.is_empty() {
                            handle_command(&command, item, &handle);
                        }
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

fn handle_command(cmd: &str, resp: &serde_json::Value, handle: &tauri::AppHandle) {
    log::info!("Executing remote command: {}", cmd);
    let cmd_lower = cmd.to_lowercase();

    // ── Commands that do NOT need the audio lock ──
    // Handled first so they never get dropped while a crossfade holds the lock.
    match cmd_lower.as_str() {
        "forcesync" | "force_sync" | "sync" => {
            crate::sync::trigger_sync();
            return;
        }
        "show_stats" | "showstats" | "stats" => {
            log::info!("Show stats requested");
            let _ = handle.emit("show-stats", serde_json::json!({}));
            return;
        }
        "set_debug" | "setdebug" | "debug" => {
            // Accept every shape clients send: value:true (bool), value:{enabled:true}
            // (object — what the admin UI sends), or a top-level `enabled`. The old
            // code only did value.as_bool(), so the admin's {enabled} object always
            // parsed as None -> false and debug could never be turned ON.
            let val = resp.get("value");
            let enabled = val.and_then(|v| v.as_bool())
                .or_else(|| val.and_then(|v| v.get("enabled")).and_then(|v| v.as_bool()))
                .or_else(|| resp.get("commandValue").and_then(|v| v.as_bool()))
                .or_else(|| resp.get("enabled").and_then(|v| v.as_bool()))
                .unwrap_or(false);
            log::info!("Debug mode set to: {} (via telemetry command)", enabled);
            let _ = config::AppConfig::update_and_save(|cfg| { cfg.debug_mode = enabled; });
            let _ = handle.emit("debug-mode", serde_json::json!({ "enabled": enabled }));
            return;
        }
        "update" => {
            log::info!("Update command received, triggering install");
            let h = handle.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::updater::install_update(h).await {
                    log::error!("Remote update failed: {}", e);
                }
            });
            return;
        }
        "restart" => {
            log::info!("Restart command received, restarting app");
            handle.restart();
        }
        _ => {}
    }

    // ── Audio commands (need the player lock) ──
    let mut player = match audio::player().try_lock() {
        Ok(p) => p,
        Err(_) => {
            log::warn!("Could not lock audio player for command '{}' (busy)", cmd);
            return;
        }
    };
    match cmd_lower.as_str() {
        "play" => {
            log::info!("Resuming playback");
            player.resume();
            let _ = handle.emit("playback-state", serde_json::json!({ "state": "playing" }));
        }
        "pause" => {
            log::info!("Pausing playback");
            player.pause();
            let _ = handle.emit("playback-state", serde_json::json!({ "state": "paused" }));
        }
        "setvolume" | "volume" | "set_volume" => {
            if let Some(val) = resp.get("value").or_else(|| resp.get("commandValue")).and_then(|v| v.as_u64()) {
                log::info!("Setting volume to {}%", val);
                player.set_volume(val as u8);
                let _ = handle.emit("volume-change", serde_json::json!({ "volume": val }));
            } else {
                log::warn!("VOLUME command missing value: {:?}", resp);
            }
        }
        "skiptrack" | "next" | "skip" => {
            let _ = player.skip_track();
        }
        _ => log::warn!("Unknown command: {}", cmd),
    }
}
