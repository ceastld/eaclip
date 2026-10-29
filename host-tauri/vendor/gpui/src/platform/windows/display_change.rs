//! Monitor replacement without HWND presentation or renderer access.

use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
};

pub(crate) fn replacement_display<T>(
    hwnd: HWND,
    previous_monitor: HMONITOR,
    is_connected: impl FnOnce(HMONITOR) -> bool,
    read_display: impl FnOnce(HMONITOR) -> Option<T>,
) -> Option<T> {
    replacement_display_with(
        previous_monitor,
        is_connected,
        || unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) },
        read_display,
    )
}

fn replacement_display_with<T>(
    previous_monitor: HMONITOR,
    is_connected: impl FnOnce(HMONITOR) -> bool,
    nearest_monitor: impl FnOnce() -> HMONITOR,
    read_display: impl FnOnce(HMONITOR) -> Option<T>,
) -> Option<T> {
    if is_connected(previous_monitor) {
        return None;
    }
    let monitor = nearest_monitor();
    if monitor.is_invalid() {
        // All displays may temporarily disappear during sleep/wake. The next
        // notification retries; never show a window to obtain a monitor.
        return None;
    }
    read_display(monitor)
}

#[cfg(test)]
mod display_change_tests {
    use super::*;
    use windows::{
        Win32::{
            Foundation::RECT, Graphics::Gdi::MONITOR_DEFAULTTONULL, UI::WindowsAndMessaging::*,
        },
        core::w,
    };

    struct OwnedWindow(HWND);

    impl OwnedWindow {
        unsafe fn hidden(transparent: bool) -> Self {
            let mut extended = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            if transparent {
                // GPUI's DirectComposition transparent overlays use this bit.
                extended |= WS_EX_NOREDIRECTIONBITMAP;
            }
            Self(unsafe {
                CreateWindowExW(
                    extended,
                    w!("STATIC"),
                    w!("ClipHost display-change regression fixture"),
                    WS_POPUP,
                    -32000,
                    -32000,
                    120,
                    90,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            })
        }

        unsafe fn snapshot(&self) -> (isize, isize, bool, bool, RECT, HWND) {
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(self.0, &mut rect).unwrap();
                (
                    GetWindowLongPtrW(self.0, GWL_STYLE),
                    GetWindowLongPtrW(self.0, GWL_EXSTYLE),
                    IsWindowVisible(self.0).as_bool(),
                    IsIconic(self.0).as_bool(),
                    rect,
                    GetForegroundWindow(),
                )
            }
        }
    }

    impl Drop for OwnedWindow {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0).unwrap() };
        }
    }

    unsafe fn assert_presentation_unchanged(transparent: bool, state_bits: WINDOW_STYLE) {
        let window = unsafe { OwnedWindow::hidden(transparent) };
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
        // These style-only fixtures do not call ShowWindow or SetWindowPos.
        // They exercise visible/minimized state preservation without presenting
        // a test window on the user's desktop or acquiring foreground focus.
        unsafe {
            let style = GetWindowLongPtrW(window.0, GWL_STYLE);
            SetWindowLongPtrW(window.0, GWL_STYLE, style | state_bits.0 as isize);
        }
        let before = unsafe { window.snapshot() };
        assert_eq!(before.2, state_bits.contains(WS_VISIBLE));
        assert_eq!(before.3, state_bits.contains(WS_MINIMIZE));
        assert_ne!(before.1 & WS_EX_NOACTIVATE.0 as isize, 0);
        assert_eq!(
            before.1 & WS_EX_NOREDIRECTIONBITMAP.0 as isize != 0,
            transparent,
        );
        let monitor = replacement_display(window.0, HMONITOR::default(), |_| false, Some);
        assert!(monitor.is_some_and(|monitor| !monitor.is_invalid()));
        assert_eq!(unsafe { window.snapshot() }, before);
        assert_ne!(before.5, window.0);
    }

    #[test]
    fn display_change_hidden_main_window_stays_hidden() {
        unsafe { assert_presentation_unchanged(false, WINDOW_STYLE::default()) };
    }

    #[test]
    fn display_change_hidden_transparent_overlay_stays_hidden() {
        unsafe { assert_presentation_unchanged(true, WINDOW_STYLE::default()) };
    }

    #[test]
    fn display_change_visible_state_does_not_activate_or_move() {
        unsafe { assert_presentation_unchanged(true, WS_VISIBLE) };
    }

    #[test]
    fn display_change_minimized_state_is_not_restored() {
        unsafe { assert_presentation_unchanged(false, WS_VISIBLE | WS_MINIMIZE) };
    }

    #[test]
    fn display_change_offscreen_window_uses_nearest_monitor_without_showing() {
        let window = unsafe { OwnedWindow::hidden(true) };
        assert!(unsafe { MonitorFromWindow(window.0, MONITOR_DEFAULTTONULL) }.is_invalid());
        let expected = unsafe { MonitorFromWindow(window.0, MONITOR_DEFAULTTONEAREST) };
        assert!(!expected.is_invalid());
        assert_eq!(
            replacement_display(window.0, HMONITOR::default(), |_| false, Some),
            Some(expected),
        );
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
    }

    #[test]
    fn display_change_connected_monitor_does_not_resolve_a_replacement() {
        assert_eq!(
            replacement_display_with::<()>(
                HMONITOR::default(),
                |_| true,
                || panic!("connected displays do not need replacement"),
                |_| panic!("connected displays must not be reread"),
            ),
            None,
        );
    }

    #[test]
    fn display_change_no_monitor_retains_previous_display_and_retries() {
        let window = unsafe { OwnedWindow::hidden(false) };
        let before = unsafe { window.snapshot() };
        let mut display = 17;
        let absent = replacement_display_with(
            HMONITOR::default(),
            |_| false,
            HMONITOR::default,
            |_| panic!("no display can be read while all monitors are absent"),
        );
        if let Some(replacement) = absent {
            display = replacement;
        }
        assert_eq!(display, 17);
        assert_eq!(unsafe { window.snapshot() }, before);
        if let Some(replacement) =
            replacement_display(window.0, HMONITOR::default(), |_| false, |_| Some(23))
        {
            display = replacement;
        }
        assert_eq!(display, 23);
        assert_eq!(unsafe { window.snapshot() }, before);
    }

    #[test]
    fn display_change_racing_monitor_read_can_retry_without_changing_window() {
        let window = unsafe { OwnedWindow::hidden(true) };
        let before = unsafe { window.snapshot() };
        assert_eq!(
            replacement_display::<()>(window.0, HMONITOR::default(), |_| false, |_| None),
            None,
        );
        assert_eq!(
            replacement_display(window.0, HMONITOR::default(), |_| false, |_| Some(23)),
            Some(23),
        );
        assert_eq!(unsafe { window.snapshot() }, before);
    }
}
