//! Screen-edge queries for the 2026-10-28 top-edge file line.

use crate::edge_hide::Rect;

/// The monitor under a point: its full bounds (the real screen edge the
/// pointer rests against) and its work area (where the line hangs, so a
/// taskbar docked at the top stays usable).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenEdge {
    pub monitor: Rect,
    pub work: Rect,
    pub dpi_scale: f64,
}

pub fn screen_edge_from_point(x: i32, y: i32) -> Option<ScreenEdge> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(monitor, &mut info).ok().ok()?;
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let dpi_scale = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() && dpi_x > 0 {
            f64::from(dpi_x) / 96.0
        } else {
            1.0
        };
        let rect = |r: windows::Win32::Foundation::RECT| Rect {
            left: f64::from(r.left), top: f64::from(r.top),
            width: f64::from(r.right - r.left), height: f64::from(r.bottom - r.top),
        };
        Some(ScreenEdge { monitor: rect(info.rcMonitor), work: rect(info.rcWork), dpi_scale })
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        None
    }
}

/// A full-screen program in front (game, video, presentation) owns its
/// screen edge, so the line stays out of its way.
pub fn foreground_is_fullscreen() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
        use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};
        use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow, GetWindowRect, IsZoomed};
        if let Ok(state) = SHQueryUserNotificationState() {
            if matches!(state, QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE) {
                return true;
            }
        }
        // Borderless "full screen" windows do not always report a busy state.
        let foreground = GetForegroundWindow();
        // A maximized window also covers the monitor while the taskbar
        // auto-hides; it keeps its title bar and is not full screen.
        if foreground.0.is_null() || IsZoomed(foreground).as_bool() {
            return false;
        }
        let mut class = [0u16; 32];
        let length = GetClassNameW(foreground, &mut class).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..length]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
            return false;
        }
        let mut window = RECT::default();
        if GetWindowRect(foreground, &mut window).is_err() {
            return false;
        }
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            return false;
        }
        covers(window, info.rcMonitor)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn covers(window: windows::Win32::Foundation::RECT, monitor: windows::Win32::Foundation::RECT) -> bool {
    window.left <= monitor.left && window.top <= monitor.top && window.right >= monitor.right && window.bottom >= monitor.bottom
}

#[cfg(all(test, windows))]
mod tests {
    use super::covers;
    use windows::Win32::Foundation::RECT;

    #[test]
    fn only_a_window_covering_the_whole_monitor_counts_as_full_screen() {
        let monitor = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        assert!(covers(monitor, monitor));
        assert!(covers(RECT { left: -1, top: -1, right: 1921, bottom: 1081 }, monitor));
        assert!(!covers(RECT { left: 0, top: 0, right: 1920, bottom: 1040 }, monitor), "a window that leaves the taskbar visible");
        assert!(!covers(RECT { left: 1920, top: 0, right: 3840, bottom: 1080 }, monitor));
    }
}
