use std::env;
use std::mem::size_of;
use tracing::{debug, info};
use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const APP_NAME: PCWSTR = w!("LumaTray");

/// Checks if LumaTray is registered in HKCU\...\Run.
pub fn is_autostart_enabled() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_READ, &mut hkey).is_ok() {
            let mut data_len: u32 = 0;
            let res = RegQueryValueExW(
                hkey,
                APP_NAME,
                None,
                None,
                None,
                Some(&mut data_len),
            );
            let _ = RegCloseKey(hkey);
            return res.is_ok() && data_len > 0;
        }
    }
    false
}

/// Enables or disables LumaTray autostart on Windows logon.
pub fn set_autostart(enable: bool) -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        let open_res = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            Some(0),
            KEY_WRITE,
            &mut hkey,
        );
        if !open_res.is_ok() {
            return Err(format!("Failed to open Run registry key: {:?}", open_res));
        }

        if enable {
            let current_exe = env::current_exe()
                .map_err(|e| format!("Failed to retrieve executable path: {:?}", e))?;
            let path_str = format!("\"{}\"", current_exe.to_string_lossy());
            let wide_path: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

            let res = RegSetValueExW(
                hkey,
                APP_NAME,
                Some(0),
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    wide_path.as_ptr() as *const u8,
                    wide_path.len() * size_of::<u16>(),
                )),
            );

            let _ = RegCloseKey(hkey);
            if res.is_ok() {
                info!("Enabled 'Start with Windows' in registry: {}", path_str);
                Ok(())
            } else {
                Err(format!("RegSetValueExW failed: {:?}", res))
            }
        } else {
            let res = RegDeleteValueW(hkey, APP_NAME);
            let _ = RegCloseKey(hkey);
            debug!("Removed 'Start with Windows' from registry: {:?}", res);
            Ok(())
        }
    }
}
