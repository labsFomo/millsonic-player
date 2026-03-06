use serde::Serialize;
use std::path::Path;

const API_BASE: &str = "https://apimillsonic.fo.com.uy/api/v1";

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
    log::info!("Downloaded {} bytes to {}", bytes.len(), dest_path.display());
    std::fs::write(dest_path, &bytes)?;
    Ok(())
}
