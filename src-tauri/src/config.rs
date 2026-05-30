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
    pub unpair_pin: Option<String>,
    pub crossfade_enabled: bool,
    pub crossfade_duration: u32,
    pub location_name: Option<String>,
    pub debug_mode: bool,
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
            unpair_pin: None,
            crossfade_enabled: false,
            crossfade_duration: 3,
            location_name: None,
            debug_mode: false,
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
        let parse = |p: &PathBuf| -> Option<Self> {
            let data = std::fs::read_to_string(p).ok()?;
            serde_json::from_str(&data).ok()
        };
        if let Some(cfg) = parse(&path) {
            return cfg;
        }
        // R-17: primary config missing/corrupt — fall back to the last good
        // backup before giving up (giving up = losing the pairing).
        let bak = path.with_extension("json.bak");
        if let Some(cfg) = parse(&bak) {
            eprintln!("[millsonic] config.json unreadable — restored from config.json.bak");
            return cfg;
        }
        Self::default()
    }

    pub fn load() -> Self {
        global().lock().unwrap().clone()
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // R-17: atomic write + keep a backup of the last good config so a crash
        // or full disk mid-write can't corrupt it and lose the pairing.
        if let Ok(cur) = std::fs::read_to_string(&path) {
            if serde_json::from_str::<AppConfig>(&cur).is_ok() {
                let _ = std::fs::copy(&path, path.with_extension("json.bak"));
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
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

#[cfg(test)]
mod config_backup_tests {
    use super::*;

    // R-17: a corrupt config.json must be recovered from config.json.bak so the
    // device never loses its pairing. Isolated via a temp XDG_CONFIG_HOME.
    #[test]
    fn restores_pairing_from_backup_when_main_is_corrupt() {
        let tmp = std::env::temp_dir().join("ms_cfg_backup_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        // First good save → writes config.json (device_id = dev-1).
        let mut c = AppConfig::default();
        c.device_id = Some("dev-1".to_string());
        c.save().unwrap();
        // Second save → backs up the good config.json to .bak, then writes new.
        c.zone_id = Some("zone-1".to_string());
        c.save().unwrap();

        // Corrupt the primary config.
        std::fs::write(AppConfig::config_path(), b"{ this is not valid json").unwrap();

        // Recovery: must come back with the pairing intact (from .bak).
        let loaded = AppConfig::load_from_disk();
        assert_eq!(loaded.device_id.as_deref(), Some("dev-1"));

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
