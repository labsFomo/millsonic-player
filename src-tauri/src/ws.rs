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
    match cmd {
        "play" => {
            if let Ok(mut p) = audio::player().lock() {
                p.resume();
            }
        }
        "pause" => {
            if let Ok(mut p) = audio::player().lock() {
                p.pause();
            }
        }
        "setVolume" => {
            if let Some(vol) = data.get("value").and_then(|v| v.as_u64()) {
                if let Ok(mut p) = audio::player().lock() {
                    p.set_volume(vol as u8);
                }
                let _ = config::AppConfig::update_and_save(|cfg| {
                    cfg.volume = vol as u8;
                });
            }
        }
        "forceSync" => {
            sync::trigger_sync();
        }
        "skipTrack" => {
            if let Ok(mut p) = audio::player().lock() {
                let _ = p.skip_track();
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
    loop {
        let cfg = config::AppConfig::load();
        if !cfg.is_paired() {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let device_id = cfg.device_id.clone().unwrap();
        let device_token = cfg.device_token.clone().unwrap();

        // Send telemetry via HTTP and check for pending commands
        let telem = build_telemetry();
        match api::send_telemetry(&device_id, &device_token, &telem).await {
            Ok(resp) => {
                if let Some(cmd) = resp.get("pendingCommand").and_then(|c| c.as_str()) {
                    if !cmd.is_empty() {
                        let cmd_data = resp.clone();
                        execute_command(cmd, &cmd_data);
                        // ACK via HTTP
                        let _ = ack_command_http(&device_id, &device_token, &resp).await;
                    }
                }
            }
            Err(e) => log::error!("HTTP polling telemetry error: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn ack_command_http(device_id: &str, device_token: &str, resp: &serde_json::Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd_id = resp.get("commandId").and_then(|v| v.as_str()).unwrap_or("");
    let client = reqwest::Client::new();
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
