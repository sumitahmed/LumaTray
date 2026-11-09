use std::mem::size_of;
use tracing::debug;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW,
    MonitorFromPoint,
    MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlyoutBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Calculates the exact screen position to anchor the flyout window right above the system tray.
/// Anchors to bottom-right of the current monitor's working area (excluding taskbar).
pub fn calculate_flyout_anchor(flyout_width: i32, flyout_height: i32) -> FlyoutBounds {
    const FLYOUT_MARGIN: i32 = 12; // Standard Windows 11 Action Center margin

    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);

        // Find work area for the active monitor nearest to cursor
        let hmon = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO::default();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let work_area = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        };

        // Position at bottom-right of working area (directly above notification area)
        let x = work_area.right - flyout_width - FLYOUT_MARGIN;
        let y = work_area.bottom - flyout_height - FLYOUT_MARGIN;

        debug!(
            "Anchoring flyout at [{}, {}] on work area [{}, {}, {}, {}]",
            x, y, work_area.left, work_area.top, work_area.right, work_area.bottom
        );

        FlyoutBounds {
            x,
            y,
            width: flyout_width,
            height: flyout_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_flyout_anchor() {
        let bounds = calculate_flyout_anchor(360, 320);
        println!("Calculated Flyout Bounds: x={}, y={}, w={}, h={}", bounds.x, bounds.y, bounds.width, bounds.height);
        assert!(bounds.x > 500, "X must be anchored towards right, got {}", bounds.x);
        assert!(bounds.y > 400, "Y must be anchored towards bottom, got {}", bounds.y);
    }
}
