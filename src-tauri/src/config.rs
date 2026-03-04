use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    pub device_id: Option<String>,
    pub device_token: Option<String>,
    pub zone_id: Option<String>,
    pub zone_name: Option<String>,
    pub volume: u8,
    pub stream_quality: u16,
    pub paired: bool,
    pub hardware_id: Option<String>,
    pub timezone: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            device_token: None,
            zone_id: None,
            zone_name: None,
            volume: 80,
            stream_quality: 128,
            paired: false,
            hardware_id: None,
            timezone: None,
        }
    }
}

static CONFIG: OnceLock<Mutex<AppConfig>> = OnceLock::new();

pub fn global() -> &'static Mutex<AppConfig> {
    CONFIG.get_or_init(|| Mutex::new(AppConfig::load_from_disk()))
}

impl AppConfig {
    fn load_from_disk() -> Self {
        let path = Self::config_path();
        if path.exists() {
            let data = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn load() -> Self {
        global().lock().unwrap().clone()
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn is_paired(&self) -> bool {
        self.device_id.is_some() && self.device_token.is_some()
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Millsonic")
            .join("config.json")
    }

    pub fn data_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Millsonic")
    }

    pub fn cache_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Millsonic")
            .join("cache")
            .join("tracks")
    }

    pub fn update_and_save<F: FnOnce(&mut AppConfig)>(f: F) -> Result<(), String> {
        let mut cfg = global().lock().map_err(|e| e.to_string())?;
        f(&mut cfg);
        cfg.save().map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub fn get_config() -> AppConfig {
    AppConfig::load()
}

pub fn update_and_save_global<F: FnOnce(&mut AppConfig)>(f: F) {
    let _ = AppConfig::update_and_save(f);
}
