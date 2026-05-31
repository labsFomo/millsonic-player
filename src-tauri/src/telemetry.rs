use crate::{api, audio, config};
use sysinfo::System;
use std::time::Duration;
use std::sync::{Mutex, OnceLock};
use chrono::Utc;
use tauri::{AppHandle, Emitter};

// ── Health telemetry: lastCrash (dirty-shutdown / panic detection) ───────────
// Populated ONCE at startup (main.rs setup) by inspecting the leftover
// running.marker + panic.txt from the previous run. Reported on every telemetry
// poll (the backend keeps the latest; repeating it is fine). Tuple:
// (at ISO8601, class, message, phase).
static LAST_CRASH: OnceLock<Mutex<Option<(String, String, String, Option<String>)>>> = OnceLock::new();

fn last_crash_cell() -> &'static Mutex<Option<(String, String, String, Option<String>)>> {
    LAST_CRASH.get_or_init(|| Mutex::new(None))
}

/// Record a detected crash from the previous run (called from main.rs setup).
pub fn set_last_crash(at: String, class: String, message: String, phase: Option<String>) {
    let mut g = last_crash_cell().lock().unwrap_or_else(|e| e.into_inner());
    *g = Some((at, class, message, phase));
}

/// Last detected unclean shutdown / panic, if any.
pub fn last_crash() -> Option<(String, String, String, Option<String>)> {
    last_crash_cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn running_marker_path() -> std::path::PathBuf {
    config::AppConfig::data_dir().join("running.marker")
}

fn panic_file_path() -> std::path::PathBuf {
    config::AppConfig::data_dir().join("panic.txt")
}

/// Write the current time into the run marker (creates it if missing). Its mtime
/// then approximates "last moment the process was alive" — if the process dies
/// dirty, the marker survives and the next boot reads ≈ the crash time.
pub fn touch_running_marker() {
    let path = running_marker_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, Utc::now().to_rfc3339());
}

/// Remove the run marker. Call on every KNOWN clean exit (graceful restart,
/// unpair, etc.) so the next boot doesn't mistake it for a crash.
pub fn clear_running_marker() {
    let _ = std::fs::remove_file(running_marker_path());
}

/// Install a panic hook that records the panic message + location to panic.txt,
/// so a Rust panic in a previous run can enrich LAST_CRASH on the next boot with
/// a real message instead of just "unclean shutdown".
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "panic".to_string(),
            },
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let body = format!("{}\nat {}\n{}", msg, loc, Utc::now().to_rfc3339());
        let _ = std::fs::write(panic_file_path(), body);
        default(info);
    }));
}

/// Detect whether the PREVIOUS run exited uncleanly and, if so, populate
/// LAST_CRASH. Then (re)create the run marker for THIS run. Call once at startup.
///
/// Logic:
///   - If panic.txt exists → previous run panicked (Rust panic). class="panic",
///     message=its contents. Consume (delete) it.
///   - Else if running.marker exists → previous run died without removing it
///     (native crash, kill -9, power loss). class="unclean_shutdown".
///   - mtime of the marker ≈ the last time the process was alive ≈ crash time.
pub fn detect_previous_crash() {
    let marker = running_marker_path();
    let panic_file = panic_file_path();

    // Approximate crash time = marker mtime (last touch while alive).
    let crash_at = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<Utc>::from(t).to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    if let Ok(panic_body) = std::fs::read_to_string(&panic_file) {
        let msg = panic_body.trim();
        log::warn!("Health: previous run PANICKED — {}", msg.lines().next().unwrap_or(msg));
        set_last_crash(
            crash_at,
            "panic".to_string(),
            if msg.is_empty() { "panic (sin mensaje)".to_string() } else { msg.to_string() },
            Some("runtime".to_string()),
        );
        let _ = std::fs::remove_file(&panic_file);
    } else if marker.exists() {
        log::warn!("Health: previous run did NOT exit cleanly (run marker present) — flagging unclean shutdown");
        set_last_crash(
            crash_at,
            "unclean_shutdown".to_string(),
            "proceso terminó sin salida limpia (crash nativo o kill)".to_string(),
            Some("runtime".to_string()),
        );
    }

    // (Re)create the marker for the current run.
    touch_running_marker();
}

pub fn get_telemetry() -> serde_json::Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory() as f64 / 1_048_576.0;
    let used_mem = sys.used_memory() as f64 / 1_048_576.0;

    // Use try_lock to never block audio thread
    let player = audio::player().try_lock().ok();
    let is_playing = player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
    let volume = player.as_ref().map(|p| p.get_volume()).unwrap_or(80);
    // When a SonicBox vote is playing, current_track() still points at the grid
    // playlist slot (the vote lives in a separate field). Report the vote's
    // trackId so the backend knows what's ACTUALLY on air — peekNext excludes the
    // currently-playing track to surface the NEXT vote, and without this the
    // backend kept handing back the playing vote, wedging a grid song between
    // every voted track instead of draining the queue.
    let current_track_id = player.as_ref().and_then(|p| {
        if p.playing_sonicbox {
            p.sonicbox_current().map(|t| t.track_id.clone())
        } else {
            p.current_track().map(|t| t.track_id.clone())
        }
    });

    // Health stats: uptime + cache size. These feed the device "Health" view in
    // the client (sin esto la sección venía vacía — vivían en el build_telemetry
    // de ws.rs, que quedó muerto al unificar los pollers).
    let uptime = crate::ws::app_start_time().elapsed().as_secs();
    let cache_dir = config::AppConfig::cache_dir();
    let cache_bytes: u64 = std::fs::read_dir(&cache_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    let free_bytes = (get_disk_free() * 1_073_741_824.0) as u64;
    let total_bytes = (get_disk_total() * 1_073_741_824.0) as u64;

    let mut payload = serde_json::json!({
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
        "uptime": uptime,
        "cacheBytes": cache_bytes,
        "freeBytes": free_bytes,
        "totalBytes": total_bytes,
    });

    if let Some(obj) = payload.as_object_mut() {
        // Health: last sync (timestamp/status/trigger) — filled by the sync loop.
        if let Some((at, status, trigger)) = crate::sync::last_sync() {
            obj.insert("lastSyncAt".to_string(), serde_json::json!(at));
            obj.insert("lastSyncStatus".to_string(), serde_json::json!(status));
            obj.insert("lastSyncTrigger".to_string(), serde_json::json!(trigger));
        }
        // Health: last crash (unclean shutdown / panic), detected at startup.
        if let Some((at, class, message, phase)) = last_crash() {
            obj.insert("lastCrashAt".to_string(), serde_json::json!(at));
            obj.insert("lastCrashClass".to_string(), serde_json::json!(class));
            obj.insert("lastCrashMessage".to_string(), serde_json::json!(message));
            obj.insert("lastCrashPhase".to_string(), serde_json::json!(phase));
        }
        // Health: current content manifest version = fingerprint of the loaded
        // track list (ordered track ids). It's the closest thing the player has
        // to a "manifest version" of the schedule/content it's currently playing.
        if let Some(fp) = crate::sync::current_manifest_version() {
            obj.insert("manifestVersion".to_string(), serde_json::json!(fp));
        }
    }

    payload
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
            // Health: known clean restart — clear the marker so the post-restart
            // boot doesn't flag this as an unclean shutdown.
            clear_running_marker();
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
