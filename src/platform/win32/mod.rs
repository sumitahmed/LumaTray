pub mod autostart;
pub mod backdrop;
pub mod flyout_hook;
pub mod power_monitor;
pub mod taskbar;
pub mod tray;

pub use autostart::{is_autostart_enabled, set_autostart};
pub use backdrop::{apply_fluent_backdrop, is_system_in_dark_mode, trim_working_set, BackdropType};
pub use flyout_hook::hook_flyout_dismissal;
pub use power_monitor::PowerSupervisor;
pub use taskbar::{calculate_flyout_anchor, FlyoutBounds};
pub use tray::{TrayDaemon, TrayEvent};
