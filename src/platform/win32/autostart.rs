use std::env;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use tracing::{debug, info};
use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

use super::installer::installed_exe_path;

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const STARTUP_APPROVED_SUBKEY: PCWSTR =
    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
const APP_NAME: PCWSTR = w!("LumaTray");

/// Returns the permanent installed executable path if available, otherwise current_exe.
pub fn preferred_autostart_exe() -> Result<PathBuf, String> {
    let installed = installed_exe_path();
    if installed.exists() {
        return Ok(installed);
    }
    env::current_exe().map_err(|e| format!("Failed to retrieve executable path: {:?}", e))
}

/// Checks if LumaTray is registered in HKCU\...\Run AND points to a valid existing executable on disk.
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

            if res.is_ok() && data_len > 2 {
                let mut buf = vec![0u16; (data_len as usize / 2) + 1];
                let res2 = RegQueryValueExW(
                    hkey,
                    APP_NAME,
                    None,
                    None,
                    Some(buf.as_mut_ptr() as *mut u8),
                    Some(&mut data_len),
                );
                let _ = RegCloseKey(hkey);

                if res2.is_ok() {
                    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    let raw = String::from_utf16_lossy(&buf[..len]).trim().to_string();
                    let cleaned = raw.trim_matches('"');
                    return !cleaned.is_empty() && Path::new(cleaned).exists();
                }
                return false;
            }
            let _ = RegCloseKey(hkey);
        }
    }
    false
}

/// Checks if any `LumaTray` value exists in HKCU\...\Run (even if pointing to an old moved path).
pub fn has_any_autostart_registry_entry() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_READ, &mut hkey).is_ok() {
            let mut data_len: u32 = 0;
            let res = RegQueryValueExW(hkey, APP_NAME, None, None, None, Some(&mut data_len));
            let _ = RegCloseKey(hkey);
            return res.is_ok() && data_len > 2;
        }
    }
    false
}

/// Self-heals the Windows Startup registry entry on boot so moved or cleaned builds never break autostart.
/// Returns true if autostart is enabled (either via config or an existing registry entry that was repaired).
pub fn sync_autostart_on_boot(should_enable: bool) -> bool {
    if should_enable || has_any_autostart_registry_entry() {
        let _ = set_autostart(true);
        true
    } else {
        false
    }
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
            let target_exe = preferred_autostart_exe()?;
            let path_str = format!("\"{}\"", target_exe.to_string_lossy());
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

            // Also ensure Windows Task Manager StartupApproved marks LumaTray as enabled (0x02)
            let mut hkey_approved = HKEY::default();
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                STARTUP_APPROVED_SUBKEY,
                Some(0),
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hkey_approved,
                None,
            )
            .is_ok()
            {
                let approved_bytes: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                let _ = RegSetValueExW(
                    hkey_approved,
                    APP_NAME,
                    Some(0),
                    REG_BINARY,
                    Some(&approved_bytes),
                );
                let _ = RegCloseKey(hkey_approved);
            }

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
