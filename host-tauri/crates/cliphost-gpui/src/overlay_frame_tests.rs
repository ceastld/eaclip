//! Hidden, owned HWND regression tests for the frameless overlay boundary.
//! The lower procedure stands in for GPUI while its App is already borrowed.
//! No window is shown and no input is injected into the user's desktop.

use std::{
    cell::{Cell, RefCell},
    sync::Once,
};

use windows::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::ClientToScreen,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::GetWindowSubclass, WindowsAndMessaging::*},
    },
};

const FORWARDED_MESSAGE: u32 = WM_APP + 0x77;
const FORWARDED_RESULT: isize = 0x12345;

#[derive(Default)]
struct Probe {
    app: RefCell<()>,
    lower_hit_tests: Cell<usize>,
    borrowed_app_callbacks: Cell<usize>,
    lower_client_calculations: Cell<usize>,
    forwarded: Cell<Option<(usize, isize)>>,
    destroyed: Cell<usize>,
    subclass_removed_on_destroy: Cell<bool>,
}

unsafe extern "system" fn base_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(probe) = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe).as_ref() {
        match message {
            WM_NCHITTEST => {
                probe.lower_hit_tests.set(probe.lower_hit_tests.get() + 1);
                // Record the forbidden callback instead of panicking across
                // an extern-system boundary if the regression is present.
                if probe.app.try_borrow_mut().is_err() {
                    probe
                        .borrowed_app_callbacks
                        .set(probe.borrowed_app_callbacks.get() + 1);
                }
                return LRESULT(HTCAPTION as isize);
            }
            WM_NCCALCSIZE if wp.0 != 0 => {
                probe
                    .lower_client_calculations
                    .set(probe.lower_client_calculations.get() + 1);
                let parameters = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                parameters.rgrc[0].top += 24;
                return LRESULT(WVR_REDRAW as isize);
            }
            FORWARDED_MESSAGE => {
                probe.forwarded.set(Some((wp.0, lp.0)));
                return LRESULT(FORWARDED_RESULT);
            }
            WM_NCDESTROY => {
                probe.destroyed.set(probe.destroyed.get() + 1);
                let mut data = 0;
                probe.subclass_removed_on_destroy.set(
                    !GetWindowSubclass(
                        hwnd,
                        Some(super::overlay_frame_proc),
                        super::OVERLAY_FRAME_SUBCLASS,
                        Some(&mut data),
                    )
                    .as_bool(),
                );
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wp, lp)
}

struct Fixture {
    hwnd: HWND,
    probe: Box<Probe>,
}

impl Fixture {
    unsafe fn new() -> Self {
        static CLASS: Once = Once::new();
        CLASS.call_once(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(base_proc),
                hInstance: HINSTANCE(GetModuleHandleW(None).unwrap().0),
                lpszClassName: w!("ClipHostFramelessOverlayBoundaryTest"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("ClipHostFramelessOverlayBoundaryTest"),
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
        let fixture = Self { hwnd, probe };
        super::raw_frameless_overlay(hwnd).unwrap();
        assert!(!IsWindowVisible(hwnd).as_bool());
        fixture
    }

    unsafe fn send(&self, message: u32, wp: usize, lp: LPARAM) -> LRESULT {
        SendMessageW(self.hwnd, message, Some(WPARAM(wp)), Some(lp))
    }

    unsafe fn screen_point(&self, x: i32, y: i32) -> LPARAM {
        let mut point = POINT { x, y };
        assert!(ClientToScreen(self.hwnd, &mut point).as_bool());
        LPARAM((((point.y as u16 as u32) << 16) | point.x as u16 as u32) as isize)
    }

    unsafe fn destroy(&mut self) {
        DestroyWindow(self.hwnd).unwrap();
        self.hwnd = HWND::default();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.hwnd.0.is_null() {
            unsafe { self.destroy() };
        }
    }
}

#[test]
fn hidden_overlay_hit_tests_never_reenter_the_borrowed_app() {
    unsafe {
        let window = Fixture::new();
        let mut rect = RECT::default();
        GetClientRect(window.hwnd, &mut rect).unwrap();
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        assert!(width > 0 && height > 0);
        let _app_borrow = window.probe.app.borrow_mut();
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
            (width / 2, 0),
            (width / 2, height - 1),
            (0, height / 2),
            (width - 1, height / 2),
            (width / 2, height / 2),
            (-1, height / 2),
            (width, height / 2),
            (width / 2, -1),
            (width / 2, height),
            (-100, -100),
        ] {
            assert_eq!(
                window.send(WM_NCHITTEST, 0, window.screen_point(x, y)),
                LRESULT(HTCLIENT as isize),
                "overlay client point ({x}, {y}) must not reach GPUI"
            );
        }
        assert_eq!(window.probe.lower_hit_tests.get(), 0);
        assert_eq!(window.probe.borrowed_app_callbacks.get(), 0);
        assert!(!IsWindowVisible(window.hwnd).as_bool());
    }
}

#[test]
fn hidden_overlay_client_calculation_preserves_the_complete_proposed_rect() {
    unsafe {
        let window = Fixture::new();
        let proposed = RECT {
            left: -1200,
            top: -800,
            right: -720,
            bottom: -440,
        };
        let mut parameters = NCCALCSIZE_PARAMS {
            rgrc: [proposed; 3],
            ..Default::default()
        };
        assert_eq!(
            window.send(
                WM_NCCALCSIZE,
                1,
                LPARAM(&mut parameters as *mut NCCALCSIZE_PARAMS as isize),
            ),
            LRESULT(0)
        );
        for actual in parameters.rgrc {
            assert_eq!(
                (actual.left, actual.top, actual.right, actual.bottom),
                (proposed.left, proposed.top, proposed.right, proposed.bottom)
            );
        }
        assert_eq!(window.probe.lower_client_calculations.get(), 0);
    }
}

#[test]
fn hidden_overlay_forwards_other_messages_and_removes_its_subclass_on_destroy() {
    unsafe {
        let mut window = Fixture::new();
        assert_eq!(
            window.send(FORWARDED_MESSAGE, 0x1234, LPARAM(0x5678)),
            LRESULT(FORWARDED_RESULT)
        );
        assert_eq!(window.probe.forwarded.get(), Some((0x1234, 0x5678)));
        let hwnd = window.hwnd;
        window.destroy();
        assert!(!IsWindow(Some(hwnd)).as_bool());
        assert_eq!(window.probe.destroyed.get(), 1);
        assert!(window.probe.subclass_removed_on_destroy.get());
    }
}
