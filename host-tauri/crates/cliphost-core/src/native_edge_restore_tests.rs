//! Exercise real HWND hit-testing without moving the cursor or reading the
//! user's clipboard. Every fixture stays outside the virtual desktop.
use super::*;
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, POINT, RECT},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
            WindowsAndMessaging::*,
        },
    },
};

struct NativeWindow(HWND);

impl NativeWindow {
    fn popup(owner: Option<HWND>, x: i32, y: i32, width: i32, height: i32) -> Self {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("ClipHost edge restore regression"),
                // SS_NOTIFY gives STATIC a real hit target rather than the
                // default transparent text-control hit behavior.
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(0x100),
                x,
                y,
                width,
                height,
                owner.unwrap_or_default(),
                None,
                GetModuleHandleW(None).unwrap(),
                None,
            )
            .unwrap();
            let window = Self(hwnd);
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect).unwrap();
            assert!(rect.right < GetSystemMetrics(SM_XVIRTUALSCREEN), "fixture must remain off desktop");
            window
        }
    }

    fn child(parent: HWND) -> Self {
        unsafe {
            Self(CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Menu child"),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x100),
                10,
                10,
                40,
                40,
                parent,
                None,
                GetModuleHandleW(None).unwrap(),
                None,
            ).unwrap())
        }
    }

    fn point(&self) -> POINT {
        unsafe {
            let mut rect = RECT::default();
            GetWindowRect(self.0, &mut rect).unwrap();
            POINT { x: rect.left + 5, y: rect.top + 5 }
        }
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe { let _ = DestroyWindow(self.0); }
    }
}

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
    let audit = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp/native-edge-restore");
    std::fs::create_dir_all(&audit).unwrap();
    let root = tempfile::tempdir_in(audit).unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().join("data")), 28965).unwrap();
    (root, host)
}

fn reset(host: &HostState, tucked: bool) {
    let mut inner = host.inner.lock();
    inner.config.dismiss_behavior = "EdgeHide".into();
    inner.config.enable_edge_hide = true;
    inner.config.enable_no_focus_mode = true;
    inner.window_visible = true;
    inner.window_pinned = false;
    inner.pointer_in_host = false;
    inner.has_host_fake_focus = false;
    inner.no_focus_session_active = true;
    inner.is_active = false;
    inner.is_collapsed = false;
    inner.is_layout_busy = false;
    inner.edge_tucked = tucked;
    inner.ignore_pointer_until_leave = false;
    inner.expand_grace_until_ms = 0;
    inner.no_focus_show_grace_until_ms = 0;
    inner.clipboard_web_view_ready = true;
    inner.chrome.clear();
}

fn commands(host: &HostState) -> Vec<ChromeCommand> {
    std::iter::from_fn(|| host.take_chrome_command()).collect()
}

#[test]
fn super_menu_owned_windows_cannot_restore_tucked_clipboard() {
    let (_root, host) = fixture();
    // A one-pixel host models the remaining exposed strip. Its owned menu and
    // preview remain fully visible elsewhere, just like the live GPUI surfaces.
    let owner = NativeWindow::popup(None, -30000, -30000, 1, 160);
    let menu = NativeWindow::popup(Some(owner.0), -29800, -30000, 120, 160);
    let child = NativeWindow::child(menu.0);
    let preview = NativeWindow::popup(Some(menu.0), -29600, -30000, 120, 160);
    host.set_host_hwnd(owner.0.0 as isize);
    host.set_super_menu_hwnd(menu.0.0 as isize);
    let created = host.menu_create_session("favor");
    assert!(created["sessionId"].as_str().is_some());
    assert_eq!(host.super_menu_state_detail()["visible"], true);

    for (name, surface) in [("menu root", &menu), ("menu child", &child), ("menu preview", &preview)] {
        let point = surface.point();
        assert_eq!(unsafe { WindowFromPoint(point) }, surface.0, "{name} fixture is not actually hit");
        assert!(crate::host_hit_test::screen_point_engages_host(point.x, point.y, owner.0.0 as isize),
            "{name} must reproduce the original owner-chain engagement");
        reset(&host, true);
        let inside = host.point_in_host(point.x, point.y);
        assert!(!inside, "{name} must not engage the tucked clipboard");
        host.set_pointer_in_host(inside);
        assert!(host.inner.lock().edge_tucked, "polling over {name} restored the clipboard");
        assert!(commands(&host).is_empty(), "polling over {name} queued window work");

        reset(&host, true);
        {
            let mut inner = host.inner.lock();
            let inside = host.input_point_in_host(&mut inner, point.x, point.y);
            assert!(!inside, "input hit-testing over {name} engaged the clipboard");
            host.input_set_pointer_in_host(&mut inner, inside);
            assert!(inner.edge_tucked);
        }
        assert!(commands(&host).is_empty());

        reset(&host, true);
        assert!(!host.try_dispatch_input_mouse(WM_MOUSEMOVE, point.x, point.y, 0));
        let inner = host.inner.lock();
        assert!(inner.edge_tucked && !inner.pointer_in_host,
            "the real mouse router restored the clipboard over {name}");
        drop(inner);
        assert!(commands(&host).is_empty(), "mouse movement over {name} queued window work");
        assert_eq!(host.super_menu_state_detail()["visible"], true);
    }

    // Bounds alone are insufficient when an independent menu covers the strip.
    let strip = POINT { x: -30000, y: -29920 };
    unsafe {
        SetWindowPos(menu.0, HWND_TOP, -30000, -30000, 0, 0,
            SWP_NOACTIVATE | SWP_NOSIZE).unwrap();
    }
    assert_eq!(unsafe { WindowFromPoint(strip) }, menu.0);
    reset(&host, true);
    assert!(!host.point_in_host(strip.x, strip.y));
    assert!(!host.try_dispatch_input_mouse(WM_MOUSEMOVE, strip.x, strip.y, 0));
    assert!(host.inner.lock().edge_tucked);
    assert!(commands(&host).is_empty());
    unsafe {
        SetWindowPos(menu.0, HWND_TOP, -29800, -30000, 0, 0,
            SWP_NOACTIVATE | SWP_NOSIZE).unwrap();
    }

    // The real exposed pixel must still restore the host through both paths.
    assert_eq!(unsafe { WindowFromPoint(strip) }, owner.0);
    reset(&host, true);
    let inside = host.point_in_host(strip.x, strip.y);
    assert!(inside, "the real host strip must remain an expansion target");
    host.set_pointer_in_host(inside);
    assert!(!host.inner.lock().edge_tucked);
    assert_eq!(commands(&host), vec![ChromeCommand::RestoreEdge]);

    reset(&host, true);
    assert!(!host.try_dispatch_input_mouse(WM_MOUSEMOVE, strip.x, strip.y, 0));
    assert!(!host.inner.lock().edge_tucked);
    assert_eq!(commands(&host), vec![ChromeCommand::RestoreEdge]);
    assert!(!host.hide_snapshot().has_fake_focus, "strip hover must not create fake focus");
}

#[test]
fn ordinary_owned_popup_keeps_expanded_clipboard_pointer_engagement() {
    let (_root, host) = fixture();
    let owner = NativeWindow::popup(None, -30000, -30400, 120, 160);
    let popup = NativeWindow::popup(Some(owner.0), -29800, -30400, 120, 160);
    host.set_host_hwnd(owner.0.0 as isize);
    let point = popup.point();
    assert_eq!(unsafe { WindowFromPoint(point) }, popup.0);
    reset(&host, false);
    assert!(host.point_in_host(point.x, point.y), "expanded clipboard must retain its owned popup");
    {
        let mut inner = host.inner.lock();
        assert!(host.input_point_in_host(&mut inner, point.x, point.y));
    }
    assert!(!host.try_dispatch_input_mouse(WM_MOUSEMOVE, point.x, point.y, 0));
    assert!(host.hide_snapshot().pointer_in_host);
    assert!(!host.inner.lock().edge_tucked);
    assert!(commands(&host).is_empty());
}
