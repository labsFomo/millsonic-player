use crate::{api, audio, config, sync, telemetry};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};

#[allow(unused_imports)]
use log;

static WS_CONNECTED: OnceLock<AtomicBool> = OnceLock::new();
static APP_START: OnceLock<Instant> = OnceLock::new();

fn ws_connected() -> &'static AtomicBool {
    WS_CONNECTED.get_or_init(|| AtomicBool::new(false))
}

pub fn app_start_time() -> Instant {
    *APP_START.get_or_init(Instant::now)
}

pub fn is_ws_connected() -> bool {
    ws_connected().load(Ordering::Relaxed)
}

fn set_ws_connected(connected: bool, handle: &AppHandle) {
    let prev = ws_connected().swap(connected, Ordering::Relaxed);
    if prev != connected {
        let status = if connected { "connected" } else { "disconnected" };
        log::info!("WebSocket status: {}", status);
        let _ = handle.emit("ws-status", serde_json::json!({ "status": status }));
    }
}

fn execute_command(cmd: &str, data: &serde_json::Value) {
    log::info!("WS command: {} data={}", cmd, data);
    let cmd_lower = cmd.to_lowercase();
    match cmd_lower.as_str() {
        "play" => {
            if let Ok(mut p) = audio::player().try_lock() {
                p.resume();
            } else {
                log::warn!("Could not lock audio player for play command (busy)");
            }
        }
        "pause" => {
            if let Ok(mut p) = audio::player().try_lock() {
                p.pause();
            } else {
                log::warn!("Could not lock audio player for pause command (busy)");
            }
        }
        "setvolume" | "volume" => {
            if let Some(vol) = data.get("value").and_then(|v| v.as_u64()) {
                if let Ok(mut p) = audio::player().try_lock() {
                    p.set_volume(vol as u8);
                } else {
                    log::warn!("Could not lock audio player for setVolume (busy)");
                }
                let _ = config::AppConfig::update_and_save(|cfg| {
                    cfg.volume = vol as u8;
                });
            }
        }
        "forcesync" => {
            sync::trigger_sync();
        }
        "skiptrack" => {
            if let Ok(mut p) = audio::player().try_lock() {
                let _ = p.skip_track();
            } else {
                log::warn!("Could not lock audio player for skipTrack (busy)");
            }
        }
        "restart" => {
            log::info!("Restart command received, exiting...");
            std::process::exit(0);
        }
        _ => {
            log::warn!("Unknown WS command: {}", cmd);
        }
    }
}

pub async fn start_ws_loop(handle: AppHandle) {
    // Init start time
    let _ = app_start_time();
    
    // WS gateway removed from API — use HTTP polling exclusively
    log::info!("WebSocket disabled, using HTTP polling only");
    let _ = handle;
    return;

    #[allow(unreachable_code)]
    let backoff_steps = [5u64, 10, 30, 60];
    let mut backoff_idx = 0;

    loop {
        let cfg = config::AppConfig::load();
        if !cfg.is_paired() {
            tokio::time::sleep(Duration::from_secs(5)).await;
            backoff_idx = 0;
            continue;
        }

        let token = cfg.device_token.clone().unwrap();
        let _device_id = cfg.device_id.clone().unwrap();
        let url = format!("wss://apimillsonic.fo.com.uy/devices/ws?deviceToken={}", token);

        log::info!("WebSocket connecting to {}", url);

        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                log::info!("WebSocket connected!");
                set_ws_connected(true, &handle);
                backoff_idx = 0;

                let (mut write, mut read) = ws_stream.split();

                // Telemetry ticker
                let mut telemetry_interval = tokio::time::interval(Duration::from_secs(60));
                telemetry_interval.tick().await; // skip first immediate tick

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    match serde_json::from_str::<serde_json::Value>(&text) {
                                        Ok(parsed) => {
                                            let event = parsed.get("event").and_then(|e| e.as_str()).unwrap_or("");
                                            if event == "command" {
                                                let cmd = parsed.get("data").and_then(|d| d.get("command")).and_then(|c| c.as_str()).unwrap_or("");
                                                let cmd_data = parsed.get("data").cloned().unwrap_or(serde_json::json!({}));
                                                let cmd_id = parsed.get("data").and_then(|d| d.get("id")).and_then(|v| v.as_str()).unwrap_or("");
                                                
                                                execute_command(cmd, &cmd_data);
                                                
                                                // Send ack
                                                let ack = serde_json::json!({
                                                    "event": "command-ack",
                                                    "data": { "commandId": cmd_id, "status": "executed" }
                                                });
                                                if let Err(e) = write.send(Message::Text(ack.to_string().into())).await {
                                                    log::error!("Failed to send command-ack: {}", e);
                                                    break;
                                                }
                                            }
                                        }
                                        Err(e) => log::warn!("WS parse error: {}", e),
                                    }
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    log::info!("WebSocket closed");
                                    break;
                                }
                                Some(Err(e)) => {
                                    log::error!("WebSocket error: {}", e);
                                    break;
                                }
                                _ => {}
                            }
                        }
                        _ = telemetry_interval.tick() => {
                            let telem = build_telemetry();
                            let msg = serde_json::json!({
                                "event": "telemetry",
                                "data": telem
                            });
                            if let Err(e) = write.send(Message::Text(msg.to_string().into())).await {
                                log::error!("Failed to send telemetry via WS: {}", e);
                                break;
                            }
                            log::info!("Telemetry sent via WS");
                        }
                    }
                }

                set_ws_connected(false, &handle);
            }
            Err(e) => {
                log::error!("WebSocket connection failed: {}", e);
                set_ws_connected(false, &handle);
            }
        }

        // Backoff
        let wait = backoff_steps[backoff_idx.min(backoff_steps.len() - 1)];
        log::info!("WS reconnecting in {}s...", wait);
        tokio::time::sleep(Duration::from_secs(wait)).await;
        if backoff_idx < backoff_steps.len() - 1 {
            backoff_idx += 1;
        }
    }
}

/// HTTP polling - primary command/telemetry channel
pub async fn start_http_polling_loop(_handle: AppHandle) {
    let mut consecutive_failures: u32 = 0;

    loop {
        let cfg = config::AppConfig::load();
        if !cfg.is_paired() {
            tokio::time::sleep(Duration::from_secs(5)).await;
            consecutive_failures = 0;
            continue;
        }

        let device_id = cfg.device_id.clone().unwrap();
        let device_token = cfg.device_token.clone().unwrap();

        // Send telemetry via HTTP with timeout — NEVER block
        let telem = build_telemetry();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            api::send_telemetry(&device_id, &device_token, &telem),
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
                        log::info!("Executing pending command: {}", command);
                        execute_command(&command, &cmd_data);
                        // Ack with timeout too
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            ack_command_http(&device_id, &device_token, &resp),
                        ).await;
                    }
                }
            }
            Ok(Err(e)) => {
                consecutive_failures += 1;
                log::error!("HTTP polling telemetry error: {} (failures: {})", e, consecutive_failures);
            }
            Err(_) => {
                consecutive_failures += 1;
                log::warn!("HTTP polling telemetry timed out (failures: {})", consecutive_failures);
            }
        }

        // Exponential backoff: 10s → 20s → 30s → max 60s
        let wait = match consecutive_failures {
            0 => 10,
            1 => 10,
            2 => 20,
            3 => 30,
            _ => 60,
        };
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

async fn ack_command_http(device_id: &str, device_token: &str, resp: &serde_json::Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd_id = resp.get("commandId").and_then(|v| v.as_str()).unwrap_or("");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    client.post(format!("https://apimillsonic.fo.com.uy/api/v1/devices/{}/command-ack", device_id))
        .json(&serde_json::json!({
            "deviceToken": device_token,
            "commandId": cmd_id,
            "status": "executed"
        }))
        .send()
        .await?;
    Ok(())
}

fn build_telemetry() -> serde_json::Value {
    let mut telem = telemetry::get_telemetry();
    let uptime = app_start_time().elapsed().as_secs();
    
    // Count cached files
    let cache_dir = config::AppConfig::cache_dir();
    let cache_size = std::fs::read_dir(&cache_dir)
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0);

    let conn_status = "online";

    if let Some(obj) = telem.as_object_mut() {
        obj.insert("uptime".to_string(), serde_json::json!(uptime));
        obj.insert("cacheSize".to_string(), serde_json::json!(cache_size));
        obj.insert("connectionStatus".to_string(), serde_json::json!(conn_status));
        obj.insert("version".to_string(), serde_json::json!(env!("CARGO_PKG_VERSION")));
    }
    telem
}
