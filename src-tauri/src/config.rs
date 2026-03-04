use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub device_id: Option<String>,
    pub device_token: Option<String>,
    pub zone_id: Option<String>,
    pub volume: u8,
    pub stream_quality: u16,
}

impl AppConfig {
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            let data = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Self { volume: 80, stream_quality: 128, ..Default::default() }
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Millsonic")
            .join("config.json")
    }
}
