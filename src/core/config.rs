use std::collections::HashMap;
use std::fs::{create_dir_all, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use super::sync_engine::PiecewiseCurve;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub friendly_name: Option<String>,
    pub is_linked: bool,
    pub min_brightness: u32,
    pub max_brightness: u32,
    pub custom_curve: Option<PiecewiseCurve>,
    pub show_contrast: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            friendly_name: None,
            is_linked: true,
            min_brightness: 0,
            max_brightness: 100,
            custom_curve: None,
            show_contrast: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub start_with_windows: bool,
    pub sync_enabled: bool,
    pub master_brightness: u32,
    pub throttle_ms: u64,
    pub hotkeys_enabled: bool,
    pub displays: HashMap<String, DisplayConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            start_with_windows: false,
            sync_enabled: false,
            master_brightness: 75,
            throttle_ms: 50,
            hotkeys_enabled: true,
            displays: HashMap::new(),
        }
    }
}

pub struct ConfigManager {
    config_path: PathBuf,
}

impl ConfigManager {
    pub fn new() -> Self {
        let path = Self::default_config_path();
        Self { config_path: path }
    }

    pub fn default_config_path() -> PathBuf {
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        appdata.join("LumaTray").join("config.json")
    }

    /// Loads configuration from disk, or creates default if not found.
    pub fn load(&self) -> AppConfig {
        if !self.config_path.exists() {
            debug!("Config file not found at {:?}, using defaults", self.config_path);
            let default_cfg = AppConfig::default();
            self.save(&default_cfg);
            return default_cfg;
        }

        match File::open(&self.config_path) {
            Ok(mut file) => {
                let mut content = String::new();
                if file.read_to_string(&mut content).is_ok() {
                    match serde_json::from_str::<AppConfig>(&content) {
                        Ok(config) => {
                            info!("Loaded configuration from {:?}", self.config_path);
                            return config;
                        }
                        Err(e) => {
                            warn!("Corrupted config file: {:?}. Backing up and resetting.", e);
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to open config file: {:?}", e);
            }
        }

        let default_cfg = AppConfig::default();
        self.save(&default_cfg);
        default_cfg
    }

    /// Saves configuration to disk atomically.
    pub fn save(&self, config: &AppConfig) {
        if let Some(parent) = self.config_path.parent() {
            if !parent.exists() {
                let _ = create_dir_all(parent);
            }
        }

        match serde_json::to_string_pretty(config) {
            Ok(json) => {
                match File::create(&self.config_path) {
                    Ok(mut file) => {
                        if let Err(e) = file.write_all(json.as_bytes()) {
                            error!("Failed to write config file: {:?}", e);
                        } else {
                            debug!("Saved configuration to {:?}", self.config_path);
                        }
                    }
                    Err(e) => {
                        error!("Failed to create config file: {:?}", e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to serialize config: {:?}", e);
            }
        }
    }
}
