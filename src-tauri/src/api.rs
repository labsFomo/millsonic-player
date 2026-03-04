use serde::Serialize;
use std::path::Path;

const API_BASE: &str = "https://apimillsonic.fo.com.uy/api/v1";

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

#[derive(Serialize)]
struct PairRequest {
    #[serde(rename = "pairingCode")]
    pairing_code: String,
}

pub async fn pair_with_code(code: &str) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client()
        .post(format!("{}/devices/pair", API_BASE))
        .json(&PairRequest { pairing_code: code.to_string() })
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
    let resp = client().get(url).send().await?;
    let bytes = resp.bytes().await?;
    std::fs::write(dest_path, &bytes)?;
    Ok(())
}
