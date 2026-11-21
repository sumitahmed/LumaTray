use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    GetDisplayConfigBufferSizes,
    QueryDisplayConfig,
    DisplayConfigGetDeviceInfo,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
use tracing::{debug, warn};

/// Queries the live active resolution (e.g. "1920 × 1080") and refresh rate in Hz (e.g. 100)
/// for a given GDI display device (such as "\\.\DISPLAY1").
pub fn query_gdi_display_mode(gdi_device_name: &str) -> (String, u32) {
    unsafe {
        let mut devmode: DEVMODEW = std::mem::zeroed();
        devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;

        let wide_name: Vec<u16> = gdi_device_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let ok = if gdi_device_name.is_empty() || gdi_device_name == "InternalPanel" {
            EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode).as_bool()
        } else {
            EnumDisplaySettingsW(PCWSTR(wide_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut devmode).as_bool()
        };

        if ok && devmode.dmPelsWidth > 0 && devmode.dmPelsHeight > 0 {
            let res = format!("{} × {}", devmode.dmPelsWidth, devmode.dmPelsHeight);
            let hz = if devmode.dmDisplayFrequency > 1 {
                devmode.dmDisplayFrequency
            } else {
                60
            };
            (res, hz)
        } else {
            ("1920 × 1080".to_string(), 60)
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DisplayConfigTarget {
    pub gdi_device_name: String,   // E.g. "\\.\DISPLAY1"
    pub device_path: String,       // Full persistent device path
    pub device_key: String,        // Substring before "#{..."
    pub friendly_name: String,     // E.g. "DELL U2720Q" or "EK240Y P6"
}

fn u16_slice_to_string(slice: &[u16]) -> String {
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..len])
}

pub fn get_display_config_targets() -> Vec<DisplayConfigTarget> {
    let mut targets = Vec::new();

    unsafe {
        let mut path_count: u32 = 0;
        let mut mode_count: u32 = 0;

        let res = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count);
        if res != WIN32_ERROR(0) || path_count == 0 {
            warn!("GetDisplayConfigBufferSizes failed or no active paths: {:?}", res);
            return targets;
        }

        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![std::mem::zeroed(); path_count as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![std::mem::zeroed(); mode_count as usize];

        let res = QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        );

        if res != WIN32_ERROR(0) {
            warn!("QueryDisplayConfig failed: {:?}", res);
            return targets;
        }

        paths.truncate(path_count as usize);

        for path in paths {
            let mut source_name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = std::mem::zeroed();
            source_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source_name.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source_name.header.adapterId = path.sourceInfo.adapterId;
            source_name.header.id = path.sourceInfo.id;

            if DisplayConfigGetDeviceInfo(&mut source_name.header) != 0 {
                continue;
            }

            let mut target_name: DISPLAYCONFIG_TARGET_DEVICE_NAME = std::mem::zeroed();
            target_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target_name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target_name.header.adapterId = path.targetInfo.adapterId;
            target_name.header.id = path.targetInfo.id;

            if DisplayConfigGetDeviceInfo(&mut target_name.header) != 0 {
                continue;
            }

            let gdi_name = u16_slice_to_string(&source_name.viewGdiDeviceName);
            let dev_path = u16_slice_to_string(&target_name.monitorDevicePath);
            let friendly = u16_slice_to_string(&target_name.monitorFriendlyDeviceName);

            let dev_key = if let Some(idx) = dev_path.find("#{") {
                dev_path[..idx].to_string()
            } else {
                dev_path.clone()
            };

            debug!(
                "Discovered DisplayConfig Target: GDI='{}', Name='{}', Path='{}'",
                gdi_name, friendly, dev_path
            );

            targets.push(DisplayConfigTarget {
                gdi_device_name: gdi_name,
                device_path: dev_path,
                device_key: dev_key,
                friendly_name: friendly,
            });
        }
    }

    targets
}
