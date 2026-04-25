use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Stable WASAPI device ID for the input mic. Empty = default.
    #[serde(default)]
    pub input_device_id: String,
    /// Stable WASAPI device ID for the output target (VB-Cable Input).
    /// Empty = auto-detect by friendly name.
    #[serde(default)]
    pub output_device_id: String,
    /// Master enable. When false, the pipeline runs in bypass mode (passes
    /// audio through without DSP) so toggling is instant.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Attenuation limit in dB for DeepFilterNet. 6.0 = subtle, 100.0 = max.
    #[serde(default = "default_atten")]
    pub attenuation_db: f32,
    /// Auto-start at user login.
    #[serde(default)]
    pub auto_start: bool,
}

fn default_true() -> bool { true }
fn default_atten() -> f32 { 100.0 }

impl Default for Config {
    fn default() -> Self {
        Self {
            input_device_id: String::new(),
            output_device_id: String::new(),
            enabled: true,
            attenuation_db: default_atten(),
            auto_start: false,
        }
    }
}

impl Config {
    pub fn load_or_default() -> Self {
        match Self::load() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "config load failed; using defaults");
                Self::default()
            }
        }
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        let cfg = toml::from_str(&text)?;
        Ok(cfg)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        Ok(())
    }
}

pub fn config_path() -> PathBuf {
    base_dir().join("config.toml")
}

pub fn log_dir() -> PathBuf {
    base_dir().join("logs")
}

fn base_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("NoiseGate")
}
