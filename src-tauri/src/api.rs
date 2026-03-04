use serde::Serialize;

const API_BASE: &str = "https://apimillsonic.fo.com.uy/api/v1";

#[derive(Serialize)]
struct PairRequest {
    #[serde(rename = "pairingCode")]
    pairing_code: String,
}

pub async fn pair_with_code(code: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/devices/pair", API_BASE))
        .json(&PairRequest { pairing_code: code.to_string() })
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

pub async fn send_telemetry(device_id: &str, device_token: &str, telemetry: &serde_json::Value) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let mut body = telemetry.clone();
    body["deviceToken"] = serde_json::json!(device_token);
    let resp = client
        .post(format!("{}/devices/{}/telemetry", API_BASE, device_id))
        .json(&body)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}

pub async fn sync_device(device_id: &str, device_token: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/devices/{}/sync?deviceToken={}", API_BASE, device_id, device_token))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp)
}
