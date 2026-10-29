use super::*;
use windows::{core::w, Win32::{
    Foundation::{BOOL, HANDLE, HWND},
    Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
    UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
}};

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) { unsafe { let _ = RemovePropW(self.0, w!("ClipHost.NativeHoverPreview")); let _ = DestroyWindow(self.0); } }
}
fn window(owner: Option<HWND>, left: i32) -> Window {
    unsafe {
        let hwnd = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"),
            w!("Native hover dismiss fixture"), WS_POPUP, left, 100, 200, 160,
            owner.unwrap_or_default(), None, GetModuleHandleW(None).unwrap(), None).unwrap();
        let cloak = BOOL(1);
        DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4).unwrap();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Window(hwnd)
    }
}
fn fixture(mode: &str) -> (tempfile::TempDir, HostState, Window, Window) {
    crate::clipboard::force_headless(true);
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
    let audit = std::env::var_os("CLIPHOST_HOVER_DISMISS_AUDIT_DIR").map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp/cliphost-hover-dismiss-gap-20261001/native"));
    std::fs::create_dir_all(&audit).unwrap();
    let root = tempfile::tempdir_in(audit).unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().join("data")), 28965).unwrap();
    let owner = window(None, 200);
    let popup = window(Some(owner.0), 420);
    if mode == "EdgeHide" {
        unsafe {
            let mut monitor = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            assert!(GetMonitorInfoW(MonitorFromWindow(owner.0, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool());
            SetWindowPos(owner.0, None, monitor.rcWork.left, monitor.rcWork.top + 100, 0, 0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER).unwrap();
            SetWindowPos(popup.0, None, monitor.rcWork.left + 220, monitor.rcWork.top + 100, 0, 0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER).unwrap();
        }
    }
    // Wire the HWND property protocol directly: baseline need not implement a
    // new API, and assertions still execute the original real poll_hide method.
    unsafe { SetPropW(owner.0, w!("ClipHost.NativeHoverPreview"), HANDLE(popup.0.0)).unwrap(); }
    host.set_host_hwnd(owner.0.0 as isize);
    reset(&host, mode);
    (root, host, owner, popup)
}
fn reset(host: &HostState, mode: &str) {
    let mut inner = host.inner.lock();
    inner.config.dismiss_behavior = mode.into();
    inner.config.enable_edge_hide = true;
    inner.config.enable_no_focus_mode = true;
    inner.window_visible = true;
    inner.window_pinned = false;
    inner.pointer_in_host = false; // The actual gap between the two windows.
    inner.has_host_fake_focus = false;
    inner.no_focus_session_active = true;
    inner.is_active = false;
    inner.foreground_is_host_related = false;
    inner.last_fg = 0;
    inner.is_collapsed = false;
    inner.is_layout_busy = false;
    inner.edge_tucked = false;
    inner.ignore_pointer_until_leave = false;
    inner.expand_grace_until_ms = 0;
    inner.no_focus_show_grace_until_ms = 0;
    inner.clipboard_web_view_ready = true;
    inner.chrome.clear();
}
fn gap_stays_then_resumes(mode: &str, expected: crate::hide_policy::HostDismissAction) {
    let (_root, host, owner, popup) = fixture(mode);
    let mut bounds = windows::Win32::Foundation::RECT::default();
    unsafe { GetWindowRect(owner.0, &mut bounds).unwrap(); }
    assert!(!host.point_in_host(bounds.right + 10, bounds.top + 40));
    let snap = host.hide_snapshot();
    if mode == "EdgeHide" { assert!(snap.can_apply_edge_tuck, "fixture must be at a real tuckable edge"); }
    assert!(!snap.pointer_in_host && !snap.has_fake_focus && !snap.spa_modal_open);
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::None,
        "host auto-dismissed before the visible preview could finish its leave grace");
    assert!(host.take_chrome_command().is_none());
    let snap = host.hide_snapshot();
    assert!(!snap.pointer_in_host && !snap.has_fake_focus && !snap.spa_modal_open,
        "preview protection must not invent pointer/fake-focus/SPA state");
    unsafe {
        assert_ne!(GetForegroundWindow(), owner.0);
        assert_ne!(GetForegroundWindow(), popup.0);
        let _ = ShowWindow(popup.0, SW_HIDE);
    }
    assert_eq!(host.poll_hide().window_action, expected, "dismiss must resume when the popup hides");
}
#[test]
fn native_hover_gap_blocks_hide_only_while_its_popup_is_visible() {
    gap_stays_then_resumes("Hide", crate::hide_policy::HostDismissAction::Hide);
}
#[test]
fn native_hover_gap_blocks_fold_only_while_its_popup_is_visible() {
    gap_stays_then_resumes("Fold", crate::hide_policy::HostDismissAction::Fold);
}
#[test]
fn native_hover_gap_blocks_edge_only_while_its_popup_is_visible() {
    gap_stays_then_resumes("EdgeHide", crate::hide_policy::HostDismissAction::EdgeHide);
}
#[test]
fn native_hover_gap_rejects_missing_hidden_other_owner_and_destroyed_popups() {
    let (_root, host, owner, popup) = fixture("Hide");
    unsafe { RemovePropW(owner.0, w!("ClipHost.NativeHoverPreview")).unwrap(); }
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
    reset(&host, "Hide");
    unsafe {
        SetPropW(owner.0, w!("ClipHost.NativeHoverPreview"), HANDLE(popup.0.0)).unwrap();
        let _ = ShowWindow(popup.0, SW_HIDE);
    }
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
    reset(&host, "Hide");
    let other = window(None, 800);
    let other_popup = window(Some(other.0), 1020);
    unsafe { SetPropW(owner.0, w!("ClipHost.NativeHoverPreview"), HANDLE(other_popup.0.0)).unwrap(); }
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
    reset(&host, "Hide");
    let destroyed = other_popup.0;
    drop(other_popup);
    unsafe { SetPropW(owner.0, w!("ClipHost.NativeHoverPreview"), HANDLE(destroyed.0)).unwrap(); }
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
}
#[test]
fn native_hover_gap_does_not_swallow_keys_or_block_outside_click_and_user_hide() {
    let (_root, host, _owner, _popup) = fixture("Hide");
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::None);
    assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x31, down: true, ctrl: false, alt: false, win: false, shift: false, now_ms: 1,
    }, 0), "visible preview is not keyboard engagement");
    assert!(host.take_chrome_command().is_none());
    host.apply_clipboard_outside_click(410, 140);
    assert!(!host.hide_snapshot().visible, "explicit outside click must still dismiss");
    reset(&host, "Hide");
    host.window_hide();
    assert!(!host.hide_snapshot().visible, "user hide must still dismiss");
}
