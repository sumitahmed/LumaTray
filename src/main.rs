#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use crossbeam_channel::unbounded;
use tracing::{error, info};

use luma_tray::hal::windows::WindowsDisplayManager;
use luma_tray::core::worker::HardwareWorkerHandle;
use luma_tray::core::state::StateStore;
use luma_tray::platform::win32::tray::{TrayDaemon, TrayEvent};
use luma_tray::platform::win32::power_monitor::PowerSupervisor;
use luma_tray::ui_bridge::UiBridge;

thread_local! {
    static ACTIVE_UI: RefCell<Option<Rc<UiBridge>>> = const { RefCell::new(None) };
}

fn main() {
    // DO NOT force software renderer. Let Slint use the native GPU backend
    // (FemtoVG / skia) for hardware-accelerated, smooth 120Hz+ rendering.
    // The old `set_var("SLINT_BACKEND", "software")` caused choppy 15 FPS rendering.

    // Create APPDATA directory for logs and config if it doesn't exist
    let appdata = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_dir = appdata.join("LumaTray");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("luma.log");

    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(log_file)
    {
        tracing_subscriber::fmt()
            .with_writer(std::sync::Arc::new(file))
            .init();
    } else {
        tracing_subscriber::fmt::init();
    }

    std::panic::set_hook(Box::new(|info| {
        error!("PANIC OCCURRED: {:?}", info);
    }));

    info!("LumaTray v0.2.0 starting");

    // 0. Ensure LumaTray is installed in %LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe
    //    and registered as an official Windows App in Start Menu Programs (.lnk)
    let installed_path = luma_tray::platform::win32::ensure_installed();
    info!("Verified Windows App installation at {:?}", installed_path);

    // 1. Initialize composite Hardware Driver (DDC/CI + WMI)
    let driver = Box::new(WindowsDisplayManager::new());

    // 2. Spawn OS Hardware Worker Actor (50ms rate limiter)
    let worker = HardwareWorkerHandle::spawn(driver, Duration::from_millis(50));

    // 3. Initial Display Enumeration
    let descriptors = worker.enumerate().unwrap_or_default();
    info!("{} display(s) detected", descriptors.len());
    for desc in &descriptors {
        info!(
            " -> [{}] {} ({:?}, {} @ {} Hz)",
            desc.id, desc.name, desc.driver_type, desc.resolution, desc.refresh_rate_hz
        );
    }

    // 4. Initialize Core StateStore & self-heal Windows Autostart registry path
    let state_store = Arc::new(StateStore::new(worker.clone()));
    state_store.update_inventory(descriptors);
    let autostart_active = luma_tray::platform::win32::sync_autostart_on_boot(
        state_store.snapshot().config.start_with_windows,
    );
    if autostart_active && !state_store.snapshot().config.start_with_windows {
        state_store.update_preferences(|cfg| {
            cfg.start_with_windows = true;
        });
    }

    // 5. Initialize Power Supervisor (sleep/wake storm suppressor)
    let power_supervisor = Arc::new(PowerSupervisor::new());

    // 6. Spawn Windows System Tray Daemon in dedicated thread
    let (tray_tx, tray_rx) = unbounded();
    let power_for_tray = power_supervisor.clone();

    thread::Builder::new()
        .name("luma-tray-daemon".into())
        .spawn(move || {
            let _tray = match TrayDaemon::spawn(tray_tx, power_for_tray) {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to initialize TrayDaemon: {:?}", e);
                    return;
                }
            };

            TrayDaemon::run_message_loop();
        })
        .expect("Failed to spawn tray thread");

    // 7. Initialize UI Bridge
    //    The window starts HIDDEN. It only appears when the user clicks the tray icon.
    let ui_bridge = UiBridge::new(state_store.clone(), worker.clone())
        .expect("Failed to initialize Slint UI");

    // Register UI bridge on UI thread-local
    ACTIVE_UI.with(|cell| {
        *cell.borrow_mut() = Some(ui_bridge.clone());
    });

    // 8. Event Dispatcher Thread for System Tray Events
    let store_for_events = state_store.clone();
    let worker_for_events = worker.clone();

    thread::Builder::new()
        .name("luma-tray-event-dispatcher".into())
        .spawn(move || {
            while let Ok(event) = tray_rx.recv() {
                match event {
                    TrayEvent::ToggleFlyout => {
                        let _ = slint::invoke_from_event_loop(|| {
                            ACTIVE_UI.with(|cell| {
                                if let Some(ui) = cell.borrow().as_ref() {
                                    ui.toggle_flyout();
                                }
                            });
                        });
                    }
                    TrayEvent::RefreshDisplays | TrayEvent::DisplayConfigChanged => {
                        let worker = worker_for_events.clone();
                        let store = store_for_events.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Ok(descriptors) = worker.enumerate() {
                                store.update_inventory(descriptors);
                            }
                            ACTIVE_UI.with(|cell| {
                                if let Some(ui) = cell.borrow().as_ref() {
                                    ui.sync_from_state();
                                }
                            });
                        });
                    }
                    TrayEvent::OpenSettings => {
                        let _ = slint::invoke_from_event_loop(|| {
                            ACTIVE_UI.with(|cell| {
                                if let Some(ui) = cell.borrow().as_ref() {
                                    ui.open_settings();
                                }
                            });
                        });
                    }
                    TrayEvent::Exit => {
                        info!("Exit requested from system tray.");
                        let _ = slint::invoke_from_event_loop(|| {
                            let _ = slint::quit_event_loop();
                        });
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn tray event dispatcher thread");

    info!("LumaTray resident in notification area. Click tray icon to toggle flyout.");

    info!("Trimming initial working set...");
    luma_tray::platform::win32::trim_working_set();

    info!("Starting Slint event loop...");
    let run_res = slint::run_event_loop_until_quit();
    info!("Slint run_event_loop_until_quit returned: {:?}", run_res);

    info!("LumaTray exited cleanly.");
}
