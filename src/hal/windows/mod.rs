pub mod ddc_ci;
pub mod display_config;
pub mod wmi;

use std::collections::HashMap;
use tracing::info;

use crate::hal::descriptor::{DisplayDescriptor, DriverType};
use crate::hal::error::DriverError;
use crate::hal::DisplayDriver;

use ddc_ci::WindowsDdcCiDriver;
use wmi::WindowsWmiDriver;

pub struct WindowsDisplayManager {
    ddc_driver: WindowsDdcCiDriver,
    wmi_driver: WindowsWmiDriver,
    id_to_driver: HashMap<String, DriverType>,
}

impl WindowsDisplayManager {
    pub fn new() -> Self {
        Self {
            ddc_driver: WindowsDdcCiDriver::new(),
            wmi_driver: WindowsWmiDriver::new(),
            id_to_driver: HashMap::new(),
        }
    }
}

impl DisplayDriver for WindowsDisplayManager {
    fn enumerate(&mut self) -> Result<Vec<DisplayDescriptor>, DriverError> {
        self.id_to_driver.clear();
        let mut all_monitors = Vec::new();

        // 1. External monitors via DDC/CI (with 3-attempt retry sweep)
        for attempt in 1..=3 {
            match self.ddc_driver.enumerate() {
                Ok(ddc_monitors) if !ddc_monitors.is_empty() => {
                    for m in ddc_monitors {
                        self.id_to_driver.insert(m.id.clone(), DriverType::DdcCi);
                        all_monitors.push(m);
                    }
                    break;
                }
                Ok(_) => {
                    if attempt < 3 {
                        std::thread::sleep(std::time::Duration::from_millis(80));
                    }
                }
                Err(e) => {
                    tracing::warn!("DDC/CI enumeration warning (attempt {}): {:?}", attempt, e);
                    if attempt < 3 {
                        std::thread::sleep(std::time::Duration::from_millis(80));
                    }
                }
            }
        }

        // 2. Laptop internal panels via WMI
        let mut wmi_internal_keys = Vec::new();
        match self.wmi_driver.enumerate() {
            Ok(wmi_monitors) => {
                for m in wmi_monitors {
                    if let Some(code) = m.id.split('\\').nth(2) {
                        wmi_internal_keys.push(code.to_uppercase());
                    }
                    self.id_to_driver.insert(m.id.clone(), DriverType::WmiInternal);
                    all_monitors.push(m);
                }
            }
            Err(e) => {
                tracing::warn!("WMI enumeration warning: {:?}", e);
            }
        }

        // Deduplicate: remove any DDC/CI monitor entry that duplicates an internal laptop panel
        all_monitors.retain(|m| {
            if m.driver_type == DriverType::DdcCi {
                let is_internal_dup = wmi_internal_keys.iter().any(|code| m.id.to_uppercase().contains(code));
                if is_internal_dup {
                    self.id_to_driver.remove(&m.id);
                    tracing::debug!("Filtered duplicate internal panel from DDC/CI: {}", m.id);
                    false
                } else {
                    true
                }
            } else {
                true
            }
        });

        info!(
            "Total active displays discovered: {} (DDC/CI + WMI)",
            all_monitors.len()
        );

        Ok(all_monitors)
    }

    fn get_brightness(&mut self, id: &str) -> Result<u32, DriverError> {
        match self.id_to_driver.get(id) {
            Some(DriverType::DdcCi) => self.ddc_driver.get_brightness(id),
            Some(DriverType::WmiInternal) => self.wmi_driver.get_brightness(id),
            _ => Err(DriverError::MonitorNotFound(id.to_string())),
        }
    }

    fn set_brightness(&mut self, id: &str, level: u32) -> Result<(), DriverError> {
        match self.id_to_driver.get(id) {
            Some(DriverType::DdcCi) => self.ddc_driver.set_brightness(id, level),
            Some(DriverType::WmiInternal) => self.wmi_driver.set_brightness(id, level),
            _ => Err(DriverError::MonitorNotFound(id.to_string())),
        }
    }

    fn get_contrast(&mut self, id: &str) -> Result<u32, DriverError> {
        match self.id_to_driver.get(id) {
            Some(DriverType::DdcCi) => self.ddc_driver.get_contrast(id),
            Some(DriverType::WmiInternal) => {
                Err(DriverError::Unsupported("WMI does not support hardware contrast".into()))
            }
            _ => Err(DriverError::MonitorNotFound(id.to_string())),
        }
    }

    fn set_contrast(&mut self, id: &str, level: u32) -> Result<(), DriverError> {
        match self.id_to_driver.get(id) {
            Some(DriverType::DdcCi) => self.ddc_driver.set_contrast(id, level),
            Some(DriverType::WmiInternal) => {
                Err(DriverError::Unsupported("WMI does not support hardware contrast".into()))
            }
            _ => Err(DriverError::MonitorNotFound(id.to_string())),
        }
    }

    fn get_vcp(&mut self, id: &str, vcp_code: u8) -> Result<(u32, u32), DriverError> {
        match self.id_to_driver.get(id) {
            Some(DriverType::DdcCi) => self.ddc_driver.get_vcp(id, vcp_code),
            Some(DriverType::WmiInternal) => {
                Err(DriverError::Unsupported("WMI does not support arbitrary VCP codes".into()))
            }
            _ => Err(DriverError::MonitorNotFound(id.to_string())),
        }
    }

    fn set_vcp(&mut self, id: &str, vcp_code: u8, value: u32) -> Result<(), DriverError> {
        match self.id_to_driver.get(id) {
            Some(DriverType::DdcCi) => self.ddc_driver.set_vcp(id, vcp_code, value),
            Some(DriverType::WmiInternal) => {
                Err(DriverError::Unsupported("WMI does not support arbitrary VCP codes".into()))
            }
            _ => Err(DriverError::MonitorNotFound(id.to_string())),
        }
    }
}
