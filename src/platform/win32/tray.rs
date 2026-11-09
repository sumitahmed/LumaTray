use std::mem::size_of;
use std::sync::Arc;
use crossbeam_channel::Sender;
use tracing::{debug, info};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW,
    NIF_ICON,
    NIF_MESSAGE,
    NIF_TIP,
    NIM_ADD,
    NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW,
    CreatePopupMenu,
    CreateWindowExW,
    DefWindowProcW,
    DestroyMenu,
    DestroyWindow,
    GetCursorPos,
    GetSystemMetrics,
    GetWindowLongPtrW,
    LoadIconW,
    LoadImageW,
    PostMessageW,
    PostQuitMessage,
    RegisterClassW,
    SetForegroundWindow,
    SetWindowLongPtrW,
    TrackPopupMenu,
    GWLP_USERDATA,
    HICON,
    IDI_APPLICATION,
    IMAGE_ICON,
    LR_DEFAULTCOLOR,
    LR_SHARED,
    MF_SEPARATOR,
    MF_STRING,
    MSG,
    SM_CXSMICON,
    SM_CYSMICON,
    TPM_BOTTOMALIGN,
    TPM_RIGHTBUTTON,
    WM_APP,
    WM_COMMAND,
    WM_CONTEXTMENU,
    WM_DESTROY,
    WM_DISPLAYCHANGE,
    WM_LBUTTONUP,
    WM_POWERBROADCAST,
    WM_RBUTTONUP,
    WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

use super::power_monitor::PowerSupervisor;

pub const WM_TRAYICON: u32 = WM_APP + 100;
pub const IDM_REFRESH: u32 = 1001;
pub const IDM_SETTINGS: u32 = 1002;
pub const IDM_EXIT: u32 = 1003;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    ToggleFlyout,
    RefreshDisplays,
    OpenSettings,
    Exit,
    DisplayConfigChanged,
}

struct TrayContext {
    event_tx: Sender<TrayEvent>,
    power_supervisor: Arc<PowerSupervisor>,
    taskbar_created_msg: u32,
    uxd_display_msg: u32,
    nid: NOTIFYICONDATAW,
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let ctx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayContext;

        if !ctx_ptr.is_null() {
            let ctx = &mut *ctx_ptr;

            if msg == ctx.taskbar_created_msg && ctx.taskbar_created_msg != 0 {
                info!("Explorer restarted (TaskbarCreated). Restoring system tray icon.");
                let _ = Shell_NotifyIconW(NIM_ADD, &ctx.nid);
                return LRESULT(0);
            }

            if msg == ctx.uxd_display_msg && ctx.uxd_display_msg != 0 {
                debug!("UxdDisplayChangeMessage received. Triggering display refresh.");
                let _ = ctx.event_tx.send(TrayEvent::DisplayConfigChanged);
                return LRESULT(0);
            }

            match msg {
                WM_TRAYICON => {
                    let event_code = (lparam.0 & 0xFFFF) as u32;
                    match event_code {
                        WM_LBUTTONUP => {
                            debug!("Tray left-clicked: ToggleFlyout");
                            let _ = ctx.event_tx.send(TrayEvent::ToggleFlyout);
                        }
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            debug!("Tray right-clicked: Showing Context Menu");
                            let mut cursor = POINT::default();
                            let _ = GetCursorPos(&mut cursor);

                            let hmenu = CreatePopupMenu().unwrap_or_default();
                            if !hmenu.is_invalid() {
                                let _ = AppendMenuW(hmenu, MF_STRING, IDM_REFRESH as usize, w!("Refresh Displays"));
                                let _ = AppendMenuW(hmenu, MF_STRING, IDM_SETTINGS as usize, w!("Settings..."));
                                let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
                                let _ = AppendMenuW(hmenu, MF_STRING, IDM_EXIT as usize, w!("Exit LumaTray"));

                                let _ = SetForegroundWindow(hwnd);
                                let _ = TrackPopupMenu(
                                    hmenu,
                                    TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                                    cursor.x,
                                    cursor.y,
                                    Some(0),
                                    hwnd,
                                    None,
                                );
                                let _ = PostMessageW(Some(hwnd), 0, WPARAM(0), LPARAM(0));
                                let _ = DestroyMenu(hmenu);
                            }
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                }
                WM_COMMAND => {
                    let id = (wparam.0 & 0xFFFF) as u32;
                    match id {
                        IDM_REFRESH => {
                            let _ = ctx.event_tx.send(TrayEvent::RefreshDisplays);
                        }
                        IDM_SETTINGS => {
                            let _ = ctx.event_tx.send(TrayEvent::OpenSettings);
                        }
                        IDM_EXIT => {
                            let _ = ctx.event_tx.send(TrayEvent::Exit);
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                }
                WM_DISPLAYCHANGE => {
                    info!("WM_DISPLAYCHANGE received. Notifying display inventory refresh.");
                    let _ = ctx.event_tx.send(TrayEvent::DisplayConfigChanged);
                    return LRESULT(0);
                }
                WM_POWERBROADCAST => {
                    const PBT_APMSUSPEND: usize = 0x0004;
                    const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
                    const PBT_APMRESUMESUSPEND: usize = 0x0007;

                    if wparam.0 == PBT_APMSUSPEND {
                        ctx.power_supervisor.on_system_suspend();
                    } else if wparam.0 == PBT_APMRESUMEAUTOMATIC || wparam.0 == PBT_APMRESUMESUSPEND {
                        ctx.power_supervisor.on_system_resume();
                        let _ = ctx.event_tx.send(TrayEvent::DisplayConfigChanged);
                    }
                    return LRESULT(1);
                }
                WM_DESTROY => {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &ctx.nid);
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                _ => {}
            }
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

pub struct TrayDaemon {
    hwnd: HWND,
    _ctx: Box<TrayContext>,
}

impl TrayDaemon {
    pub fn spawn(
        event_tx: Sender<TrayEvent>,
        power_supervisor: Arc<PowerSupervisor>,
    ) -> Result<Self, String> {
        unsafe {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;

            let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class_name = w!("LumaTrayMessageWindowClass");

            let wnd_class = WNDCLASSW {
                lpfnWndProc: Some(tray_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };

            let _ = RegisterClassW(&wnd_class);

            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                w!("LumaTray Hidden Message Window"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )
            .map_err(|e| format!("Failed to create tray message window: {:?}", e))?;

            let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            let uxd_display = RegisterWindowMessageW(w!("UxdDisplayChangeMessage"));

            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.uCallbackMessage = WM_TRAYICON;
            let cx = GetSystemMetrics(SM_CXSMICON);
            let cy = GetSystemMetrics(SM_CYSMICON);
            let hicon = if let Ok(handle) = LoadImageW(
                Some(HINSTANCE(hinstance.0)),
                PCWSTR(1 as *const u16),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR | LR_SHARED,
            ) {
                HICON(handle.0)
            } else if let Ok(icon) = LoadIconW(Some(HINSTANCE(hinstance.0)), PCWSTR(1 as *const u16)) {
                icon
            } else {
                LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
            };

            nid.hIcon = hicon;

            // Tip tooltip: "LumaTray - Monitor Brightness"
            let tip = w!("LumaTray - Monitor Brightness");
            let tip_slice = tip.as_wide();
            let copy_len = tip_slice.len().min(nid.szTip.len() - 1);
            nid.szTip[..copy_len].copy_from_slice(&tip_slice[..copy_len]);

            let mut registered = false;
            for attempt in 1..=3 {
                if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                    registered = true;
                    info!("Registered Windows notification area system tray icon.");
                    break;
                }
                debug!("Shell_NotifyIconW(NIM_ADD) attempt {}/3 failed, retrying in 250ms...", attempt);
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            if !registered {
                let err = windows::core::Error::from_thread();
                debug!("Shell_NotifyIconW deferred (Explorer may be busy or non-interactive): {:?}", err);
            }

            let ctx = Box::new(TrayContext {
                event_tx,
                power_supervisor,
                taskbar_created_msg: taskbar_created,
                uxd_display_msg: uxd_display,
                nid,
            });

            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*ctx as *const _ as isize);

            Ok(Self { hwnd, _ctx: ctx })
        }
    }

    /// Blocking Windows message loop for the background tray thread.
    /// Sleeps with 0% CPU until a tray message arrives.
    pub fn run_message_loop() {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, TranslateMessage, DispatchMessageW};
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Non-blocking check for pending Windows messages.
    pub fn pump_messages() -> bool {
        unsafe {
            let mut msg = MSG::default();
            while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut msg,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
            )
            .as_bool()
            {
                if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_QUIT {
                    return false;
                }
                let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
            }
            true
        }
    }

    pub fn shutdown(&self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_icon() {
        unsafe {
            let icon = LoadIconW(None, IDI_APPLICATION);
            println!("LoadIconW result: {:?}", icon);
            assert!(icon.is_ok(), "LoadIconW should succeed");
            let icon_val = icon.unwrap();
            assert!(!icon_val.is_invalid(), "hIcon must not be invalid");
        }
    }

    #[test]
    fn test_tray_add() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let sup = Arc::new(PowerSupervisor::new());
        let daemon = TrayDaemon::spawn(tx, sup);
        println!("TrayDaemon::spawn result: {:?}", daemon.is_ok());
        if let Err(e) = &daemon {
            println!("Spawn err: {}", e);
        }
        assert!(daemon.is_ok());
        daemon.unwrap().shutdown();
    }
}
