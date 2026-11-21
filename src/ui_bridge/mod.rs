use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tracing::info;

use crate::core::state::StateStore;
use crate::core::worker::HardwareWorkerHandle;
use crate::hal::descriptor::DriverType;
use crate::platform::win32::autostart::{is_autostart_enabled, set_autostart};
use crate::platform::win32::backdrop::{apply_fluent_backdrop, is_system_in_dark_mode, trim_working_set, BackdropType};
use crate::platform::win32::flyout_hook::{hook_flyout_dismissal, notify_flyout_shown};
use crate::platform::win32::installer::{installed_exe_path, is_running_from_install_dir, open_external_url};
use crate::platform::win32::taskbar::calculate_flyout_anchor;

slint::include_modules!();

pub const GITHUB_REPO_URL: &str = "https://github.com/sumitahmed/LumaTray";

/// Shared visibility flag that can be flipped from the Win32 dismiss hook (Send+Sync).
static FLYOUT_VISIBLE: AtomicBool = AtomicBool::new(false);

pub struct UiBridge {
    window: MainWindow,
    display_model: Rc<VecModel<DisplayItem>>,
    settings_window: RefCell<Option<SettingsWindow>>,
    welcome_window: RefCell<Option<WelcomeWindow>>,
    welcome_timer: Timer,
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

        let snapshot = state_store.snapshot();
        let is_dark = resolve_is_dark(&snapshot.config.app_theme);
        window.set_is_dark(is_dark);
        window.set_accent_id(SharedString::from(&snapshot.config.accent_color));

        let bridge = Rc::new(Self {
            window,
            display_model,
            settings_window: RefCell::new(None),
            welcome_window: RefCell::new(None),
            welcome_timer: Timer::default(),
            state_store,
            worker,
            is_visible: RefCell::new(false),
        });

        bridge.setup_callbacks();
        bridge.sync_from_state();

        // Automatically show "Hello, LumaTray!" onboarding guide on first run
        // or whenever the user runs the standalone installer exe
        if !snapshot.config.first_run_completed || !is_running_from_install_dir() {
            bridge.open_welcome();
        }

        schedule_ram_trim();

        Ok(bridge)
    }

    fn setup_callbacks(self: &Rc<Self>) {
        let weak_bridge = Rc::downgrade(self);

        // 1. Monitor Brightness Changed
        {
            let weak = weak_bridge.clone();
            self.window.on_monitor_brightness_changed(move |id, val| {
                if let Some(bridge) = weak.upgrade() {
                    let id_str = id.to_string();
                    let val_clamped = val.clamp(0, 100) as u32;

                    bridge.state_store.set_display_brightness(&id_str, val_clamped);

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

        // 2. Monitor Contrast Changed
        {
            let weak = weak_bridge.clone();
            self.window.on_monitor_contrast_changed(move |id, val| {
                if let Some(bridge) = weak.upgrade() {
                    let id_str = id.to_string();
                    let val_clamped = val.clamp(0, 100) as u32;
                    bridge.state_store.set_display_contrast(&id_str, val_clamped);

                    let snapshot = bridge.state_store.snapshot();
                    for (i, disp) in snapshot.displays.iter().enumerate() {
                        if disp.descriptor.id == id_str {
                            if let Some(mut row) = bridge.display_model.row_data(i) {
                                row.contrast = val_clamped as i32;
                                bridge.display_model.set_row_data(i, row);
                            }
                        }
                    }
                }
            });
        }

        // 3. Master Brightness Changed
        {
            let weak = weak_bridge.clone();
            self.window.on_master_brightness_changed(move |val| {
                if let Some(bridge) = weak.upgrade() {
                    let val_clamped = val.clamp(0, 100) as u32;
                    bridge.state_store.set_master_brightness(val_clamped);

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

        // 4. Toggle Sync Master
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

        // 5. Toggle Monitor Link
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

        // 6. Refresh Displays
        {
            let weak = weak_bridge.clone();
            self.window.on_refresh_clicked(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.refresh_displays();
                }
            });
        }

        // 7. Open Dedicated Settings Window
        {
            let weak = weak_bridge.clone();
            self.window.on_open_settings_clicked(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.open_settings();
                }
            });
        }

        // 8. Open GitHub Repository URL
        {
            self.window.on_open_github_clicked(move || {
                open_external_url(GITHUB_REPO_URL);
            });
        }

        // 9. Native Header Window Dragging
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

    pub fn refresh_displays(self: &Rc<Self>) {
        info!("Refreshing connected displays from hardware...");
        self.window.set_status_text("Scanning displays...".into());
        if let Ok(descriptors) = self.worker.enumerate() {
            let count = descriptors.len();
            self.state_store.update_inventory(descriptors);
            self.sync_from_state();
            let status = format!("{} display{} active", count, if count == 1 { "" } else { "s" });
            self.window.set_status_text(SharedString::from(status));
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
            let supports_contrast = disp.descriptor.driver_type == DriverType::DdcCi;

            items.push(DisplayItem {
                id: SharedString::from(&disp.descriptor.id),
                name: SharedString::from(name),
                gdi_name: SharedString::from(&disp.descriptor.gdi_name),
                resolution: SharedString::from(&disp.descriptor.resolution),
                refresh_rate: disp.descriptor.refresh_rate_hz as i32,
                driver_type: SharedString::from(driver_type_str),
                brightness: disp.current_brightness as i32,
                contrast: disp.current_contrast.unwrap_or(50) as i32,
                min_limit: disp.min_limit as i32,
                max_limit: disp.max_limit as i32,
                has_contrast: supports_contrast,
                show_contrast: disp.show_contrast,
                is_linked: disp.is_linked,
                is_accessible: true,
            });
        }

        self.display_model.set_vec(items);
        self.window.set_sync_enabled(snapshot.linked_mode);
        self.window.set_master_brightness(snapshot.master_brightness as i32);

        let is_dark = resolve_is_dark(&snapshot.config.app_theme);
        self.window.set_is_dark(is_dark);
        self.window.set_accent_id(SharedString::from(&snapshot.config.accent_color));

        let status = format!(
            "{} display{} active",
            snapshot.displays.len(),
            if snapshot.displays.len() == 1 { "" } else { "s" }
        );
        self.window.set_status_text(SharedString::from(status));

        // Update SettingsWindow if currently open
        if let Some(settings) = self.settings_window.borrow().as_ref() {
            settings.set_is_dark(is_dark);
            settings.set_accent_id(SharedString::from(&snapshot.config.accent_color));
            settings.set_displays(ModelRc::from(self.display_model.clone()));
            settings.set_autostart_enabled(is_autostart_enabled());
            settings.set_restore_on_startup(snapshot.config.restore_brightness_on_startup);
            settings.set_app_theme(SharedString::from(&snapshot.config.app_theme));
            settings.set_accent_color(SharedString::from(&snapshot.config.accent_color));
            settings.set_backdrop_style(SharedString::from(&snapshot.config.backdrop_style));
            settings.set_throttle_ms(snapshot.config.throttle_ms as i32);
            settings.set_installed_path(SharedString::from(
                installed_exe_path().to_string_lossy().as_ref(),
            ));
        }

        // Update WelcomeWindow if currently open
        if let Some(welcome) = self.welcome_window.borrow().as_ref() {
            welcome.set_is_dark(is_dark);
            welcome.set_accent_id(SharedString::from(&snapshot.config.accent_color));
        }

        // If flyout is currently visible, dynamically resize and re-anchor its bottom-right position
        if self.is_flyout_visible() {
            self.reposition_flyout();
        }
    }

    /// Dynamically resizes and anchors the Win32 Flyout window using Slint's native DPI-aware
    /// `LogicalSize` and `SWP_NOSIZE` so high-DPI monitors (125% / 150%) are never clipped horizontally or vertically.
    fn reposition_flyout(&self) {
        let logical_w = 384.0f32;
        let logical_h = self.window.get_desired_height().max(180.0);

        // Let Slint/winit resize the HWND using its exact per-monitor DPI scale factor
        self.window
            .window()
            .set_size(slint::LogicalSize::new(logical_w, logical_h));

        #[cfg(target_os = "windows")]
        unsafe {
            use windows::core::w;
            use windows::Win32::Foundation::RECT;
            use windows::Win32::UI::WindowsAndMessaging::{
                FindWindowW, GetWindowRect, SetForegroundWindow, SetWindowPos, HWND_TOPMOST,
                SWP_NOSIZE, SWP_SHOWWINDOW,
            };

            if let Ok(hwnd) = FindWindowW(None, w!("LumaTray")) {
                if !hwnd.is_invalid() {
                    let scale = self.window.window().scale_factor().max(1.0);
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    let mut physical_w = rect.right - rect.left;
                    let mut physical_h = rect.bottom - rect.top;

                    if physical_w <= 100 || physical_h <= 100 {
                        physical_w = (logical_w * scale).round() as i32;
                        physical_h = (logical_h * scale).round() as i32;
                    }

                    let anchor = calculate_flyout_anchor(physical_w, physical_h);

                    // First move to the target monitor using SWP_NOSIZE so Win32 never squishes DPI scaling
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        anchor.x,
                        anchor.y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_SHOWWINDOW,
                    );

                    // Re-apply logical size in case moving to the target monitor changed the window's DPI scale
                    self.window
                        .window()
                        .set_size(slint::LogicalSize::new(logical_w, logical_h));

                    let mut rect2 = RECT::default();
                    if GetWindowRect(hwnd, &mut rect2).is_ok() {
                        let new_w = rect2.right - rect2.left;
                        let new_h = rect2.bottom - rect2.top;
                        if new_w > 100 && new_h > 100 && (new_w != physical_w || new_h != physical_h) {
                            let anchor2 = calculate_flyout_anchor(new_w, new_h);
                            let _ = SetWindowPos(
                                hwnd,
                                Some(HWND_TOPMOST),
                                anchor2.x,
                                anchor2.y,
                                0,
                                0,
                                SWP_NOSIZE | SWP_SHOWWINDOW,
                            );
                        }
                    }

                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
    }

    /// Toggles the flyout window visibility anchored over the system tray.
    pub fn toggle_flyout(self: &Rc<Self>) {
        if self.is_flyout_visible() {
            self.hide_flyout();
        } else {
            self.sync_from_state();
            let _ = self.window.show();

            notify_flyout_shown();
            *self.is_visible.borrow_mut() = true;
            FLYOUT_VISIBLE.store(true, Ordering::SeqCst);

            self.reposition_flyout();
            self.apply_platform_effects();
            schedule_ram_trim();
        }
    }

    /// Lazily opens the dedicated spacious SettingsWindow and drops it from RAM on close.
    pub fn open_settings(self: &Rc<Self>) {
        self.hide_flyout();

        if let Some(existing) = self.settings_window.borrow().as_ref() {
            let _ = existing.show();
            return;
        }

        let settings = match SettingsWindow::new() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("Failed to create SettingsWindow: {:?}", e);
                return;
            }
        };

        let snapshot = self.state_store.snapshot();
        let is_dark = resolve_is_dark(&snapshot.config.app_theme);
        settings.set_is_dark(is_dark);
        settings.set_accent_id(SharedString::from(&snapshot.config.accent_color));
        settings.set_displays(ModelRc::from(self.display_model.clone()));
        settings.set_autostart_enabled(is_autostart_enabled());
        settings.set_restore_on_startup(snapshot.config.restore_brightness_on_startup);
        settings.set_app_theme(SharedString::from(&snapshot.config.app_theme));
        settings.set_accent_color(SharedString::from(&snapshot.config.accent_color));
        settings.set_backdrop_style(SharedString::from(&snapshot.config.backdrop_style));
        settings.set_throttle_ms(snapshot.config.throttle_ms as i32);
        settings.set_installed_path(SharedString::from(
            installed_exe_path().to_string_lossy().as_ref(),
        ));

        let weak = Rc::downgrade(self);

        // 1. Toggle Launch at Startup
        {
            let weak = weak.clone();
            settings.on_toggle_autostart(move |val| {
                if let Some(bridge) = weak.upgrade() {
                    let _ = set_autostart(val);
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.start_with_windows = val;
                    });
                    bridge.sync_from_state();
                }
            });
        }

        // 2. Toggle Restore Brightness at Startup
        {
            let weak = weak.clone();
            settings.on_toggle_restore_startup(move |val| {
                if let Some(bridge) = weak.upgrade() {
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.restore_brightness_on_startup = val;
                    });
                }
            });
        }

        // 3. Theme Mode Changed
        {
            let weak = weak.clone();
            settings.on_theme_changed(move |theme| {
                if let Some(bridge) = weak.upgrade() {
                    let theme_str = theme.to_string();
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.app_theme = theme_str;
                    });
                    bridge.sync_from_state();
                    bridge.apply_platform_effects();
                }
            });
        }

        // 4. Accent Color Swatch Changed
        {
            let weak = weak.clone();
            settings.on_accent_changed(move |accent| {
                if let Some(bridge) = weak.upgrade() {
                    let accent_str = accent.to_string();
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.accent_color = accent_str;
                    });
                    bridge.sync_from_state();
                }
            });
        }

        // 5. Window Backdrop Changed
        {
            let weak = weak.clone();
            settings.on_backdrop_changed(move |bd| {
                if let Some(bridge) = weak.upgrade() {
                    let bd_str = bd.to_string();
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.backdrop_style = bd_str;
                    });
                    bridge.apply_platform_effects();
                }
            });
        }

        // 6. DDC/CI Update Rate Changed
        {
            let weak = weak.clone();
            settings.on_throttle_changed(move |ms| {
                if let Some(bridge) = weak.upgrade() {
                    let ms_u64 = ms.clamp(15, 250) as u64;
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.throttle_ms = ms_u64;
                    });
                }
            });
        }

        // 7. Per-Monitor Min/Max Limits & Contrast Slider Toggle
        {
            let weak = weak.clone();
            settings.on_monitor_limits_changed(move |id, min_l, max_l, show_c| {
                if let Some(bridge) = weak.upgrade() {
                    bridge.state_store.update_display_limits(
                        id.as_str(),
                        min_l.clamp(0, 90) as u32,
                        max_l.clamp(10, 100) as u32,
                        show_c,
                    );
                    bridge.sync_from_state();
                }
            });
        }

        // 8. Rescan Monitors from Settings
        {
            let weak = weak.clone();
            settings.on_refresh_monitors(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.refresh_displays();
                }
            });
        }

        // 9. Show Welcome Walkthrough Guide
        {
            let weak = weak.clone();
            settings.on_show_welcome_guide(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.open_welcome();
                }
            });
        }

        // 10. Reset All Settings & Colors to Default
        {
            let weak = weak.clone();
            settings.on_reset_all_settings(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.state_store.reset_to_defaults();
                    let _ = set_autostart(true);
                    bridge.sync_from_state();
                    bridge.apply_platform_effects();
                }
            });
        }

        // 11. Open GitHub Repository
        {
            settings.on_open_github(move || {
                open_external_url(GITHUB_REPO_URL);
            });
        }

        // Destroy SettingsWindow & trim RAM back to <1.5 MB when closed
        {
            let weak = weak.clone();
            settings.window().on_close_requested(move || {
                if let Some(bridge) = weak.upgrade() {
                    if let Some(w) = bridge.settings_window.borrow_mut().take() {
                        let _ = w.hide();
                    }
                    schedule_ram_trim();
                }
                CloseRequestResponse::HideWindow
            });
        }

        let _ = settings.show();
        *self.settings_window.borrow_mut() = Some(settings);
        self.apply_platform_effects();
        schedule_ram_trim();
    }

    /// Lazily opens the "Hello, LumaTray!" first-run welcome walkthrough and frees RAM on close.
    pub fn open_welcome(self: &Rc<Self>) {
        if let Some(existing) = self.welcome_window.borrow().as_ref() {
            let _ = existing.show();
            return;
        }

        let welcome = match WelcomeWindow::new() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("Failed to create WelcomeWindow: {:?}", e);
                return;
            }
        };

        let snapshot = self.state_store.snapshot();
        welcome.set_is_dark(resolve_is_dark(&snapshot.config.app_theme));
        welcome.set_accent_id(SharedString::from(&snapshot.config.accent_color));

        let weak = Rc::downgrade(self);
        {
            let weak = weak.clone();
            welcome.on_close_clicked(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.welcome_timer.stop();
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.first_run_completed = true;
                    });
                    if let Some(w) = bridge.welcome_window.borrow_mut().take() {
                        let _ = w.hide();
                    }
                    schedule_ram_trim();
                }
            });
        }

        {
            let weak = weak.clone();
            welcome.window().on_close_requested(move || {
                if let Some(bridge) = weak.upgrade() {
                    bridge.welcome_timer.stop();
                    bridge.state_store.update_preferences(|cfg| {
                        cfg.first_run_completed = true;
                    });
                    if let Some(w) = bridge.welcome_window.borrow_mut().take() {
                        let _ = w.hide();
                    }
                    schedule_ram_trim();
                }
                CloseRequestResponse::HideWindow
            });
        }

        let frame_counter = Rc::new(RefCell::new(0u32));
        {
            let fc = frame_counter.clone();
            welcome.on_step_selected(move |step_idx| {
                *fc.borrow_mut() = match step_idx {
                    0 => 0,
                    1 => 37,
                    _ => 73,
                };
            });
        }

        let _ = welcome.show();
        *self.welcome_window.borrow_mut() = Some(welcome);

        // Drive the 20 FPS (50ms) 148-frame onboarding animation while WelcomeWindow is open
        let weak_for_timer = Rc::downgrade(self);
        self.welcome_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                if let Some(bridge) = weak_for_timer.upgrade() {
                    if let Some(w) = bridge.welcome_window.borrow().as_ref() {
                        let mut f_guard = frame_counter.borrow_mut();
                        let f = *f_guard;
                        *f_guard = (f + 1) % 148;
                        update_welcome_animation_frame(w, f);
                    }
                }
            },
        );

        self.apply_platform_effects();
        schedule_ram_trim();
    }

    /// Hides the flyout WITHOUT quitting the event loop.
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

        trim_working_set();
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

            let snapshot = self.state_store.snapshot();
            let is_dark = self.window.get_is_dark();
            let backdrop_type = match snapshot.config.backdrop_style.as_str() {
                "mica" => BackdropType::Mica,
                "solid" => BackdropType::None,
                _ => BackdropType::Acrylic,
            };

            if let Ok(hwnd) = FindWindowW(None, w!("LumaTray")) {
                if !hwnd.is_invalid() {
                    apply_fluent_backdrop(hwnd, backdrop_type, is_dark);

                    let hwnd_raw = hwnd.0 as isize;
                    hook_flyout_dismissal(hwnd, move || {
                        use windows::Win32::Foundation::HWND;
                        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

                        let h = HWND(hwnd_raw as *mut _);
                        let _ = unsafe { ShowWindow(h, SW_HIDE) };
                        FLYOUT_VISIBLE.store(false, Ordering::SeqCst);
                        trim_working_set();
                    });
                }
            }

            if let Ok(settings_hwnd) = FindWindowW(None, w!("LumaTray Settings")) {
                if !settings_hwnd.is_invalid() {
                    apply_fluent_backdrop(settings_hwnd, BackdropType::Mica, is_dark);
                }
            }

            if let Ok(welcome_hwnd) = FindWindowW(None, w!("Welcome to LumaTray")) {
                if !welcome_hwnd.is_invalid() {
                    apply_fluent_backdrop(welcome_hwnd, BackdropType::Mica, is_dark);
                }
            }
        }
    }
}

fn resolve_is_dark(theme_pref: &str) -> bool {
    match theme_pref {
        "dark" => true,
        "light" => false,
        _ => is_system_in_dark_mode(),
    }
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn ease_in_out(t: f32) -> f32 {
    let t = clamp01(t);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = clamp01(t);
    1.0 - (1.0 - t).powi(3)
}

fn quad_bezier(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let x = u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0;
    let y = u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1;
    (x, y)
}

/// Updates all continuous properties of `WelcomeWindow` for frame `f` (0..148 @ 20 FPS = 7.4s loop).
fn update_welcome_animation_frame(w: &WelcomeWindow, f: u32) {
    let t = f as f32 / 20.0;
    let global_prog = f as f32 / 147.0;

    const CHEV_POS: (f32, f32) = (394.0, 336.0);
    const OVERFLOW_LUMA: (f32, f32) = (431.0, 281.0);
    const PIN_TARGET: (f32, f32) = (394.0, 336.0);

    let mut step = 0;
    let mut chev_hover = false;
    let mut chev_open = false;
    let mut overflow_alpha = 0.0f32;
    let mut overflow_dy = 10.0f32;
    let mut icon_in_overflow = true;
    let mut icon_hover = false;
    let mut dragging = false;
    let mut drag_pos = OVERFLOW_LUMA;
    let mut pinned_slot_t = 0.0f32;
    let mut icon_pinned = false;
    let mut tray_active = false;
    let mut flyout_alpha = 0.0f32;
    let mut flyout_dy = 12.0f32;
    let mut b1 = 65.0f32;
    let mut b2 = 40.0f32;
    let mut active_slider = 0;
    let mut cursor_pressed = false;
    let mut ripple = 0.0f32;
    let (cx, cy);

    if t < 0.7 {
        let p = ease_in_out(t / 0.7);
        cx = lerp(260.0, CHEV_POS.0, p);
        cy = lerp(200.0, CHEV_POS.1, p);
        chev_hover = p > 0.75;
    } else if t < 1.0 {
        cx = CHEV_POS.0;
        cy = CHEV_POS.1;
        chev_hover = true;
        chev_open = true;
        cursor_pressed = t < 0.84;
        ripple = clamp01((t - 0.7) / 0.35);
        let pop_p = ease_out_cubic((t - 0.72) / 0.28);
        overflow_alpha = pop_p;
        overflow_dy = lerp(10.0, 0.0, pop_p);
    } else if t < 1.85 {
        chev_open = true;
        overflow_alpha = 1.0;
        overflow_dy = 0.0;
        let p = ease_in_out((t - 1.0) / 0.65);
        cx = lerp(CHEV_POS.0, OVERFLOW_LUMA.0, p);
        cy = lerp(CHEV_POS.1, OVERFLOW_LUMA.1, p);
        icon_hover = p > 0.78;
    } else if t < 3.15 {
        step = 1;
        chev_open = true;
        overflow_alpha = 1.0;
        overflow_dy = 0.0;
        icon_in_overflow = false;
        dragging = true;
        cursor_pressed = true;
        let p = ease_in_out((t - 1.85) / 1.15);
        let mid = (OVERFLOW_LUMA.0 + 28.0, (OVERFLOW_LUMA.1 + PIN_TARGET.1) / 2.0);
        let pos = quad_bezier(OVERFLOW_LUMA, mid, PIN_TARGET, p);
        cx = pos.0;
        cy = pos.1;
        drag_pos = pos;
        pinned_slot_t = clamp01((p - 0.35) / 0.55);
        if t > 3.0 {
            dragging = false;
            cursor_pressed = false;
            icon_pinned = true;
            pinned_slot_t = 1.0;
            ripple = clamp01((t - 3.0) / 0.35);
        }
    } else if t < 3.65 {
        step = 1;
        icon_pinned = true;
        pinned_slot_t = 1.0;
        cx = PIN_TARGET.0;
        cy = PIN_TARGET.1;
        let fade = 1.0 - ease_in_out((t - 3.15) / 0.32);
        overflow_alpha = fade;
        overflow_dy = lerp(0.0, 8.0, 1.0 - fade);
        chev_open = fade > 0.2;
        ripple = clamp01((t - 3.0) / 0.35);
    } else {
        step = 2;
        icon_pinned = true;
        pinned_slot_t = 1.0;
        tray_active = true;

        if t < 4.05 {
            cx = PIN_TARGET.0;
            cy = PIN_TARGET.1;
            cursor_pressed = t < 3.80;
            ripple = clamp01((t - 3.65) / 0.35);
            let fp = ease_out_cubic((t - 3.68) / 0.34);
            flyout_alpha = fp;
            flyout_dy = lerp(12.0, 0.0, fp);
        } else {
            flyout_alpha = 1.0;
            flyout_dy = 0.0;
            // Slider geometry inside 540x356 stage
            let fx = 540.0 - 292.0 - 14.0;
            let fy = 356.0 - 40.0 - 214.0 - 10.0;
            let tx1 = fx + 12.0 + 10.0 + 20.0;
            let tx2 = fx + 292.0 - 12.0 - 10.0 - 38.0;
            let s1_y = fy + 40.0 + 48.0;
            let s2_y = fy + 116.0 + 48.0;

            if t < 4.65 {
                let p = ease_in_out((t - 4.05) / 0.55);
                let target_x = tx1 + (tx2 - tx1) * 0.65;
                cx = lerp(PIN_TARGET.0, target_x, p);
                cy = lerp(PIN_TARGET.1, s1_y, p);
            } else if t < 5.45 {
                let p = ease_in_out((t - 4.65) / 0.75);
                b1 = lerp(65.0, 25.0, p);
                active_slider = 1;
                cursor_pressed = true;
                cx = tx1 + (tx2 - tx1) * (b1 / 100.0);
                cy = s1_y;
            } else if t < 6.25 {
                let p = ease_in_out((t - 5.45) / 0.75);
                b1 = lerp(25.0, 88.0, p);
                active_slider = 1;
                cursor_pressed = true;
                cx = tx1 + (tx2 - tx1) * (b1 / 100.0);
                cy = s1_y;
            } else if t < 6.60 {
                b1 = 88.0;
                let p = ease_in_out((t - 6.25) / 0.35);
                let sx_prev = tx1 + (tx2 - tx1) * 0.88;
                let sx_next = tx1 + (tx2 - tx1) * 0.40;
                cx = lerp(sx_prev, sx_next, p);
                cy = lerp(s1_y, s2_y, p);
            } else if t < 7.15 {
                b1 = 88.0;
                let p = ease_in_out((t - 6.60) / 0.50);
                b2 = lerp(40.0, 78.0, p);
                active_slider = 2;
                cursor_pressed = true;
                cx = tx1 + (tx2 - tx1) * (b2 / 100.0);
                cy = s2_y;
            } else {
                b1 = 88.0;
                b2 = 78.0;
                cx = tx1 + (tx2 - tx1) * 0.78;
                cy = s2_y;
            }
        }
    }

    let dim = lerp(0.45, 0.0, clamp01(b1 / 100.0));

    w.set_active_step(step);
    w.set_global_progress(global_prog);
    w.set_cursor_x(cx);
    w.set_cursor_y(cy);
    w.set_cursor_pressed(cursor_pressed);
    w.set_ripple_progress(ripple);
    w.set_chev_hover(chev_hover);
    w.set_chev_open(chev_open);
    w.set_overflow_alpha(overflow_alpha);
    w.set_overflow_dy(overflow_dy);
    w.set_icon_in_overflow(icon_in_overflow);
    w.set_icon_hover(icon_hover);
    w.set_dragging(dragging);
    w.set_drag_x(drag_pos.0);
    w.set_drag_y(drag_pos.1);
    w.set_pinned_slot_t(pinned_slot_t);
    w.set_icon_pinned(icon_pinned);
    w.set_tray_active(tray_active);
    w.set_flyout_alpha(flyout_alpha);
    w.set_flyout_dy(flyout_dy);
    w.set_b1(b1.round() as i32);
    w.set_b2(b2.round() as i32);
    w.set_active_slider(active_slider);
    w.set_wallpaper_dim(dim);
}

/// Flushes one-time GPU shader compiler & layout pages out of the process working set
/// immediately and after short settling intervals so RAM stays strictly under 1.5–4 MB.
pub fn schedule_ram_trim() {
    trim_working_set();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(450));
        trim_working_set();
        std::thread::sleep(Duration::from_millis(1200));
        trim_working_set();
    });
}
