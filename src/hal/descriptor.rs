use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverType {
    DdcCi,
    WmiInternal,
    SoftwareGamma,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayCapabilities {
    pub min_brightness: u32,
    pub max_brightness: u32,
    pub current_brightness: u32,
    pub supports_contrast: bool,
    pub min_contrast: Option<u32>,
    pub max_contrast: Option<u32>,
    pub current_contrast: Option<u32>,
    pub vcp_brightness_code: u8,
    pub supported_vcp_codes: Vec<u8>,
}

impl Default for DisplayCapabilities {
    fn default() -> Self {
        Self {
            min_brightness: 0,
            max_brightness: 100,
            current_brightness: 50,
            supports_contrast: false,
            min_contrast: None,
            max_contrast: None,
            current_contrast: None,
            vcp_brightness_code: 0x10,
            supported_vcp_codes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayDescriptor {
    /// Permanent hardware instance ID / device path (e.g. from QueryDisplayConfig)
    pub id: String,
    /// User-friendly monitor name (e.g. "DELL U2720Q", "EK240Y P6", "Internal Display")
    pub name: String,
    /// GDI device name (e.g. "\\.\DISPLAY1")
    pub gdi_name: String,
    /// Native resolution string (e.g. "1920 × 1080")
    #[serde(default = "default_resolution")]
    pub resolution: String,
    /// Active refresh rate in Hz (e.g. 60, 100, 144)
    #[serde(default = "default_refresh_rate")]
    pub refresh_rate_hz: u32,
    /// Communication driver type used
    pub driver_type: DriverType,
    /// Whether this is a laptop internal display
    pub is_internal: bool,
    /// Hardware capabilities and current levels
    pub capabilities: DisplayCapabilities,
}

fn default_resolution() -> String {
    "1920 × 1080".to_string()
}

fn default_refresh_rate() -> u32 {
    60
}
