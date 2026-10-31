//! No renderer, global hooks or foreground input: dispatch production jobs on
//! a hidden, self-owned HWND and deliver their errors after a new Show.
use super::*;
use windows::{core::w, Win32::UI::WindowsAndMessaging::*};

struct Fixture {
    hwnd: windows::Win32::Foundation::HWND,
    host: HostState,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        cliphost_core::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors().nth(3).unwrap().join(".tmp/issue-110/native-errors");
        std::fs::create_dir_all(&root).unwrap();
        let root = tempfile::tempdir_in(root).unwrap();
        let host = HostState::open(
            cliphost_core::AppPaths::from_data_root(root.path().join("data")), 29065,
        ).unwrap();
        let hwnd = unsafe {
            CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"),
                w!("ClipHost issue 110 error regression"), WS_POPUP,
                -32000, -32000, 32, 32, None, None, None, None)
        }.unwrap();
        host.set_host_hwnd(hwnd.0 as isize);
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        host.request_show();
        Self { hwnd, host, _root: root }
    }

    fn fail_show(&self) {
        let task = std::iter::from_fn(|| self.host.take_chrome_task())
            .find(|task| task.command() == ChromeCommand::Show).unwrap();
        let _guard = guard_task(&self.host, task);
        enqueue_native(self.hwnd, Box::new(|_| Err("injected foreground focus refusal".into())));
        unsafe { dispatch_native(self.hwnd); }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        NATIVE_QUEUE.with(|queues| { queues.borrow_mut().remove(&(self.hwnd.0 as isize)); });
        unsafe { let _ = DestroyWindow(self.hwnd); }
    }
}

#[test]
fn old_show_failure_cannot_hide_a_reopened_or_reengaged_clipboard() {
    for hide_first in [true, false] {
        let fixture = Fixture::new();
        fixture.fail_show();
        if hide_first { fixture.host.window_hide(); }
        fixture.host.request_show();
        assert!(take_error_hwnd(fixture.hwnd).is_none(), "stale error reached the new Show");
        assert!(fixture.host.hide_snapshot().visible);
        assert!(fixture.host.hide_snapshot().has_fake_focus);
    }
}

#[test]
fn current_show_failure_still_retires_visibility_and_input() {
    let fixture = Fixture::new();
    fixture.fail_show();
    assert_eq!(take_error_hwnd(fixture.hwnd).as_deref(), Some("injected foreground focus refusal"));
    let stamp = fixture.host.native_clipboard_activation_stamp();
    assert!(fixture.host.hide_native_clipboard_if_current(stamp));
    let state = fixture.host.hide_snapshot();
    assert!(!state.visible && !state.has_fake_focus && !state.no_focus_session_active);
}

#[test]
fn discarded_error_does_not_mask_a_later_current_error() {
    let fixture = Fixture::new();
    fixture.fail_show();
    fixture.host.request_show();
    fixture.fail_show();
    assert!(take_error_hwnd(fixture.hwnd).is_some());
    assert!(take_error_hwnd(fixture.hwnd).is_none());
}
