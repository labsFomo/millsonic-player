use serde::Serialize;
use std::path::Path;

const API_BASE: &str = "https://apifo.millsonic.com/api/v1";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Serialize)]
struct PairRequest {
    #[serde(rename = "pairingCode")]
    pairing_code: String,
    #[serde(rename = "hardwareId")]
    hardware_id: String,
}

fn get_hardware_id() -> String {
    // Try to load persisted hardware ID, or generate one
    let config = crate::config::get_config();
    if let Some(ref hw_id) = config.hardware_id {
        return hw_id.clone();
    }
    let hw_id = format!("tauri-{}", uuid::Uuid::new_v4());
    drop(config);
    // Save it
    crate::config::update_and_save_global(|c| { c.hardware_id = Some(hw_id.clone()); });
    hw_id
}

pub async fn pair_with_code(code: &str) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client()
        .post(format!("{}/devices/pair", API_BASE))
        .json(&PairRequest {
            pairing_code: code.to_string(),
            hardware_id: get_hardware_id(),
        })
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

pub async fn sync_device(device_id: &str, device_token: &str) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client()
        .get(format!("{}/devices/{}/sync?deviceToken={}", API_BASE, device_id, device_token))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

pub async fn send_telemetry(device_id: &str, device_token: &str, telemetry: &serde_json::Value) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut body = telemetry.clone();
    body["deviceToken"] = serde_json::json!(device_token);
    let resp = client()
        .post(format!("{}/devices/{}/telemetry", API_BASE, device_id))
        .json(&body)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

/// R-18: refresh the device token before it expires. Returns the new token.
pub async fn refresh_device_token(
    device_id: &str,
    device_token: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client()
        .post(format!("{}/devices/{}/refresh-token", API_BASE, device_id))
        .json(&serde_json::json!({ "deviceToken": device_token }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    resp.get("deviceToken")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "refresh-token: no deviceToken in response".into())
}

pub async fn ack_command(device_id: &str, device_token: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = client()
        .post(format!("{}/devices/{}/command-ack", API_BASE, device_id))
        .json(&serde_json::json!({ "deviceToken": device_token }))
        .send()
        .await?;
    Ok(())
}

#[derive(Serialize)]
struct PlayReportBatch {
    #[serde(rename = "deviceToken")]
    device_token: String,
    reports: Vec<serde_json::Value>,
}

pub async fn report_plays_batch(
    device_id: &str,
    device_token: &str,
    reports: Vec<serde_json::Value>,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let body = PlayReportBatch {
        device_token: device_token.to_string(),
        reports,
    };
    let resp = client()
        .post(format!("{}/devices/{}/play-report-batch", API_BASE, device_id))
        .json(&body)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

/// SonicBox — fetch the zone's now-playing payload (public endpoint).
/// Returns the full JSON; the caller reads `sonicboxNext` + `nextTracks[0]`.
pub async fn fetch_zone_now_playing(
    zone_id: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client()
        .get(format!("{}/zones/{}/now-playing", API_BASE, zone_id))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

/// SonicBox — report a completed play to the *real* player endpoint so the
/// backend closes the vote loop (markPlayed) when completed && !skipped.
/// Auth: the device's pairing `deviceToken` IS a signed device JWT, so we send
/// it as a Bearer token. Body shape matches PlayReportDto { plays: [...] }.
pub async fn report_player_play(
    device_token: &str,
    plays: Vec<serde_json::Value>,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client()
        .post(format!("{}/player/play-report", API_BASE))
        .bearer_auth(device_token)
        .json(&serde_json::json!({ "plays": plays }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

pub async fn download_track(url: &str, dest_path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let download_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let resp = download_client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {} downloading track: {}", status, &body[..body.len().min(200)]).into());
    }
    let bytes = resp.bytes().await?;
    // R-04: reject suspiciously small payloads (error pages / truncated bodies)
    // so a corrupt file never lands in the cache and breaks playback later.
    if bytes.len() < 2048 {
        return Err(format!(
            "download too small ({} bytes), likely not audio: {}",
            bytes.len(),
            dest_path.display()
        )
        .into());
    }
    // R-04: write to a temp file then atomically rename, so an interrupted
    // download can never appear as a complete cached track.
    let tmp_path = dest_path.with_extension("part");
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, dest_path)?;
    log::info!("Downloaded {} bytes to {}", bytes.len(), dest_path.display());
    Ok(())
}
