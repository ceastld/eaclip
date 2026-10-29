//! Clipboard-only native resize hit testing. No GPUI/App borrow crosses the
//! synchronous Windows size loop, and ordinary controls keep their own cursors.

#[cfg(windows)]
mod win {
    use std::{cell::RefCell, collections::HashSet};
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::ScreenToClient,
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    };

    const RESIZE_BORDER_DIP: u32 = 3;

    thread_local! {
        static ROOTS: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
    }

    pub(crate) fn register(hwnd: HWND) -> Result<(), String> {
        unsafe {
            let mut pid = 0;
            if hwnd.0.is_null()
                || GetWindowThreadProcessId(hwnd, Some(&mut pid)) != GetCurrentThreadId()
                || pid != GetCurrentProcessId()
            {
                return Err("Clipboard resize policy requires an owned UI-thread window.".into());
            }
        }
        ROOTS.with(|roots| roots.borrow_mut().insert(hwnd.0 as isize));
        Ok(())
    }

    fn registered(hwnd: HWND) -> bool {
        ROOTS.with(|roots| roots.borrow().contains(&(hwnd.0 as isize)))
    }

    fn resize_hit(hit: u32) -> bool {
        matches!(
            hit,
            HTLEFT
                | HTRIGHT
                | HTTOP
                | HTBOTTOM
                | HTTOPLEFT
                | HTTOPRIGHT
                | HTBOTTOMLEFT
                | HTBOTTOMRIGHT
        )
    }

    fn border_pixels(dpi: u32) -> i32 {
        ((u64::from(RESIZE_BORDER_DIP) * u64::from(dpi) + 48) / 96).clamp(1, i32::MAX as u64) as i32
    }

    fn client_hit(x: i32, y: i32, width: i32, height: i32, border: i32) -> u32 {
        if x < 0 || y < 0 || x >= width || y >= height {
            return HTCLIENT;
        }
        let border = border.min(width / 2).min(height / 2);
        match (
            x < border,
            x >= width - border,
            y < border,
            y >= height - border,
        ) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            (true, _, _, _) => HTLEFT,
            (_, true, _, _) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ => HTCLIENT,
        }
    }

    unsafe fn resizable(hwnd: HWND) -> bool {
        GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_THICKFRAME.0 as isize != 0
            && !IsZoomed(hwnd).as_bool()
            && !IsIconic(hwnd).as_bool()
    }

    /// `caption_hit` reads only the last painted caption cache. Missing or stale
    /// geometry is ordinary client, never GPUI's extra, style-blind top grip.
    pub(crate) unsafe fn handle_message(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
        caption_hit: impl FnOnce(isize, i32, i32, i32, i32, u32) -> Option<bool>,
    ) -> Option<LRESULT> {
        if message == WM_NCDESTROY {
            ROOTS.with(|roots| roots.borrow_mut().remove(&(hwnd.0 as isize)));
            return None;
        }
        if !registered(hwnd) {
            return None;
        }
        if message == WM_NCLBUTTONDOWN && resize_hit(wp.0 as u32) {
            // GPUI forwards non-client presses through ordinary element input
            // first. A button there can swallow the press despite the HT cursor.
            return Some(if resizable(hwnd) {
                DefWindowProcW(hwnd, message, wp, lp)
            } else {
                LRESULT(0)
            });
        }
        if message != WM_NCHITTEST {
            return None;
        }

        let mut point = POINT {
            x: lp.0 as u16 as i16 as i32,
            y: (lp.0 >> 16) as u16 as i16 as i32,
        };
        let screen = point;
        let mut client = RECT::default();
        if !ScreenToClient(hwnd, &mut point).as_bool() || GetClientRect(hwnd, &mut client).is_err()
        {
            return Some(LRESULT(HTCLIENT as isize));
        }
        let (width, height) = (client.right - client.left, client.bottom - client.top);
        let can_resize = resizable(hwnd);
        if point.x < 0 || point.y < 0 || point.x >= width || point.y >= height {
            // Retain the real Windows non-client frame/outside-window result.
            // The narrow custom band applies only inside the rendered client.
            let mut outer = RECT::default();
            if GetWindowRect(hwnd, &mut outer).is_err()
                || screen.x < outer.left
                || screen.x >= outer.right
                || screen.y < outer.top
                || screen.y >= outer.bottom
            {
                return Some(LRESULT(HTNOWHERE as isize));
            }
            let native = DefWindowProcW(hwnd, message, wp, lp);
            return Some(if !can_resize && resize_hit(native.0 as u32) {
                LRESULT(HTCLIENT as isize)
            } else {
                native
            });
        }
        let dpi = GetDpiForWindow(hwnd);
        if can_resize {
            let hit = client_hit(point.x, point.y, width, height, border_pixels(dpi));
            if resize_hit(hit) {
                return Some(LRESULT(hit as isize));
            }
        }
        Some(LRESULT(
            if caption_hit(hwnd.0 as isize, point.x, point.y, width, height, dpi).unwrap_or(false) {
                HTCAPTION as isize
            } else {
                HTCLIENT as isize
            },
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{cell::Cell, sync::Once};
        use windows::{
            core::w,
            Win32::{
                Foundation::HINSTANCE,
                Graphics::Gdi::ClientToScreen,
                System::LibraryLoader::GetModuleHandleW,
                UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            },
        };

        #[derive(Default)]
        struct Probe {
            caption: Cell<Option<bool>>,
            lower_nc_presses: Cell<usize>,
            lower_client_presses: Cell<usize>,
            size_commands: Cell<usize>,
        }

        unsafe extern "system" fn base_proc(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            if msg == WM_NCCALCSIZE && wp.0 != 0 {
                let rect = &mut (*(lp.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0];
                rect.left += 6;
                rect.top += 6;
                rect.right -= 6;
                rect.bottom -= 6;
                return LRESULT(0);
            }
            if let Some(probe) = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe).as_ref() {
                if msg == WM_NCHITTEST {
                    return LRESULT(HTTOP as isize);
                }
                if msg == WM_NCLBUTTONDOWN {
                    probe.lower_nc_presses.set(probe.lower_nc_presses.get() + 1);
                    return LRESULT(0); // Stand in for GPUI swallowing an element press.
                }
                if msg == WM_LBUTTONDOWN {
                    probe
                        .lower_client_presses
                        .set(probe.lower_client_presses.get() + 1);
                    return LRESULT(0);
                }
                if msg == WM_SYSCOMMAND && wp.0 as u32 & 0xfff0 == SC_SIZE {
                    probe.size_commands.set(probe.size_commands.get() + 1);
                    return LRESULT(0); // Never enter an interactive desktop size loop.
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        unsafe extern "system" fn subclass(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
            id: usize,
            _: usize,
        ) -> LRESULT {
            let result = handle_message(hwnd, msg, wp, lp, |_, _, _, _, _, _| {
                (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe)
                    .as_ref()
                    .and_then(|probe| probe.caption.get())
            });
            if msg == WM_NCDESTROY {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass), id);
            }
            result.unwrap_or_else(|| DefSubclassProc(hwnd, msg, wp, lp))
        }

        struct Fixture {
            hwnd: HWND,
            probe: Box<Probe>,
        }
        impl Fixture {
            unsafe fn new(managed: bool) -> Self {
                static CLASS: Once = Once::new();
                CLASS.call_once(|| {
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(base_proc),
                        hInstance: HINSTANCE(GetModuleHandleW(None).unwrap().0),
                        lpszClassName: w!("ClipHostClipboardResizeTest"),
                        ..Default::default()
                    };
                    assert_ne!(RegisterClassW(&class), 0);
                });
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("ClipHostClipboardResizeTest"),
                    w!(""),
                    WS_OVERLAPPEDWINDOW,
                    -1200,
                    -800,
                    480,
                    360,
                    None,
                    None,
                    Some(HINSTANCE(GetModuleHandleW(None).unwrap().0)),
                    None,
                )
                .unwrap();
                let probe = Box::<Probe>::default();
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*probe as *const Probe as isize);
                assert!(SetWindowSubclass(hwnd, Some(subclass), 1, 0).as_bool());
                if managed {
                    register(hwnd).unwrap();
                }
                Self { hwnd, probe }
            }
            unsafe fn screen_point(&self, x: i32, y: i32) -> LPARAM {
                let mut point = POINT { x, y };
                assert!(ClientToScreen(self.hwnd, &mut point).as_bool());
                LPARAM((((point.y as u16 as u32) << 16) | point.x as u16 as u32) as isize)
            }
            unsafe fn send(&self, msg: u32, wp: usize, lp: LPARAM) -> LRESULT {
                SendMessageW(self.hwnd, msg, Some(WPARAM(wp)), Some(lp))
            }
            unsafe fn hit(&self, x: i32, y: i32) -> u32 {
                self.send(WM_NCHITTEST, 0, self.screen_point(x, y)).0 as u32
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    if !self.hwnd.0.is_null() {
                        DestroyWindow(self.hwnd).unwrap();
                    }
                }
            }
        }

        #[test]
        fn three_dip_band_preserves_corners_without_the_old_eight_dip_interior() {
            for (dpi, border) in [(96, 3), (120, 4), (144, 5)] {
                assert_eq!(border_pixels(dpi), border);
                assert_eq!(client_hit(0, 0, 420, 600, border), HTTOPLEFT);
                assert_eq!(client_hit(419, 0, 420, 600, border), HTTOPRIGHT);
                assert_eq!(client_hit(0, 599, 420, 600, border), HTBOTTOMLEFT);
                assert_eq!(client_hit(419, 599, 420, 600, border), HTBOTTOMRIGHT);
                assert_eq!(client_hit(border - 1, 30, 420, 600, border), HTLEFT);
                assert_eq!(client_hit(border, 30, 420, 600, border), HTCLIENT);
                assert_eq!(client_hit(420 - border, 30, 420, 600, border), HTRIGHT);
                assert_eq!(client_hit(419 - border, 30, 420, 600, border), HTCLIENT);
                assert_eq!(client_hit(30, border - 1, 420, 600, border), HTTOP);
                assert_eq!(client_hit(30, border, 420, 600, border), HTCLIENT);
                assert_eq!(client_hit(30, 600 - border, 420, 600, border), HTBOTTOM);
                assert_eq!(client_hit(30, 599 - border, 420, 600, border), HTCLIENT);
                assert_eq!(
                    client_hit(8 * dpi as i32 / 96 - 1, 30, 420, 600, border),
                    HTCLIENT
                );
            }
        }

        #[test]
        fn hidden_hwnd_edges_work_without_caption_cache_and_follow_current_resize_style() {
            unsafe {
                let window = Fixture::new(true);
                let border = border_pixels(GetDpiForWindow(window.hwnd));
                assert_eq!(window.hit(border - 1, 30), HTLEFT);
                assert_eq!(window.hit(border, 30), HTCLIENT);
                assert_eq!(window.hit(30, 0), HTTOP);
                assert_eq!(window.hit(30, border), HTCLIENT);
                window.probe.caption.set(Some(true));
                assert_eq!(window.hit(30, 20), HTCAPTION);
                assert_eq!(window.hit(0, 20), HTLEFT);
                window.probe.caption.set(None);
                let style = GetWindowLongPtrW(window.hwnd, GWL_STYLE);
                SetWindowLongPtrW(window.hwnd, GWL_STYLE, style & !(WS_THICKFRAME.0 as isize));
                assert_eq!(window.hit(0, 20), HTCLIENT);
                assert_eq!(window.hit(30, 0), HTCLIENT);
                SetWindowLongPtrW(window.hwnd, GWL_STYLE, style);
                assert_eq!(window.hit(30, 0), HTTOP);
                let outside = window.screen_point(-1, 30);
                assert_eq!(
                    window.send(WM_NCHITTEST, 0, outside),
                    DefWindowProcW(window.hwnd, WM_NCHITTEST, WPARAM(0), outside)
                );
                assert_eq!(window.hit(-100, 30), HTNOWHERE);
            }
        }

        #[test]
        fn hidden_hwnd_resize_press_bypasses_swallowed_input_but_client_and_other_windows_do_not() {
            unsafe {
                let window = Fixture::new(true);
                window.send(
                    WM_NCLBUTTONDOWN,
                    HTLEFT as usize,
                    window.screen_point(0, 30),
                );
                assert_eq!(window.probe.size_commands.get(), 1);
                assert_eq!(window.probe.lower_nc_presses.get(), 0);
                window.send(WM_LBUTTONDOWN, 0, LPARAM(0));
                assert_eq!(window.probe.lower_client_presses.get(), 1);
                let style = GetWindowLongPtrW(window.hwnd, GWL_STYLE);
                SetWindowLongPtrW(window.hwnd, GWL_STYLE, style & !(WS_THICKFRAME.0 as isize));
                window.send(WM_NCLBUTTONDOWN, HTTOP as usize, window.screen_point(30, 0));
                assert_eq!(window.probe.size_commands.get(), 1);
                let other = Fixture::new(false);
                assert_eq!(other.hit(30, 20), HTTOP); // Existing procedure is unchanged.
                other.send(WM_NCLBUTTONDOWN, HTLEFT as usize, other.screen_point(0, 30));
                assert_eq!(other.probe.size_commands.get(), 0);
                assert_eq!(other.probe.lower_nc_presses.get(), 1);
            }
        }

        #[test]
        fn destroyed_hidden_hwnd_retires_its_main_window_registration() {
            unsafe {
                let mut window = Fixture::new(true);
                let hwnd = window.hwnd;
                assert!(registered(hwnd));
                DestroyWindow(hwnd).unwrap();
                window.hwnd = HWND::default();
                assert!(!registered(hwnd));
                assert!(handle_message(
                    hwnd,
                    WM_NCHITTEST,
                    WPARAM(0),
                    LPARAM(0),
                    |_, _, _, _, _, _| panic!("retired caption must not be read")
                )
                .is_none());
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use win::{handle_message, register};
