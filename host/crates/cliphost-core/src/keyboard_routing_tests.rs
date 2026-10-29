use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    {
        let mut inner = host.inner.lock();
        inner.config.enable_no_focus_mode = true;
        inner.no_focus_session_active = true;
        inner.window_visible = true;
        inner.has_host_fake_focus = true;
    }
    drain_commands(&host);
    (root, host)
}

fn drain_commands(host: &HostState) {
    while host.take_chrome_command().is_some() {}
}

fn key(vk: u32, down: bool, modifiers: u8) -> KeyEvent {
    KeyEvent {
        vk,
        down,
        ctrl: modifiers & 1 != 0,
        alt: modifiers & 2 != 0,
        win: modifiers & 4 != 0,
        shift: modifiers & 8 != 0,
        now_ms: 1,
    }
}

fn fifo_fixture() -> (tempfile::TempDir, HostState, Vec<String>) {
    let (root, host) = fixture();
    let items = [ClipRecord::new_text("fifo first", "fixture"),
        ClipRecord::new_text("fifo second", "fixture")];
    let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
    for item in items { host.store().lock().insert_clip(&item, true).unwrap(); }
    assert_eq!(host.fifo_arm(&json!({ "ids": ids })).unwrap()["armed"], true);
    drain_commands(&host);
    (root, host, ids)
}

#[test]
fn fifo_escape_cancels_on_the_keyboard_worker_without_paste() {
    let (_root, host, _ids) = fifo_fixture();
    host.set_input_runner(|_| panic!("FIFO cancellation must not send input"));
    assert!(host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0));
    assert_eq!(host.fifo_status()["armed"], true, "hook must only queue work");
    assert!(host.needs_keyboard_work_poll());
    assert!(host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0));
    assert_eq!(host.inner.lock().qp.pending_commands.len(), 1, "held Esc queued twice");
    host.poll_keyboard_work();
    assert_eq!(host.fifo_status()["armed"], false);
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, false, 0), 0));
}

#[test]
fn fifo_escape_keeps_shortcuts_and_protected_input_with_their_owner() {
    let (_root, host, _ids) = fifo_fixture();
    for modifiers in 1..=15 {
        assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, true, modifiers), 0));
        assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, false, modifiers), 0));
    }
    for protected in 0..6 {
        {
            let mut inner = host.inner.lock();
            inner.window_runtime_ready = protected != 0;
            inner.ole_drag_active = protected == 1;
            inner.move_interaction_active = protected == 2;
            inner.window_visible = protected >= 3;
            inner.spa_modal_open = protected == 3;
            inner.spa_overlay_open = protected == 4;
            inner.ui.clipboard_editing = protected == 5;
        }
        assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0),
            "FIFO took Escape from protected context {protected}");
        assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, false, 0), 0));
        assert!(host.inner.lock().qp.pending_commands.is_empty());
        assert_eq!(host.fifo_status()["armed"], true);
    }
    {
        let mut inner = host.inner.lock();
        inner.window_visible = false;
        inner.spa_modal_open = false;
        inner.spa_overlay_open = false;
        inner.ui.clipboard_editing = false;
    }
    #[cfg(windows)] {
        let mut windows = NativeWindows(Vec::new());
        for label in ["clipboard", "panel", "main"] {
            let owner = windows.create(0, false);
            match label {
                "clipboard" => host.inner.lock().host_hwnd = owner,
                "panel" => host.inner.lock().panel_hwnd = owner,
                _ => host.inner.lock().main_hwnd = owner,
            }
            let dialog = windows.create(owner, false);
            for foreground in [owner, dialog] {
                assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), foreground));
                assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, false, 0), foreground));
                assert!(host.inner.lock().qp.pending_commands.is_empty());
                assert_eq!(host.fifo_status()["armed"], true);
            }
        }
    }
}

#[test]
fn fifo_escape_from_an_old_queue_cannot_cancel_a_rearmed_queue() {
    let (_root, host, ids) = fifo_fixture();
    assert!(host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0));
    let pending = host.inner.lock().qp.pending_commands.pop_front().unwrap();
    host.fifo_arm(&json!({ "ids": [ids[1].clone()] })).unwrap();
    host.inner.lock().qp.pending_commands.push_back(pending);
    host.poll_keyboard_work();
    assert_eq!(host.fifo_status()["armed"], true);
    assert_eq!(host.inner.lock().fifo.as_ref().unwrap(), &vec![ids[1].clone()]);
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, false, 0), 0));
    assert!(host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0));
    host.poll_keyboard_work();
    assert_eq!(host.fifo_status()["armed"], false);
}

#[test]
fn clipboard_modifier_shortcuts_reach_foreground_without_fallback_interception() {
    let (_root, host) = fixture();
    // The C# low-level handler passes Ctrl/Alt/Win to the foreground app.
    // Include digits, keypad, navigation, Alt+Tab and Ctrl+Escape; Ctrl+F has
    // its own intentional search activation and is covered by existing tests.
    for modifiers in [1, 2, 4, 3, 5, 6, 7, 9, 10, 12] {
        for vk in [0x31, 0x61, 0x09, 0x0D, 0x20, 0x21, 0x22, 0x23, 0x24,
                   0x25, 0x26, 0x27, 0x28, 0x1B] {
            for down in [true, false] {
                assert!(!host.dispatch_low_level_key(key(vk, down, modifiers)),
                    "Intercepted vk={vk:x}, down={down}, modifiers={modifiers}");
                assert!(host.take_chrome_command().is_none(),
                    "Foreground shortcut also dispatched a clipboard command");
                assert!(host.inner.lock().window_visible,
                    "Modified Escape must not dismiss the clipboard window");
            }
        }
    }
}

#[test]
fn clipboard_range_navigation_retains_shift_through_queue_and_cancellation() {
    let (_root, host) = fixture();
    for (vk, name) in [(0x26, "ArrowUp"), (0x28, "ArrowDown"), (0x24, "Home"), (0x23, "End")] {
        for shift in [true, false] {
            assert!(host.dispatch_low_level_key(key(vk, true, if shift { 8 } else { 0 })));
            // Releasing Shift before the UI consumes the task must not change
            // the range operation that was requested at key-down time.
            assert!(!host.dispatch_low_level_key(key(0x10, false, 0)));
            let task = host.take_chrome_task().unwrap();
            assert_eq!(task.command(), ChromeCommand::ClipboardNavKey { key: name, shift });
            assert!(host.chrome_task_input_allowed(task));
            assert!(host.take_chrome_task().is_none());
        }
    }
    host.inner.lock().caps_guide_until_keyup = true;
    assert!(host.dispatch_low_level_key(key(0x4B, true, 8)));
    let task = host.take_chrome_task().unwrap();
    assert_eq!(task.command(), ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: true });
    host.apply_window_action("setSpaModalOpen", true);
    host.apply_window_action("setSpaModalOpen", false);
    assert!(!host.chrome_task_input_allowed(task), "closing a popup revived old range selection");
    let inner = host.inner.lock();
    assert!(inner.no_focus_session_active && inner.has_host_fake_focus);
}

#[test]
fn repeated_enter_does_not_execute_the_clipboard_or_panel_twice() {
    for panel in [false, true] {
        let (_root, host) = fixture();
        if panel {
            let mut inner = host.inner.lock();
            inner.ui.panel_visible = true;
            inner.ui.panel_no_focus_session = true;
            inner.ui.panel_fake_focus = true;
            inner.panel_web_view_ready = true;
        }
        for _ in 0..2 {
            assert!(host.dispatch_low_level_key(key(0x0D, true, 0)));
            assert_eq!(host.take_chrome_command(), Some(if panel { ChromeCommand::PanelNavKey("Enter") }
                else { ChromeCommand::ClipboardNavKey { key: "Enter", shift: false } }));
            for _ in 0..3 {
                assert!(host.dispatch_low_level_key(key(0x0D, true, 0)));
                assert!(host.take_chrome_command().is_none(), "held Enter queued another execution");
            }
            assert!(!host.dispatch_low_level_key(key(0x0D, false, 0)));
        }
    }
}

#[test]
fn held_enter_cannot_execute_after_a_modal_or_readiness_change() {
    let (_root, host) = fixture();
    for modal in [true, false] {
        if modal { host.apply_window_action("setSpaModalOpen", true); }
        else { host.set_window_runtime_ready(false); }
        drain_commands(&host);
        assert!(!host.dispatch_low_level_key(key(0x0D, true, 0)));
        if modal { host.apply_window_action("setSpaModalOpen", false); }
        else { host.set_window_runtime_ready(true); }
        drain_commands(&host);
        assert!(host.dispatch_low_level_key(key(0x0D, true, 0)));
        assert!(host.take_chrome_command().is_none(), "old held Enter executed in the newly available list");
        assert!(!host.dispatch_low_level_key(key(0x0D, false, 0)));
        assert!(host.dispatch_low_level_key(key(0x0D, true, 0)));
        assert!(host.take_chrome_command().is_some());
        assert!(!host.dispatch_low_level_key(key(0x0D, false, 0)));
    }
}

#[test]
fn digit_repeat_keeps_search_and_navigation_repeat_available() {
    let (_root, host) = fixture();
    for vk in [0x31, 0x61] {
        for repeat in [false, true, true] {
            assert!(host.dispatch_low_level_key(key(vk, true, 0)));
            assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat }));
        }
        assert!(!host.dispatch_low_level_key(key(vk, false, 0)));
        assert!(host.dispatch_low_level_key(key(vk, true, 0)));
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
        assert!(!host.dispatch_low_level_key(key(vk, false, 0)));
    }
    for (vk, expected) in [(0x41, ChromeCommand::ClipboardAppendSearch('a')),
        (0x08, ChromeCommand::ClipboardSearchBackspace),
        (0x28, ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false })] {
        for _ in 0..3 {
            assert!(host.dispatch_low_level_key(key(vk, true, 0)));
            assert_eq!(host.take_chrome_command(), Some(expected));
        }
        assert!(!host.dispatch_low_level_key(key(vk, false, 0)));
    }
}

#[test]
fn clipboard_search_preserves_foreground_shortcuts_with_additional_modifiers() {
    let (_root, host) = fixture();
    for modifiers in [3, 5, 7, 9, 11, 13, 15] {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key_for_foreground(key(0x46, down, modifiers), 0),
                "Clipboard stole a modified search chord: modifiers={modifiers}, down={down}");
            assert!(host.take_chrome_command().is_none());
            let inner = host.inner.lock();
            assert!(inner.window_visible && inner.no_focus_session_active && inner.has_host_fake_focus,
                "Passing through a foreground shortcut changed the no-focus session");
        }
    }
    for modifiers in 1..=15 {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, down, modifiers), 0),
                "Clipboard stole modified Escape: modifiers={modifiers}, down={down}");
            assert!(host.take_chrome_command().is_none());
            let inner = host.inner.lock();
            assert!(inner.window_visible && inner.no_focus_session_active && inner.has_host_fake_focus);
        }
    }
    assert!(host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardHandleEscape));
    // The explicitly supported chord still performs its original focus handoff.
    assert!(host.dispatch_low_level_key_for_foreground(key(0x46, true, 1), 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardKeyboardFocus));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardActivateSearch));
    assert!(host.take_chrome_command().is_none());
    assert!(!host.inner.lock().no_focus_session_active);
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x46, false, 1), 0));
}

#[test]
fn super_menu_preserves_modified_escape_and_plain_escape_closes_its_session() {
    let (_root, host) = fixture();
    host.window_hide();
    drain_commands(&host);
    let opened = host.menu_create_session("favor");
    let session = opened["sessionId"].as_str().unwrap();
    drain_commands(&host);
    for modifiers in 1..=15 {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, down, modifiers), 0),
                "Super menu stole modified Escape: modifiers={modifiers}, down={down}");
            assert!(host.take_chrome_command().is_none());
            assert_eq!(host.super_menu_state_detail()["visible"], true);
            assert!(host.menu_nodes(session, "root").is_ok());
        }
    }
    assert!(host.dispatch_low_level_key_for_foreground(key(0x1B, true, 0), 0));
    assert_eq!(host.super_menu_state_detail()["visible"], false);
    assert!(host.menu_sessions()["sessions"].as_array().unwrap().is_empty());
    assert!(host.menu_nodes(session, "root").is_err());
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x1B, false, 0), 0));
}

#[test]
fn clipboard_modal_keeps_keyboard_input_and_navigation_resumes_after_close() {
    let (_root, host) = fixture();
    host.apply_window_action("setSpaModalOpen", true);
    drain_commands(&host);
    for (vk, modifiers) in [(0x31, 0), (0x61, 0), (0x09, 0), (0x09, 8),
                            (0x0D, 0), (0x1B, 0), (0x28, 0), (0x08, 0),
                            (0x41, 0), (0x46, 1)] {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key(key(vk, down, modifiers)),
                "Modal input intercepted: vk={vk:x}, down={down}");
            assert!(host.take_chrome_command().is_none());
            assert!(host.inner.lock().window_visible);
        }
    }
    host.apply_window_action("setSpaModalOpen", false);
    drain_commands(&host);
    assert!(host.dispatch_low_level_key(key(0x28, true, 0)));
    assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false })));
}

#[cfg(windows)]
struct NativeWindows(Vec<windows::Win32::Foundation::HWND>);

#[cfg(windows)]
impl NativeWindows {
    fn create(&mut self, parent: isize, child: bool) -> isize {
        use windows::{core::w, Win32::{Foundation::HWND, UI::WindowsAndMessaging::{
            CreateWindowExW, WINDOW_EX_STYLE, WS_CHILD, WS_POPUP,
        }}};
        // Hidden built-in windows exercise real GW_OWNER semantics without
        // activating windows, sending input or installing global hooks.
        let window = unsafe {
            CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("ClipHost isolated keyboard owner test"),
                if child { WS_CHILD } else { WS_POPUP },
                0, 0, 1, 1, HWND(parent as *mut _), None, None, None).unwrap()
        };
        self.0.push(window);
        window.0 as isize
    }
}

#[cfg(windows)]
impl Drop for NativeWindows {
    fn drop(&mut self) {
        for window in self.0.iter().rev() {
            unsafe { let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(*window); }
        }
    }
}

#[cfg(windows)]
#[test]
fn owned_native_dialog_receives_input_instead_of_clipboard_panel_or_menu() {
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{GetWindow, GW_OWNER}};
    let (_root, host) = fixture();
    let mut windows = NativeWindows(Vec::new());
    let owners = [windows.create(0, false), windows.create(0, false), windows.create(0, false)];
    {
        let mut inner = host.inner.lock();
        inner.host_hwnd = owners[0];
        inner.panel_hwnd = owners[1];
        inner.main_hwnd = owners[2];
        inner.ui.panel_visible = true;
        inner.ui.panel_fake_focus = true;
        inner.ui.panel_no_focus_session = true;
        inner.panel_web_view_ready = true;
        inner.super_menu.visible = true;
    }
    for owner in owners {
        let dialog = windows.create(owner, false);
        assert_eq!(unsafe { GetWindow(HWND(dialog as *mut _), GW_OWNER).unwrap().0 as isize }, owner);
        for (vk, modifiers) in [(0x31, 0), (0x41, 0), (0x09, 0), (0x0D, 0),
                                (0x28, 0), (0x1B, 0), (0x46, 1)] {
            for down in [true, false] {
                assert!(!host.dispatch_low_level_key_for_foreground(key(vk, down, modifiers), dialog),
                    "Owned dialog lost vk={vk:x}, down={down}");
                assert!(host.take_chrome_command().is_none());
                let inner = host.inner.lock();
                assert!(inner.window_visible && inner.ui.panel_visible && inner.super_menu.visible);
                assert!(inner.no_focus_session_active, "Ctrl+F must stay in the dialog");
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn unowned_native_window_and_host_child_preserve_fake_focus_navigation() {
    let (_root, host) = fixture();
    let mut windows = NativeWindows(Vec::new());
    let owner = windows.create(0, false);
    let child = windows.create(owner, true);
    let unowned = windows.create(0, false);
    host.inner.lock().host_hwnd = owner;
    // Do not classify all windows of our process or all descendants as dialogs.
    for foreground in [owner, child, unowned] {
        assert!(host.dispatch_low_level_key_for_foreground(key(0x31, true, 0), foreground));
        assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false })));
        assert!(host.dispatch_low_level_key_for_foreground(key(0x28, true, 0), foreground));
        assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false })));
        assert!(host.take_chrome_command().is_none());
        assert!(!host.dispatch_low_level_key_for_foreground(key(0x31, false, 0), foreground));
        assert!(!host.dispatch_low_level_key_for_foreground(key(0x28, false, 0), foreground));
    }
}
