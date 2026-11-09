use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tracing::info;

use crate::core::state::StateStore;
use crate::core::worker::HardwareWorkerHandle;
use crate::hal::descriptor::DriverType;
use crate::platform::win32::autostart::{is_autostart_enabled, set_autostart};
use crate::platform::win32::backdrop::{apply_fluent_backdrop, is_system_in_dark_mode, BackdropType};
use crate::platform::win32::flyout_hook::{hook_flyout_dismissal, notify_flyout_shown};
use crate::platform::win32::taskbar::calculate_flyout_anchor;

slint::include_modules!();

/// Shared visibility flag that can be flipped from the Win32 dismiss hook (Send+Sync).
static FLYOUT_VISIBLE: AtomicBool = AtomicBool::new(false);

pub struct UiBridge {
    window: MainWindow,
    display_model: Rc<VecModel<DisplayItem>>,
    state_store: Arc<StateStore>,
    worker: HardwareWorkerHandle,
    is_visible: RefCell<bool>,
}

impl UiBridge {
    pub fn new(
        state_store: Arc<StateStore>,
        worker: HardwareWorkerHandle,
    ) -> Result<Rc<Self>, slint::PlatformError> {
        let window = MainWindow::new()?;
        let display_model = Rc::new(VecModel::default());
        window.set_displays(ModelRc::from(display_model.clone()));

        // Initialize autostart state
        window.set_autostart_enabled(is_autostart_enabled());
        window.set_app_theme("system".into());
        window.set_is_dark(is_system_in_dark_mode());

        let bridge = Rc::new(Self {
            window,
            display_model,
            state_store,
            worker,
            is_visible: RefCell::new(false),
        });

        bridge.setup_callbacks();
        bridge.sync_from_state();

        Ok(bridge)
    }

    fn setup_callbacks(self: &Rc<Self>) {
        let weak_bridge = Rc::downgrade(self);

        // 1. Monitor Brightness Changed
        // Update both hardware AND the row in display_model so Slint's model and slider stay in sync!
        {
            let weak = weak_bridge.clone();
            self.window.on_monitor_brightness_changed(move |id, val| {
                if let Some(bridge) = weak.upgrade() {
                    let id_str = id.to_string();
                    let val_clamped = val.clamp(0, 100) as u32;

                    bridge.state_store.set_display_brightness(&id_str, val_clamped);

                    // Update the active display's row in-place without recreating the model
                    let snapshot = bridge.state_store.snapshot();
                    for (i, disp) in snapshot.displays.iter().enumerate() {
                        if disp.descriptor.id == id_str {
                            if let Some(mut row) = bridge.display_model.row_data(i) {
                                if row.brightness != val_clamped as i32 {
                                    row.brightness = val_clamped as i32;
                                    bridge.display_model.set_row_data(i, row);
                                }
                            }
                        } else if snapshot.linked_mode && disp.is_linked {
                            if let Some(mut row) = bridge.display_model.row_data(i) {
                                if row.brightness != disp.current_brightness as i32 {
                                    row.brightness = disp.current_brightness as i32;
                                    bridge.display_model.set_row_data(i, row);
                                }
                            }
                        }
                    }

                    if snapshot.linked_mode {
                        bridge.window.set_master_brightness(snapshot.master_brightness as i32);
                    }
                }
            });
        }

        // 2. Master Brightness Changed
        {
            let weak = weak_bridge.clone();
            self.window.on_master_brightness_changed(move |val| {
                if let Some(bridge) = weak.upgrade() {
                    let val_clamped = val.clamp(0, 100) as u32;
                    bridge.state_store.set_master_brightness(val_clamped);

                    // Update linked display cards in-place without recreating the model
                    let snapshot = bridge.state_store.snapshot();
                    for (i, disp) in snapshot.displays.iter().enumerate() {
                        if disp.is_linked {
                            if let Some(mut row) = bridge.display_model.row_data(i) {
                                if row.brightness != disp.current_brightness as i32 {
                                    row.brightness = disp.current_brightness as i32;
                                    bridge.display_model.set_row_data(i, row);
                                }
                            }
                        }
                    }
                }
            });
        }

        // 3. Toggle Sync Master
        {
            let weak = weak_bridge.clone();
            self.window.on_toggle_sync_master(move || {
                if let Some(bridge) = weak.upgrade() {
                    let next = !bridge.window.get_sync_enabled();
                    bridge.state_store.set_linked_mode(next);
                    bridge.sync_from_state();
                }
            });
        }

        // 4. Toggle Monitor Link
        {
            let weak = weak_bridge.clone();
            self.window.on_toggle_monitor_link(move |id| {
                if let Some(bridge) = weak.upgrade() {
                    let id_str = id.to_string();
                    let snapshot = bridge.state_store.snapshot();
                    if let Some(disp) = snapshot.displays.iter().find(|d| d.descriptor.id == id_str) {
                        let new_link_state = !disp.is_linked;
                        bridge.state_store.set_display_link_state(&id_str, new_link_state);
                        bridge.sync_from_state();
                    }
                }
            });
        }

        // 5. Refresh Displays (Actual Hardware Re-enumeration)
        {
            let weak = weak_bridge.clone();
            self.window.on_refresh_clicked(move || {
                if let Some(bridge) = weak.upgrade() {
                    info!("Refreshing connected displays from hardware...");
                    bridge.window.set_status_text("Scanning displays...".into());
                    if let Ok(descriptors) = bridge.worker.enumerate() {
                        let count = descriptors.len();
                        bridge.state_store.update_inventory(descriptors);
                        bridge.sync_from_state();
                        let status = format!("{} display{} active", count, if count == 1 { "" } else { "s" });
                        bridge.window.set_status_text(SharedString::from(status));
                    }
                }
            });
        }

        // 6. Start with Windows Toggle
        {
            let weak = weak_bridge.clone();
            self.window.on_toggle_autostart(move |val| {
                if let Some(bridge) = weak.upgrade() {
                    let _ = set_autostart(val);
                    bridge.window.set_autostart_enabled(val);
                }
            });
        }

        // 7. Theme Changed (Genuine dynamic theme switching)
        {
            let weak = weak_bridge.clone();
            self.window.on_theme_changed(move |theme| {
                if let Some(bridge) = weak.upgrade() {
                    info!("User changed theme preference to: {}", theme);
                    let theme_str = theme.to_string();
                    bridge.window.set_app_theme(theme);
                    let is_dark = match theme_str.as_str() {
                        "dark" => true,
                        "light" => false,
                        _ => is_system_in_dark_mode(),
                    };
                    bridge.window.set_is_dark(is_dark);
                    bridge.apply_platform_effects();
                }
            });
        }

        // 8. Native Header Window Dragging
        {
            self.window.on_drag_window(move || {
                #[cfg(target_os = "windows")]
                unsafe {
                    use windows::core::w;
                    use windows::Win32::Foundation::{LPARAM, WPARAM};
                    use windows::Win32::UI::WindowsAndMessaging::{
                        FindWindowW, SendMessageW, HTCAPTION, WM_NCLBUTTONDOWN,
                    };

                    unsafe extern "system" {
                        fn ReleaseCapture() -> windows::core::BOOL;
                    }

                    if let Ok(hwnd) = FindWindowW(None, w!("LumaTray")) {
                        let _ = ReleaseCapture();
                        let _ = SendMessageW(
                            hwnd,
                            WM_NCLBUTTONDOWN,
                            Some(WPARAM(HTCAPTION as usize)),
                            Some(LPARAM(0)),
                        );
                    }
                }
            });
        }
    }

    /// Synchronizes the Slint UI model from the current `StateStore` snapshot.
    pub fn sync_from_state(&self) {
        let snapshot = self.state_store.snapshot();
        let mut items = Vec::new();

        for disp in &snapshot.displays {
            let driver_type_str = match disp.descriptor.driver_type {
                DriverType::DdcCi => "ddc_ci",
                DriverType::WmiInternal => "wmi",
                DriverType::SoftwareGamma => "gamma",
            };

            let name = disp.user_friendly_name.as_ref().unwrap_or(&disp.descriptor.name);

            items.push(DisplayItem {
                id: SharedString::from(&disp.descriptor.id),
                name: SharedString::from(name),
                driver_type: SharedString::from(driver_type_str),
                brightness: disp.current_brightness as i32,
                contrast: disp.current_contrast.unwrap_or(50) as i32,
                has_contrast: false,
                is_linked: disp.is_linked,
                is_accessible: true,
            });
        }

        self.display_model.set_vec(items);
        self.window.set_sync_enabled(snapshot.linked_mode);
        self.window.set_master_brightness(snapshot.master_brightness as i32);

        let status = format!(
            "{} display{} active",
            snapshot.displays.len(),
            if snapshot.displays.len() == 1 { "" } else { "s" }
        );
        self.window.set_status_text(SharedString::from(status));
    }

    /// Toggles the flyout window visibility anchored over the system tray.
    pub fn toggle_flyout(self: &Rc<Self>) {
        let mut visible = self.is_visible.borrow_mut();

        // Check both local state and atomic dismiss flag
        if FLYOUT_VISIBLE.load(Ordering::SeqCst) && *visible {
            self.hide_flyout_inner();
            *visible = false;
            FLYOUT_VISIBLE.store(false, Ordering::SeqCst);
        } else {
            self.sync_from_state();

            // Sync theme preference to active dark/light mode
            let theme_pref = self.window.get_app_theme();
            let is_dark = match theme_pref.as_str() {
                "dark" => true,
                "light" => false,
                _ => is_system_in_dark_mode(),
            };
            self.window.set_is_dark(is_dark);

            let _ = self.window.show();

            #[cfg(target_os = "windows")]
            unsafe {
                use windows::core::w;
                use windows::Win32::Foundation::RECT;
                use windows::Win32::UI::WindowsAndMessaging::{
                    FindWindowW, GetWindowRect, SetWindowPos, SetForegroundWindow,
                    HWND_TOPMOST, SWP_SHOWWINDOW, SWP_NOSIZE,
                };
                use windows::Win32::UI::HiDpi::GetDpiForWindow;

                if let Ok(hwnd) = FindWindowW(None, w!("LumaTray")) {
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    let mut physical_w = rect.right - rect.left;
                    let mut physical_h = rect.bottom - rect.top;

                    // Fallback to DPI calculation if window has not completed layout
                    if physical_w <= 0 || physical_h <= 100 {
                        let dpi = GetDpiForWindow(hwnd);
                        let scale = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };
                        physical_w = (380.0 * scale).round() as i32;
                        physical_h = (280.0 * scale).round() as i32;
                    }

                    let anchor = calculate_flyout_anchor(physical_w, physical_h);

                    // Position using exact physical pixels; SWP_NOSIZE prevents squishing the DPI scale!
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        anchor.x,
                        anchor.y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_SHOWWINDOW,
                    );

                    let _ = SetForegroundWindow(hwnd);
                }
            }

            notify_flyout_shown();
            *visible = true;
            FLYOUT_VISIBLE.store(true, Ordering::SeqCst);

            // Apply Windows 11 Fluent Acrylic / Mica backdrop & click-outside hook
            self.apply_platform_effects();
        }
    }

    pub fn open_settings(self: &Rc<Self>) {
        self.window.set_show_settings(true);
        if !self.is_flyout_visible() {
            self.toggle_flyout();
        }
    }

    /// Hides the flyout WITHOUT quitting the event loop.
    /// Uses Win32 ShowWindow(SW_HIDE) instead of Slint's window.hide() which
    /// would signal the event loop to terminate.
    fn hide_flyout_inner(&self) {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::{
                FindWindowW, ShowWindow, SW_HIDE,
            };

            if let Ok(hwnd) = FindWindowW(None, w!("LumaTray")) {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }

        crate::platform::win32::backdrop::trim_working_set();
    }

    pub fn hide_flyout(&self) {
        let mut visible = self.is_visible.borrow_mut();
        if *visible || FLYOUT_VISIBLE.load(Ordering::SeqCst) {
            self.hide_flyout_inner();
            *visible = false;
            FLYOUT_VISIBLE.store(false, Ordering::SeqCst);
        }
    }

    pub fn is_flyout_visible(&self) -> bool {
        *self.is_visible.borrow() && FLYOUT_VISIBLE.load(Ordering::SeqCst)
    }

    #[allow(unused_unsafe)]
    fn apply_platform_effects(self: &Rc<Self>) {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

            if let Ok(hwnd) = FindWindowW(None, w!("LumaTray")) {
                if !hwnd.is_invalid() {
                    let is_dark = self.window.get_is_dark();
                    apply_fluent_backdrop(hwnd, BackdropType::Acrylic, is_dark);

                    // Auto-dismiss when focus is lost (clicking outside).
                    // HWND raw pointer is Send+Sync safe to capture.
                    let hwnd_raw = hwnd.0 as isize;
                    hook_flyout_dismissal(hwnd, move || {
                        use windows::Win32::Foundation::HWND;
                        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

                        let h = HWND(hwnd_raw as *mut _);
                        let _ = unsafe { ShowWindow(h, SW_HIDE) };
                        FLYOUT_VISIBLE.store(false, Ordering::SeqCst);
                        crate::platform::win32::backdrop::trim_working_set();
                    });
                }
            }
        }
    }
}
