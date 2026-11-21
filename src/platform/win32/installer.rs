use std::env;
use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellExecuteW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Returns the canonical Windows per-user installation path:
/// `%LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe`
pub fn installed_exe_path() -> PathBuf {
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        PathBuf::from(local_app_data)
            .join("Programs")
            .join("LumaTray")
            .join("LumaTray.exe")
    } else {
        env::current_exe().unwrap_or_else(|_| PathBuf::from("LumaTray.exe"))
    }
}

/// Returns the Windows Start Menu shortcut path so Windows Search indexes LumaTray as an "App":
/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\LumaTray.lnk`
pub fn start_menu_shortcut_path() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("LumaTray.lnk")
    })
}

/// Ensures LumaTray is installed in `%LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe`,
/// creates the Windows Start Menu `.lnk` App shortcut, and registers the App in Windows Registry.
/// Returns the active canonical executable path for autostart registration.
pub fn ensure_installed() -> PathBuf {
    let current_exe = match env::current_exe() {
        Ok(p) => p,
        Err(_) => return installed_exe_path(),
    };

    let target_exe = installed_exe_path();

    // 1. Copy executable to %LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe if running from elsewhere
    let active_exe = if paths_equal(&current_exe, &target_exe) {
        target_exe.clone()
    } else {
        if let Some(parent) = target_exe.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let needs_copy = match (fs::metadata(&current_exe), fs::metadata(&target_exe)) {
            (Ok(src_meta), Ok(dst_meta)) => {
                src_meta.len() != dst_meta.len()
                    || src_meta.modified().ok() > dst_meta.modified().ok()
            }
            (Ok(_), Err(_)) => true,
            _ => false,
        };

        if needs_copy {
            match fs::copy(&current_exe, &target_exe) {
                Ok(_) => {
                    info!(
                        "Installed LumaTray binary to standard Windows App path: {:?}",
                        target_exe
                    );
                    target_exe.clone()
                }
                Err(e) => {
                    debug!(
                        "Could not overwrite {:?} (may already be running): {:?}. Using existing.",
                        target_exe, e
                    );
                    if target_exe.exists() {
                        target_exe.clone()
                    } else {
                        current_exe.clone()
                    }
                }
            }
        } else {
            target_exe.clone()
        }
    };

    // 2. Create or update the Windows Start Menu shortcut (.lnk) so Windows Search shows "LumaTray (App)"
    if let Some(lnk_path) = start_menu_shortcut_path() {
        if let Err(e) = create_start_menu_shortcut(&active_exe, &lnk_path) {
            warn!("Failed to create Start Menu shortcut: {}", e);
        }
    }

    // 3. Register in HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\LumaTray
    register_uninstall_metadata(&active_exe);

    active_exe
}

/// Returns true if the current process is executing directly from the canonical
/// `%LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe` installation directory.
pub fn is_running_from_install_dir() -> bool {
    if let Ok(curr) = env::current_exe() {
        paths_equal(&curr, &installed_exe_path())
    } else {
        false
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Creates a native Windows Shell Link (`.lnk`) using COM `IShellLinkW` and `IPersistFile`.
pub fn create_start_menu_shortcut(target_exe: &Path, lnk_path: &Path) -> Result<(), String> {
    if let Some(parent) = lnk_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let shell_link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("CoCreateInstance(ShellLink) failed: {:?}", e))?;

        let exe_wide: Vec<u16> = target_exe
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        shell_link
            .SetPath(PCWSTR(exe_wide.as_ptr()))
            .map_err(|e| format!("IShellLinkW::SetPath failed: {:?}", e))?;

        if let Some(work_dir) = target_exe.parent() {
            let dir_wide: Vec<u16> = work_dir
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let _ = shell_link.SetWorkingDirectory(PCWSTR(dir_wide.as_ptr()));
        }

        let _ = shell_link.SetDescription(w!(
            "LumaTray - Ultra-lightweight native monitor brightness controller"
        ));
        let _ = shell_link.SetIconLocation(PCWSTR(exe_wide.as_ptr()), 0);

        let persist_file: IPersistFile = shell_link
            .cast()
            .map_err(|e| format!("IShellLinkW::cast::<IPersistFile> failed: {:?}", e))?;

        let lnk_wide: Vec<u16> = lnk_path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        persist_file
            .Save(PCWSTR(lnk_wide.as_ptr()), true)
            .map_err(|e| format!("IPersistFile::Save failed: {:?}", e))?;

        info!("Registered Windows Start Menu App shortcut at {:?}", lnk_path);
    }

    Ok(())
}

fn set_reg_sz(hkey: HKEY, name: PCWSTR, value: &str) {
    unsafe {
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = RegSetValueExW(
            hkey,
            name,
            Some(0),
            REG_SZ,
            Some(std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * size_of::<u16>(),
            )),
        );
    }
}

fn set_reg_dword(hkey: HKEY, name: PCWSTR, value: u32) {
    unsafe {
        let _ = RegSetValueExW(
            hkey,
            name,
            Some(0),
            REG_DWORD,
            Some(std::slice::from_raw_parts(
                &value as *const u32 as *const u8,
                size_of::<u32>(),
            )),
        );
    }
}

fn register_uninstall_metadata(target_exe: &Path) {
    unsafe {
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\LumaTray");
        let mut hkey = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey,
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .is_ok()
        {
            let exe_str = target_exe.to_string_lossy();
            let icon_str = format!("{},0", exe_str);
            let install_loc = target_exe
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            set_reg_sz(hkey, w!("DisplayName"), "LumaTray");
            set_reg_sz(hkey, w!("DisplayVersion"), "0.2.0");
            set_reg_sz(hkey, w!("Publisher"), "Sumit Ahmed");
            set_reg_sz(hkey, w!("DisplayIcon"), &icon_str);
            set_reg_sz(hkey, w!("InstallLocation"), &install_loc);
            set_reg_sz(hkey, w!("URLInfoAbout"), "https://github.com/sumitahmed/LumaTray");
            set_reg_sz(hkey, w!("UninstallString"), &format!("\"{}\" --uninstall", exe_str));
            set_reg_dword(hkey, w!("NoModify"), 1);
            set_reg_dword(hkey, w!("NoRepair"), 1);

            let _ = RegCloseKey(hkey);
        }
    }
}

/// Opens an external HTTPS URL in the user's default browser using Win32 `ShellExecuteW`.
pub fn open_external_url(url: &str) {
    info!("Opening external URL in default browser: {}", url);
    unsafe {
        let wide_url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let res = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide_url.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if (res.0 as isize) <= 32 {
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .spawn();
        }
    }
}
