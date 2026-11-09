use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tracing::debug;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW,
    SetForegroundWindow,
    SetWindowLongPtrW,
    GWLP_WNDPROC,
    WA_INACTIVE,
    WM_ACTIVATE,
    WNDPROC,
};

static OLD_WNDPROC: AtomicIsize = AtomicIsize::new(0);
static DISMISS_CALLBACK: Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>> = Mutex::new(None);
static LAST_SHOW_INSTANT: Mutex<Option<Instant>> = Mutex::new(None);

/// Records the timestamp when the flyout was displayed to ignore transient activation noise.
pub fn notify_flyout_shown() {
    *LAST_SHOW_INSTANT.lock() = Some(Instant::now());
}

unsafe extern "system" fn flyout_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_ACTIVATE => {
                let state = (wparam.0 & 0xFFFF) as u32;
                if state == WA_INACTIVE {
                    let is_transient = LAST_SHOW_INSTANT
                        .lock()
                        .map_or(false, |t| t.elapsed() < Duration::from_millis(250));

                    if !is_transient {
                        debug!("Flyout lost activation (WA_INACTIVE). Triggering auto-dismiss.");
                        let cb = DISMISS_CALLBACK.lock().clone();
                        if let Some(cb) = cb {
                            cb();
                        }
                    } else {
                        debug!("Ignoring transient WA_INACTIVE during flyout show grace period");
                    }
                }
            }
            _ => {}
        }

        let old_proc = OLD_WNDPROC.load(Ordering::SeqCst);
        if old_proc != 0 {
            let proc_fn: WNDPROC = std::mem::transmute(old_proc);
            CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam)
        } else {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

pub fn hook_flyout_dismissal<F>(hwnd: HWND, on_dismiss: F)
where
    F: Fn() + Send + Sync + 'static,
{
    unsafe {
        *DISMISS_CALLBACK.lock() = Some(Arc::new(on_dismiss));

        // Subclass once
        let current_old = OLD_WNDPROC.load(Ordering::SeqCst);
        if current_old == 0 {
            let prev_proc = SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                flyout_subclass_proc as *const () as usize as isize,
            );
            OLD_WNDPROC.store(prev_proc, Ordering::SeqCst);
            debug!("Flyout subclass hook installed successfully");
        }

        let _ = SetForegroundWindow(hwnd);
    }
}
