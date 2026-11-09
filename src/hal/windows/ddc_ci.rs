use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use windows::core::BOOL;
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor,
    GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply,
    SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{HANDLE, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors,
    GetMonitorInfoW,
    HDC,
    HMONITOR,
    MONITORINFO,
    MONITORINFOEXW,
};

use crate::hal::descriptor::{DisplayCapabilities, DisplayDescriptor, DriverType};
use crate::hal::error::DriverError;
use super::display_config::{get_display_config_targets, DisplayConfigTarget};

pub struct SafePhysicalHandle(pub HANDLE);

unsafe impl Send for SafePhysicalHandle {}
unsafe impl Sync for SafePhysicalHandle {}

impl Drop for SafePhysicalHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DestroyPhysicalMonitor(self.0);
            }
        }
    }
}

pub struct PhysicalDisplayEntry {
    pub descriptor: DisplayDescriptor,
    pub handle: Arc<SafePhysicalHandle>,
}

pub struct WindowsDdcCiDriver {
    monitors: HashMap<String, PhysicalDisplayEntry>,
}

fn is_transient_ddc_error(code: u32) -> bool {
    matches!(
        code,
        0xC0262580..=0xC0262585 // DDC/CI transmission, timing, checksum, length errors
            | 0x0000001F        // ERROR_GEN_FAILURE
            | 0x000004DF        // ERROR_BUSY
    )
}

fn try_ddc_op<F>(mut op: F) -> Result<(), DriverError>
where
    F: FnMut() -> windows::core::Result<()>,
{
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = None;

    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        match op() {
            Ok(()) => return Ok(()),
            Err(e) => {
                let code = e.code().0 as u32;
                if !is_transient_ddc_error(code) {
                    return Err(DriverError::Win32 {
                        code,
                        message: e.message().to_string(),
                    });
                }
                debug!(
                    "Transient DDC/CI error (attempt #{}/{}): {:#010X} ({})",
                    attempt,
                    MAX_ATTEMPTS,
                    code,
                    e.message()
                );
                last_err = Some(e);
            }
        }
    }

    let err = last_err.unwrap();
    Err(DriverError::DdcTimeout(format!(
        "DDC/CI call failed after 3 transient retries: {:#010X} ({})",
        err.code().0,
        err.message()
    )))
}

#[allow(dead_code)]
impl WindowsDdcCiDriver {
    pub fn new() -> Self {
        Self {
            monitors: HashMap::new(),
        }
    }

    pub fn enumerate(&mut self) -> Result<Vec<DisplayDescriptor>, DriverError> {
        self.monitors.clear();

        // 1. Query DisplayConfig active targets for high-fidelity permanent paths & names
        let targets = get_display_config_targets();

        // 2. Enumerate active GDI monitors
        let hmonitors = Arc::new(Mutex::new(Vec::new()));
        let hmonitors_clone = Arc::clone(&hmonitors);

        unsafe extern "system" fn enum_proc(
            hmonitor: HMONITOR,
            _hdc: HDC,
            _lprect: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            let list = unsafe { &*(lparam.0 as *const Mutex<Vec<HMONITOR>>) };
            list.lock().push(hmonitor);
            BOOL(1)
        }

        unsafe {
            let lparam = LPARAM(&*hmonitors_clone as *const _ as isize);
            let _ = EnumDisplayMonitors(None, None, Some(enum_proc), lparam);
        }

        let monitor_list = hmonitors.lock().clone();
        let mut results = Vec::new();

        for hmonitor in monitor_list {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

            let gdi_name = unsafe {
                if GetMonitorInfoW(hmonitor, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
                    let len = mi
                        .szDevice
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(mi.szDevice.len());
                    String::from_utf16_lossy(&mi.szDevice[..len])
                } else {
                    continue;
                }
            };

            let mut num_physical = 0u32;
            let mut count_ok = false;
            for _attempt in 1..=3 {
                let count_res = unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut num_physical) };
                if count_res.is_ok() && num_physical > 0 {
                    count_ok = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            if !count_ok || num_physical == 0 {
                debug!("Skipping '{}': no physical monitors after 3 attempts", gdi_name);
                continue;
            }

            let mut phys_monitors: Vec<PHYSICAL_MONITOR> = vec![unsafe { std::mem::zeroed() }; num_physical as usize];
            let mut phys_ok = false;
            for _attempt in 1..=3 {
                let phys_res = unsafe { GetPhysicalMonitorsFromHMONITOR(hmonitor, &mut phys_monitors) };
                if phys_res.is_ok() {
                    phys_ok = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            if !phys_ok {
                warn!("Couldn't get physical monitors for '{}' after 3 attempts", gdi_name);
                continue;
            }

            // Find matching display targets for this GDI device name
            let matched_targets: Vec<&DisplayConfigTarget> = targets
                .iter()
                .filter(|t| t.gdi_device_name.eq_ignore_ascii_case(&gdi_name))
                .collect();

            for (idx, phys) in phys_monitors.into_iter().enumerate() {
                let handle = Arc::new(SafePhysicalHandle(phys.hPhysicalMonitor));

                let desc_chars = phys.szPhysicalMonitorDescription;
                let desc_len = desc_chars
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc_chars.len());
                let phys_desc = String::from_utf16_lossy(&desc_chars[..desc_len]);

                // Construct a permanent, stable ID
                let (target_id, friendly_name) = if let Some(target) = matched_targets.get(idx) {
                    (target.device_path.clone(), target.friendly_name.clone())
                } else if let Some(first_target) = matched_targets.first() {
                    (
                        format!("{}#Monitor{}", first_target.device_path, idx),
                        first_target.friendly_name.clone(),
                    )
                } else {
                    (format!("{}\\Monitor{}", gdi_name, idx), phys_desc.clone())
                };

                let display_name = if !friendly_name.is_empty() {
                    friendly_name
                } else if !phys_desc.is_empty() {
                    phys_desc
                } else {
                    format!("External Display ({})", gdi_name)
                };

                // Test DDC/CI capabilities: determine which VCP code works for brightness
                let raw_handle = handle.0;
                let mut brightness_vcp: Option<u8> = None;
                let mut current_bright = 50u32;
                let mut max_bright = 100u32;

                // Probe order: 0x10 (Standard Luminance), 0x13 (Backlight Control), 0x6B (Backlight White)
                for code in [0x10u8, 0x13u8, 0x6Bu8] {
                    for attempt in 1..=3 {
                        if attempt > 1 {
                            std::thread::sleep(std::time::Duration::from_millis(40));
                        }
                        let mut cur = 0u32;
                        let mut max = 0u32;
                        let ok = unsafe {
                            GetVCPFeatureAndVCPFeatureReply(
                                raw_handle,
                                code,
                                None,
                                &mut cur,
                                Some(&mut max),
                            )
                        } != 0;

                        if ok {
                            brightness_vcp = Some(code);
                            current_bright = cur;
                            max_bright = if max > 0 { max } else { 100 };
                            break;
                        }
                    }
                    if brightness_vcp.is_some() {
                        break;
                    }
                }

                // If VCP probe failed all retries but physical monitor handle exists,
                // do NOT drop the external monitor! Fallback to standard VCP 0x10 so the user
                // can still control brightness!
                let vcp_code = brightness_vcp.unwrap_or(0x10);

                // Probe contrast (0x12)
                let mut cur_contrast = 0u32;
                let mut max_contrast = 0u32;
                let supports_contrast = unsafe {
                    GetVCPFeatureAndVCPFeatureReply(
                        raw_handle,
                        0x12,
                        None,
                        &mut cur_contrast,
                        Some(&mut max_contrast),
                    )
                } != 0;

                let capabilities = DisplayCapabilities {
                    min_brightness: 0,
                    max_brightness: max_bright,
                    current_brightness: current_bright,
                    supports_contrast,
                    min_contrast: if supports_contrast { Some(0) } else { None },
                    max_contrast: if supports_contrast { Some(max_contrast) } else { None },
                    current_contrast: if supports_contrast { Some(cur_contrast) } else { None },
                    vcp_brightness_code: vcp_code,
                    supported_vcp_codes: vec![vcp_code],
                };

                let descriptor = DisplayDescriptor {
                    id: target_id.clone(),
                    name: display_name,
                    gdi_name: gdi_name.clone(),
                    driver_type: DriverType::DdcCi,
                    is_internal: false,
                    capabilities,
                };

                info!(
                    "Registered DDC/CI monitor: '{}' [ID: {}] (VCP {:#04X}, Brightness: {}/{})",
                    descriptor.name, descriptor.id, vcp_code, current_bright, max_bright
                );

                self.monitors.insert(
                    target_id.clone(),
                    PhysicalDisplayEntry {
                        descriptor: descriptor.clone(),
                        handle,
                    },
                );

                results.push(descriptor);
            }
        }

        Ok(results)
    }

    pub fn get_brightness(&mut self, id: &str) -> Result<u32, DriverError> {
        let entry = self
            .monitors
            .get(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        let handle = entry.handle.0;
        let vcp = entry.descriptor.capabilities.vcp_brightness_code;

        let mut current = 0u32;
        let mut max = 0u32;

        try_ddc_op(|| {
            let ok = unsafe {
                GetVCPFeatureAndVCPFeatureReply(handle, vcp, None, &mut current, Some(&mut max))
            };
            if ok != 0 {
                Ok(())
            } else {
                Err(windows::core::Error::from_thread())
            }
        })?;

        Ok(current)
    }

    pub fn set_brightness(&mut self, id: &str, level: u32) -> Result<(), DriverError> {
        let entry = self
            .monitors
            .get_mut(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        let handle = entry.handle.0;
        let vcp = entry.descriptor.capabilities.vcp_brightness_code;
        let clamped = level.min(entry.descriptor.capabilities.max_brightness);

        try_ddc_op(|| {
            let ok = unsafe { SetVCPFeature(handle, vcp, clamped) };
            if ok != 0 {
                Ok(())
            } else {
                Err(windows::core::Error::from_thread())
            }
        })?;

        entry.descriptor.capabilities.current_brightness = clamped;
        Ok(())
    }

    pub fn get_contrast(&mut self, id: &str) -> Result<u32, DriverError> {
        let entry = self
            .monitors
            .get(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        if !entry.descriptor.capabilities.supports_contrast {
            return Err(DriverError::Unsupported("Contrast is not supported".into()));
        }

        let handle = entry.handle.0;
        let mut current = 0u32;
        let mut max = 0u32;

        try_ddc_op(|| {
            let ok = unsafe {
                GetVCPFeatureAndVCPFeatureReply(handle, 0x12, None, &mut current, Some(&mut max))
            };
            if ok != 0 {
                Ok(())
            } else {
                Err(windows::core::Error::from_thread())
            }
        })?;

        Ok(current)
    }

    pub fn set_contrast(&mut self, id: &str, level: u32) -> Result<(), DriverError> {
        let entry = self
            .monitors
            .get_mut(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        if !entry.descriptor.capabilities.supports_contrast {
            return Err(DriverError::Unsupported("Contrast is not supported".into()));
        }

        let handle = entry.handle.0;
        let max_val = entry.descriptor.capabilities.max_contrast.unwrap_or(100);
        let clamped = level.min(max_val);

        try_ddc_op(|| {
            let ok = unsafe { SetVCPFeature(handle, 0x12, clamped) };
            if ok != 0 {
                Ok(())
            } else {
                Err(windows::core::Error::from_thread())
            }
        })?;

        entry.descriptor.capabilities.current_contrast = Some(clamped);
        Ok(())
    }

    pub fn get_vcp(&mut self, id: &str, vcp_code: u8) -> Result<(u32, u32), DriverError> {
        let entry = self
            .monitors
            .get(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        let handle = entry.handle.0;
        let mut current = 0u32;
        let mut max = 0u32;

        try_ddc_op(|| {
            let ok = unsafe {
                GetVCPFeatureAndVCPFeatureReply(handle, vcp_code, None, &mut current, Some(&mut max))
            };
            if ok != 0 {
                Ok(())
            } else {
                Err(windows::core::Error::from_thread())
            }
        })?;

        Ok((current, max))
    }

    pub fn set_vcp(&mut self, id: &str, vcp_code: u8, value: u32) -> Result<(), DriverError> {
        let entry = self
            .monitors
            .get(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        let handle = entry.handle.0;
        try_ddc_op(|| {
            let ok = unsafe { SetVCPFeature(handle, vcp_code, value) };
            if ok != 0 {
                Ok(())
            } else {
                Err(windows::core::Error::from_thread())
            }
        })?;

        Ok(())
    }
}
