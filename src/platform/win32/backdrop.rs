use std::mem::size_of;
use tracing::{debug, warn};
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute,
    DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE,
    DWM_SYSTEMBACKDROP_TYPE,
    DWM_WINDOW_CORNER_PREFERENCE,
    DWMSBT_MAINWINDOW,
    DWMSBT_TRANSIENTWINDOW,
    DWMWCP_ROUND,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    LR_SHARED, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackdropType {
    Mica,
    Acrylic,
    None,
}

/// Detects if the user's Windows system theme is Dark Mode.
pub fn is_system_in_dark_mode() -> bool {
    // Check registry: HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
        };

        let mut hkey = HKEY::default();
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, Some(0), KEY_READ, &mut hkey).is_ok() {
            let mut data: u32 = 0;
            let mut data_len = size_of::<u32>() as u32;
            let mut data_type = REG_DWORD;

            let res = RegQueryValueExW(
                hkey,
                w!("AppsUseLightTheme"),
                None,
                Some(&mut data_type),
                Some(&mut data as *mut _ as *mut _),
                Some(&mut data_len),
            );

            let _ = RegCloseKey(hkey);

            if res.is_ok() {
                return data == 0; // 0 = Dark mode, 1 = Light mode
            }
        }
    }
    true // Default to Dark mode for sleek aesthetics
}

/// Assigns the embedded LumaTray executable icon (Resource ID 1) to a window's titlebar and taskbar slot.
pub fn apply_window_icon(hwnd: HWND) {
    unsafe {
        if let Ok(hinstance) = GetModuleHandleW(None) {
            let cx_sm = GetSystemMetrics(SM_CXSMICON);
            let cy_sm = GetSystemMetrics(SM_CYSMICON);
            if let Ok(hicon_sm) = LoadImageW(
                Some(HINSTANCE(hinstance.0)),
                PCWSTR(1 as *const u16),
                IMAGE_ICON,
                cx_sm,
                cy_sm,
                LR_DEFAULTCOLOR | LR_SHARED,
            ) {
                let _ = SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(ICON_SMALL as usize)),
                    Some(LPARAM(hicon_sm.0 as isize)),
                );
            }

            let cx_big = GetSystemMetrics(SM_CXICON);
            let cy_big = GetSystemMetrics(SM_CYICON);
            if let Ok(hicon_big) = LoadImageW(
                Some(HINSTANCE(hinstance.0)),
                PCWSTR(1 as *const u16),
                IMAGE_ICON,
                cx_big,
                cy_big,
                LR_DEFAULTCOLOR | LR_SHARED,
            ) {
                let _ = SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(ICON_BIG as usize)),
                    Some(LPARAM(hicon_big.0 as isize)),
                );
            }
        }
    }
}

/// Applies Windows 11 DWM Mica or Acrylic backdrop, rounded corners, and embedded window icon.
pub fn apply_fluent_backdrop(hwnd: HWND, backdrop: BackdropType, dark_mode: bool) {
    apply_window_icon(hwnd);
    unsafe {
        // 1. Dark Mode Immersive Theme
        let dark_val = BOOL(if dark_mode { 1 } else { 0 });
        let res_dark = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_val as *const _ as *const _,
            size_of::<BOOL>() as u32,
        );
        if res_dark.is_err() {
            debug!("DWMWA_USE_IMMERSIVE_DARK_MODE failed or not supported on this Windows build");
        }

        // 2. Windows 11 Rounded Corners (DWMWCP_ROUND)
        let round_val: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        let res_corner = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round_val as *const _ as *const _,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        if res_corner.is_err() {
            debug!("DWMWA_WINDOW_CORNER_PREFERENCE not supported on this Windows build");
        }

        // 3. System Backdrop (Acrylic / Mica)
        let backdrop_type: DWM_SYSTEMBACKDROP_TYPE = match backdrop {
            BackdropType::Acrylic => DWMSBT_TRANSIENTWINDOW,
            BackdropType::Mica => DWMSBT_MAINWINDOW,
            BackdropType::None => return,
        };

        let res_backdrop = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop_type as *const _ as *const _,
            size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        );

        if res_backdrop.is_err() {
            warn!("DWMWA_SYSTEMBACKDROP_TYPE could not be applied (requires Windows 11 22H2+)");
        } else {
            debug!("Successfully applied Windows 11 Fluent backdrop: {:?}", backdrop);
        }
    }
}

/// Trims the active working set of the process using the Windows kernel memory manager.
/// Flushes unreferenced startup pages to achieve the leanest possible background RAM footprint.
pub fn trim_working_set() {
    unsafe {
        unsafe extern "system" {
            fn SetProcessWorkingSetSize(
                hprocess: windows::Win32::Foundation::HANDLE,
                dwminimumworkingsetsize: usize,
                dwmaximumworkingsetsize: usize,
            ) -> windows::core::BOOL;
            fn GetCurrentProcess() -> windows::Win32::Foundation::HANDLE;
        }
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}
