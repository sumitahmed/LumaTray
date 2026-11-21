# LumaTray Architecture and Developer Reference

This document describes the system design, threading model, module layout, and Windows platform behavior of LumaTray (`v0.2.0`) for future maintenance and feature development.

---

## 1. System Architecture

LumaTray separates hardware I/O, application state, OS integration, and UI rendering across four distinct layers:

```
+-------------------------------------------------------------------+
|                        Slint UI Layer (ui/)                       |
|   MainWindow (Flyout) | SettingsWindow (Lazy) | WelcomeWindow     |
+---------------------------------+---------------------------------+
                                  |
                                  v
+-------------------------------------------------------------------+
|                   UI Bridge (src/ui_bridge/mod.rs)                |
|  VecModel<DisplayItem>, DPI-aware sizing, lazy window lifecycle,  |
|  theme/accent synchronization, working-set trim scheduling        |
+----------------+---------------------------------+----------------+
                 |                                 ^
                 v                                 | (invoke_from_event_loop)
+--------------------------------+  +--------------+----------------+
|   Core State (src/core/)       |  | Win32 Platform (src/platform/)|
|   - StateStore (RwLock)        |  | - TrayDaemon (dedicated loop) |
|   - DisplaySyncEngine          |  | - FlyoutHook (WM_ACTIVATE)    |
|   - ConfigManager (JSON)       |  | - Taskbar anchor & HiDPI      |
|   - HardwareWorkerHandle       |  | - DWM Backdrop & WM_SETICON   |
+----------------+---------------+  | - Installer & Autostart       |
                 |                  | - PowerSupervisor             |
                 | (crossbeam)      +-------------------------------+
                 v
+-------------------------------------------------------------------+
|            Hardware Worker Thread (src/core/worker.rs)            |
|      WriteCoalescer (30ms / 50ms / 100ms per-display throttle)    |
+---------------------------------+---------------------------------+
                                  |
                                  v
+-------------------------------------------------------------------+
|                Hardware Abstraction Layer (src/hal/)              |
|                  trait DisplayDriver: Send + Sync                 |
|                                 |                                 |
|                WindowsDisplayManager (Composite)                  |
|          +----------------------+----------------------+          |
|          |                                             |          |
|          v                                             v          |
|   WindowsDdcCiDriver                            WindowsWmiDriver  |
|   - dxva2.dll VCP 0x10/0x13/0x6B/0x12           - ROOT\WMI COM    |
|   - QueryDisplayConfig + EnumDisplaySettingsW   - WmiMonitorID    |
+-------------------------------------------------------------------+
```

### Design Principles

1. **Non-blocking UI**: DDC/CI I2C calls take 20-50ms per monitor and can block on driver timeouts. All hardware calls run on a dedicated `luma-hardware-worker` thread, never on the Slint UI thread.
2. **Driver Decoupling**: `DisplayDriver` (`src/hal/mod.rs`) isolates OS-specific display APIs from `StateStore`, `WriteCoalescer`, and the UI. Future Linux (`ddcutil`) or macOS backends can implement `DisplayDriver` without changing `core` or `ui`.
3. **Minimal Idle RAM**: `SettingsWindow` and `WelcomeWindow` are instantiated on demand and dropped (`take()`) when closed. After opening/closing windows, `schedule_ram_trim()` flushes unreferenced pages via `SetProcessWorkingSetSize`.
4. **Zero Heavy Media Assets**: Icons and the onboarding animation are pure Slint `Path` vectors and rectangles; the only binary assets are `assets/tray.ico` and `assets/app.ico`.

---

## 2. Tech Stack

| Component | Crate / API | Version | Purpose |
| :--- | :--- | :--- | :--- |
| Language | Rust | Edition 2024 | Memory safety, single-binary compilation, zero GC |
| UI Framework | `slint`, `slint-build` | `1.18.1` | Declarative `.slint` UI compiled to native Rust, GPU-accelerated via FemtoVG/Skia |
| Win32 Bindings | `windows` | `0.62.2` | Direct bindings for Display/DDC, WMI/COM, Shell, DWM, Registry, GDI, HiDPI |
| Concurrency | `crossbeam-channel` | `0.5.17` | MPSC channels for tray events and hardware commands |
| Synchronization | `parking_lot` | `0.13.0` | Low-overhead `RwLock` / `Mutex` for `AppState` and hooks |
| Serialization | `serde`, `serde_json` | `1.0.229` / `1.0.154` | Persistent configuration in `%APPDATA%\LumaTray\config.json` |
| Error Handling | `thiserror` | `2.0.21` | Typed `DriverError` variants |
| Logging | `tracing`, `tracing-subscriber` | `0.1.44` / `0.3.23` | Structured file logging to `%APPDATA%\LumaTray\luma.log` |
| Resource Linker | `windres` (MSYS2 UCRT64) | External tool | Compiles `assets/resource.rc` into COFF object for `.exe` icon embedding |

---

## 3. Threading & Runtime Model

LumaTray runs four threads at steady state:

1. **Main / Slint UI Thread (`main.rs`)**:
   - Runs `slint::run_event_loop_until_quit()`.
   - Owns `UiBridge`, `MainWindow`, and optional `SettingsWindow` / `WelcomeWindow` instances.
   - Note: Slint's `window.hide()` can signal the event loop to quit when the last window closes; therefore, hiding the tray flyout uses Win32 `ShowWindow(hwnd, SW_HIDE)` directly rather than `MainWindow::hide()`.
2. **Hardware Worker Thread (`luma-hardware-worker` in `src/core/worker.rs`)**:
   - Owns `Box<dyn DisplayDriver>` and `WriteCoalescer`.
   - Blocks on `cmd_rx.recv_timeout(...)` (`500ms` when idle, `20ms` when deferred writes are queued) and flushes coalesced brightness/contrast writes.
3. **System Tray Daemon Thread (`luma-tray-daemon` in `src/platform/win32/tray.rs`)**:
   - Owns a hidden message-only window (`LumaTrayMessageWindowClass`) and runs a blocking `GetMessageW` / `DispatchMessageW` loop (0% CPU when idle).
   - Listens for `WM_TRAYICON`, `WM_COMMAND`, `WM_DISPLAYCHANGE`, `WM_POWERBROADCAST`, `TaskbarCreated`, and `UxdDisplayChangeMessage`.
4. **Tray Event Dispatcher Thread (`luma-tray-event-dispatcher` in `src/main.rs`)**:
   - Consumes `TrayEvent` from `tray_rx` and marshals UI actions (`toggle_flyout`, `sync_from_state`, `open_settings`, `quit_event_loop`) onto the Slint thread via `slint::invoke_from_event_loop`.

---

## 4. Module-by-Module Reference

### Entry Point & Build

- `src/main.rs`: Initializes file logging (`%APPDATA%\LumaTray\luma.log`) and panic hook, runs `ensure_installed()`, spawns `HardwareWorkerHandle`, enumerates initial displays, creates `StateStore`, self-heals autostart (`sync_autostart_on_boot`), spawns `TrayDaemon` and event dispatcher threads, initializes `UiBridge`, trims working set, and starts Slint's event loop.
- `src/lib.rs`: Exposes `core`, `hal`, `platform`, and `ui_bridge` modules.
- `build.rs`: Compiles `ui/main.slint` via `slint_build::compile` and invokes `windres` on `assets/resource.rc` to link `tray.ico` (Resource ID `1`) and `app.ico` (Resource ID `2`) into the binary.
- `Cargo.toml`: Release profile uses `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, and `strip = true`.

### Hardware Abstraction Layer (`src/hal/`)

- `src/hal/mod.rs`: Defines `trait DisplayDriver: Send + Sync` (`enumerate`, `get_brightness`, `set_brightness`, `get_contrast`, `set_contrast`, `get_vcp`, `set_vcp`).
- `src/hal/descriptor.rs`: Defines `DriverType` (`DdcCi`, `WmiInternal`, `SoftwareGamma`), `DisplayCapabilities`, and `DisplayDescriptor` (including `id`, `name`, `gdi_name`, `resolution`, `refresh_rate_hz`, `is_internal`).
- `src/hal/error.rs`: Defines `DriverError` enum (`Win32`, `Wmi`, `MonitorNotFound`, `Unsupported`, `DdcTimeout`, `Internal`).
- `src/hal/windows/mod.rs`: `WindowsDisplayManager` implements `DisplayDriver` by combining `WindowsDdcCiDriver` and `WindowsWmiDriver`.
  - Runs up to 3 DDC/CI enumeration attempts (80ms apart) so external monitors are not dropped on transient driver delays.
  - Deduplicates internal laptop panels: extracts panel hardware codes (e.g., `CMN1618`) from WMI instances and filters matching entries out of the DDC/CI list so the laptop display is only controlled via WMI.
- `src/hal/windows/ddc_ci.rs`: `WindowsDdcCiDriver` enumerates `HMONITOR` handles via `EnumDisplayMonitors`, retrieves `PHYSICAL_MONITOR` handles (`GetPhysicalMonitorsFromHMONITOR`), maps permanent device paths and friendly names from `get_display_config_targets()`, probes brightness VCP codes in order (`0x10`, `0x13`, `0x6B`) with fallback to `0x10`, probes contrast (`0x12`), and retries transient DDC/CI errors (`0xC0262580..=0xC0262585`, `ERROR_GEN_FAILURE`, `ERROR_BUSY`) up to 3 times.
- `src/hal/windows/wmi.rs`: `WindowsWmiDriver` connects to `ROOT\WMI` via COM, reads friendly names from `WmiMonitorID`, resolves `__PATH` from `WmiMonitorBrightnessMethods`, reads current brightness from `WmiMonitorBrightness`, and sets brightness via `ExecMethod("WmiSetBrightness")`. Matches WMI instance IDs against `DisplayConfigTarget` to attach real GDI resolution and refresh rate.
- `src/hal/windows/display_config.rs`: Wraps `GetDisplayConfigBufferSizes`, `QueryDisplayConfig`, and `DisplayConfigGetDeviceInfo` (`QDC_ONLY_ACTIVE_PATHS`) to obtain stable monitor device paths and EDID friendly names, plus `query_gdi_display_mode` (`EnumDisplaySettingsW`) for active resolution and Hz.

### Core State & Synchronization (`src/core/`)

- `src/core/coalescer.rs`: `WriteCoalescer` maintains per-`(display_id, feature)` slots. The first write executes immediately (`CoalesceAction::WriteNow`); rapid subsequent writes within `interval` overwrite a single `pending` slot (`CoalesceAction::Defer`) and are flushed by `HardwareWorker` once the interval elapses.
- `src/core/worker.rs`: `HardwareWorker` and `HardwareWorkerHandle` process `HardwareCommand` variants (`Enumerate`, `SetBrightness`, `SetContrast`, `SetVcp`, `ReadBrightness`, `SetThrottleInterval`, `FlushPending`, `Shutdown`).
- `src/core/sync_engine.rs`: `PiecewiseCurve` and `DisplaySyncEngine` perform forward (`map_forward`) and reverse (`map_reverse`) piecewise linear interpolation across `CalibrationPoint`s (`0..=100`).
- `src/core/config.rs`: `ConfigManager` loads and saves `AppConfig` and per-monitor `DisplayConfig` as JSON at `%APPDATA%\LumaTray\config.json`.
- `src/core/state.rs`: `StateStore` coordinates `AppState` under `RwLock`, maps UI `0..=100` slider percentages to per-monitor `[min_limit..=max_limit]` hardware values (`map_to_hardware_range`), persists state on changes, and provides `reset_to_defaults()`.

### Windows Platform Layer (`src/platform/win32/`)

- `src/platform/win32/tray.rs`: `TrayDaemon` registers the notification area icon (`Shell_NotifyIconW`) using Resource ID `1`, creates the right-click context menu (`Refresh Displays`, `Settings...`, `Exit LumaTray`), handles Explorer restarts (`TaskbarCreated`), and forwards `WM_DISPLAYCHANGE` / `WM_POWERBROADCAST` events.
- `src/platform/win32/power_monitor.rs`: `PowerSupervisor` tracks `PBT_APMSUSPEND` and `PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND` with a 15-second `WAKE_GRACE_PERIOD`.
- `src/platform/win32/taskbar.rs`: `calculate_flyout_anchor` inspects the monitor work area nearest the cursor (`MonitorFromPoint` + `GetMonitorInfoW`) and computes bottom-right coordinates with a `12px` margin above the taskbar.
- `src/platform/win32/flyout_hook.rs`: Subclasses the flyout `HWND` (`SetWindowLongPtrW` / `GWLP_WNDPROC`) to detect `WM_ACTIVATE(WA_INACTIVE)` and auto-dismiss the flyout when clicking outside, ignoring transient deactivation within 250ms of showing (`notify_flyout_shown`).
- `src/platform/win32/backdrop.rs`: Reads `AppsUseLightTheme` from `HKCU\...\Themes\Personalize`, sets `DWMWA_USE_IMMERSIVE_DARK_MODE`, `DWMWA_WINDOW_CORNER_PREFERENCE` (`DWMWCP_ROUND`), `DWMWA_SYSTEMBACKDROP_TYPE` (`DWMSBT_TRANSIENTWINDOW` for Acrylic / `DWMSBT_MAINWINDOW` for Mica), assigns `WM_SETICON` (`ICON_SMALL` / `ICON_BIG`), and exposes `trim_working_set()`.
- `src/platform/win32/installer.rs`: `ensure_installed()` copies the running binary to `%LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe`, builds `%APPDATA%\Microsoft\Windows\Start Menu\Programs\LumaTray.lnk` via COM `IShellLinkW`, writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\LumaTray`, and provides `open_external_url()` (`ShellExecuteW` with `cmd /C start` fallback).
- `src/platform/win32/autostart.rs`: Manages `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\LumaTray` and `Explorer\StartupApproved\Run\LumaTray`, validates that the registered path exists on disk (`is_autostart_enabled`), and repairs stale paths on boot (`sync_autostart_on_boot`).

### UI Bridge & Slint Views (`src/ui_bridge/`, `ui/`)

- `src/ui_bridge/mod.rs`: `UiBridge` binds `DisplayItem` `VecModel` rows, propagates brightness/contrast/sync changes, resizes and repositions the flyout via `LogicalSize` + `SetWindowPos(..., SWP_NOSIZE)` to handle multi-DPI scaling cleanly, and manages lazy creation/destruction of `SettingsWindow` and `WelcomeWindow`.
  - **Important Slint detail**: Each Slint `Window` component (`MainWindow`, `SettingsWindow`, `WelcomeWindow`) has its own `global Theme` instance. Any theme (`is-dark`) or accent (`accent-id`) update must be pushed to all currently open windows.
- `ui/theme.slint`: `global Theme` defining dark/light surface colors and 6 accent presets (`cyan`, `purple`, `emerald`, `amber`, `rose`, `blue`).
- `ui/icons.slint`: `FluentToggle` and pure vector `Path` icons (`MonitorIcon`, `SunIcon`, `ContrastIcon`, `LinkIcon`, `GearIcon`, `RefreshIcon`, `PaletteIcon`, `InfoIcon`, `SparkleIcon`, `CursorIcon`, `ChevronDownIcon`, `ChevronUpIcon`, `ArrowLeftIcon`).
- `ui/slider.slint`: Custom `FluentSlider` supporting pointer click/drag and mouse-wheel steps (`+/- 2%`).
- `ui/monitor_card.slint`: `DisplayItem` struct and `MonitorCard` component (name, resolution/Hz, driver badge, link toggle, brightness slider, optional contrast slider).
- `ui/main.slint`: `MainWindow` borderless flyout (draggable header, sync pill, refresh/settings buttons, master slider, monitor list, footer with GitHub link).
- `ui/settings.slint`: `SettingsWindow` with sidebar navigation (`General`, `Monitor Settings`, `Appearance & Colors`, `About`), width-locked `Flickable` content pane (`viewport-width: pane.width`), and factory reset triggers.
- `ui/welcome.slint`: `WelcomeWindow` ("Hello, LumaTray!") 3-step tray-pinning walkthrough driven by `welcome_timer` (`TimerMode::Repeated`, 2.8s interval).

---

## 5. Configuration & Data Paths

| Artifact | Location | Notes |
| :--- | :--- | :--- |
| Installed Binary | `%LOCALAPPDATA%\Programs\LumaTray\LumaTray.exe` | Canonical executable used by Start Menu and Startup |
| Start Menu Shortcut | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\LumaTray.lnk` | Enables Windows Search App indexing |
| Configuration | `%APPDATA%\LumaTray\config.json` | Serialized `AppConfig` and per-display `DisplayConfig` |
| Runtime Log | `%APPDATA%\LumaTray\luma.log` | Appended `tracing` output |
| Autostart Registry | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\LumaTray` | Quoted path to installed binary |
| Startup Approval | `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run\LumaTray` | 12-byte binary enabled state (`0x02`) |
| Uninstall Metadata | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\LumaTray` | DisplayName, Version, Publisher, Icon, URLInfoAbout |

---

## 6. Known Caveats & Maintenance Notes

1. **Slint Scroll Containers**: Avoid `std-widgets.slint` `ScrollView` when embedding cards with long device paths; use `Flickable` with explicit `viewport-width: pane.width` (or `content-width` in newer Slint versions) and `min-width: 0px` on `horizontal-stretch: 1` children so text wraps instead of triggering horizontal scrolling.
2. **DWM Backdrop & Light Mode**: On Windows 11, calling `SetWindowPos` before `ShowWindow` / `apply_fluent_backdrop` ensures DWM applies rounded corners and Mica/Acrylic without a flash.
3. **DDC/CI Quirks**: Some monitors return transient errors right after sleep or input switching. `ddc_ci.rs` and `windows/mod.rs` retain 3-attempt retry loops and fall back to VCP `0x10` rather than hiding the monitor if initial capability reads time out.
4. **Build Environment**: `build.rs` calls `windres` directly. Ensure MSYS2 UCRT64 (`windres.exe`) or an equivalent GNU `windres` binary is on `PATH` when building on Windows.
