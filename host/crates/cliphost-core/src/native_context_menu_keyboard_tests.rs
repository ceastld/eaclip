//! Calls the real cached hook path in an isolated data root; no OS input,
//! windows, hooks, network listener or clipboard access.
use super::*;

const LEASE: u64 = 71;
const OWNER: isize = 100;
const POPUP: isize = 101;
const FOREGROUND: isize = 200;

fn fixture(opt_in: bool) -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("context-menu-keyboard-")
        .tempdir_in(scratch)
        .unwrap();
    let host = HostState::open(AppPaths::from_data_root(temp.path().to_path_buf()), 29065).unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    host.set_host_hwnd(OWNER);
    {
        let mut inner = host.inner.lock();
        inner.window_visible = true;
        inner.config.enable_no_focus_mode = true;
        inner.no_focus_session_active = true;
        inner.has_host_fake_focus = true;
        inner.keyboard_target_hwnd = FOREGROUND;
        inner.keyboard_target_rebind_until_ms = 0;
        inner.last_fg = FOREGROUND;
        inner.last_non_host_fg = FOREGROUND;
        inner.ui.keyboard_focus_host = None;
    }
    assert!(host.acquire_native_context_menu(LEASE, "clipboard"));
    assert!(host.set_native_context_menu_hwnd(LEASE, POPUP));
    if opt_in {
        assert!(host.register_native_context_menu_keyboard(LEASE, POPUP, scope(1, 1)));
    }
    while host.take_chrome_command().is_some() {}
    (temp, host)
}
fn scope(presentation: u64, focus: u64) -> NativeContextMenuKeyboardScope {
    NativeContextMenuKeyboardScope {
        presentation,
        focus,
    }
}
fn event(vk: u32, down: bool) -> KeyEvent {
    KeyEvent {
        vk,
        down,
        ctrl: false,
        alt: false,
        shift: false,
        win: false,
        now_ms: 1,
    }
}
fn route(host: &HostState, vk: u32, down: bool) -> bool {
    host.dispatch_low_level_key_for_foreground(event(vk, down), FOREGROUND)
}
fn take(host: &HostState) -> Vec<NativeContextMenuKey> {
    let events = host.drain_native_context_menu_keys(LEASE);
    assert!(events
        .iter()
        .all(|event| host.native_context_menu_key_is_current(event)));
    events
}

#[test]
fn context_menu_keyboard_admits_only_registered_lease_hwnd_and_keeps_fast_keys() {
    let (_temp, host) = fixture(false);
    assert!(!host.register_native_context_menu_keyboard(LEASE + 1, POPUP, scope(1, 1)));
    assert!(!host.register_native_context_menu_keyboard(LEASE, POPUP + 1, scope(1, 1)));
    assert!(host.register_native_context_menu_keyboard(LEASE, POPUP, scope(1, 1)));
    assert!(route(&host, 0x28, true));
    assert!(route(&host, 0x28, true));
    assert!(route(&host, 0x28, false));
    assert!(route(&host, 0x0D, true));
    assert!(route(&host, 0x0D, true));
    assert!(route(&host, 0x0D, false));
    // Repainting the old logical focus must not discard accepted navigation.
    assert!(host.register_native_context_menu_keyboard(LEASE, POPUP, scope(1, 1)));
    let events = take(&host);
    assert_eq!(
        events.iter().map(|event| event.key()).collect::<Vec<_>>(),
        ["down", "down", "enter"]
    );
    assert!(events.iter().all(|event| event.lease() == LEASE));
    assert!(
        !route(&host, 0x41, true),
        "ordinary typing belongs to the external app"
    );
    assert!(!route(&host, 0x41, false));
}

#[test]
fn context_menu_keyboard_actions_ignore_repeat_and_pair_modified_release() {
    let (_temp, host) = fixture(true);
    for (vk, ctrl, key) in [
        (0x20, false, "space"),
        (0x2E, false, "delete"),
        (0x43, true, "ctrl-c"),
        (0x50, true, "ctrl-p"),
    ] {
        let mut down = event(vk, true);
        down.ctrl = ctrl;
        assert!(host.dispatch_low_level_key_for_foreground(down, FOREGROUND));
        assert!(host.dispatch_low_level_key_for_foreground(down, FOREGROUND));
        assert!(
            route(&host, vk, false),
            "release remains owned after Ctrl is released"
        );
        assert_eq!(
            take(&host)
                .iter()
                .map(|event| event.key())
                .collect::<Vec<_>>(),
            [key]
        );
    }
    for modifier in 0..4 {
        let mut down = event(0x1B, true);
        match modifier {
            0 => down.ctrl = true,
            1 => down.shift = true,
            2 => down.alt = true,
            _ => down.win = true,
        }
        assert!(!host.dispatch_low_level_key_for_foreground(down, FOREGROUND));
        assert!(!route(&host, 0x1B, false));
        assert!(host.native_context_menu_is_current(LEASE));
    }
}

#[test]
fn context_menu_keyboard_escape_reaches_child_first_and_drains_after_close() {
    let (_temp, host) = fixture(true);
    assert!(route(&host, 0x1B, true));
    assert!(
        host.native_context_menu_is_current(LEASE),
        "opt-in Escape does not generically retire the parent"
    );
    assert_eq!(take(&host)[0].key(), "escape");
    assert!(route(&host, 0x1B, true));
    assert!(take(&host).is_empty());
    assert!(route(&host, 0x1B, false));
    assert!(route(&host, 0x1B, true));
    assert_eq!(take(&host)[0].key(), "escape");
    assert!(host.begin_native_context_menu_escape_drain(LEASE));
    assert!(host.release_native_context_menu(LEASE));
    assert!(route(&host, 0x1B, true));
    assert!(route(&host, 0x1B, false));
    assert!(!host.inner.lock().native_context_menu_escape_down);
    let mut inner = host.inner.lock();
    assert!(
        !host.input_route_native_context_menu_key(
            &mut inner,
            event(0x1B, true),
            FOREGROUND,
            false,
            false
        ),
        "the next fresh Escape belongs to the normal owner route, not the retired menu"
    );
}

#[test]
fn context_menu_keyboard_non_opt_in_menu_keeps_generic_escape() {
    let (_temp, host) = fixture(false);
    assert!(!route(&host, 0x28, true));
    assert!(route(&host, 0x1B, true));
    assert!(!host.native_context_menu_is_current(LEASE));
    assert!(host.drain_native_context_menu_keys(LEASE).is_empty());
    assert!(matches!(
        host.take_chrome_command(),
        Some(ChromeCommand::DismissContextMenu(LEASE))
    ));
    assert!(route(&host, 0x1B, false));
}

#[test]
fn context_menu_keyboard_focus_scope_retires_queued_and_preheld_keys() {
    let (_temp, host) = fixture(true);
    assert!(route(&host, 0x28, true));
    let old = take(&host).remove(0);
    assert!(route(&host, 0x0D, true));
    assert!(host.register_native_context_menu_keyboard(LEASE, POPUP, scope(1, 2)));
    assert!(!host.register_native_context_menu_keyboard(LEASE, POPUP, scope(1, 1)));
    assert!(!host.native_context_menu_key_is_current(&old));
    assert!(take(&host).is_empty());
    assert!(route(&host, 0x0D, true));
    assert!(route(&host, 0x0D, false));
    assert!(take(&host).is_empty());
    assert!(route(&host, 0x0D, true));
    assert_eq!(take(&host)[0].scope(), scope(1, 2));
}

#[test]
fn context_menu_keyboard_replace_and_old_clear_cannot_lend_or_retire_new_scope() {
    let (_temp, host) = fixture(true);
    assert!(route(&host, 0x20, true));
    let old = take(&host).remove(0);
    assert!(host.acquire_native_context_menu(LEASE + 1, "clipboard"));
    assert!(host.set_native_context_menu_hwnd(LEASE + 1, POPUP));
    assert!(host.register_native_context_menu_keyboard(LEASE + 1, POPUP, scope(1, 1)));
    host.clear_native_context_menu_keyboard(LEASE);
    assert!(!host.native_context_menu_key_is_current(&old));
    assert!(route(&host, 0x20, true));
    assert!(route(&host, 0x20, false));
    assert!(host.drain_native_context_menu_keys(LEASE + 1).is_empty());
    assert!(route(&host, 0x0D, true));
    assert_eq!(
        host.drain_native_context_menu_keys(LEASE + 1)[0].lease(),
        LEASE + 1
    );
}

#[test]
fn context_menu_keyboard_foreground_owner_hide_and_hwnd_changes_retire() {
    for reason in ["foreground", "hide", "owner", "popup", "runtime"] {
        let (_temp, host) = fixture(true);
        assert!(route(&host, 0x28, true));
        let old = take(&host).remove(0);
        match reason {
            "foreground" => {
                host.notify_native_super_menu_foreground(300);
                host.notify_native_super_menu_foreground(FOREGROUND);
            }
            "hide" => {
                host.window_hide();
            }
            "owner" => {
                host.set_host_hwnd(400);
                host.set_host_hwnd(OWNER);
            }
            "popup" => {
                host.set_native_context_menu_hwnd(LEASE, 401);
                host.set_native_context_menu_hwnd(LEASE, POPUP);
            }
            _ => {
                host.set_window_runtime_ready(false);
                host.set_window_runtime_ready(true);
            }
        }
        assert!(!host.native_context_menu_key_is_current(&old), "{reason}");
        assert!(
            host.drain_native_context_menu_keys(LEASE).is_empty(),
            "{reason}"
        );
        assert!(
            !host.register_native_context_menu_keyboard(LEASE, POPUP, scope(2, 1)),
            "{reason} cannot revive a retired lease"
        );
        assert!(
            route(&host, 0x28, false),
            "accepted key-up still drains after {reason}"
        );
    }
}

#[test]
fn context_menu_keyboard_queue_is_bounded_and_overflow_retires() {
    let (_temp, host) = fixture(true);
    for _ in 0..MAX_KEYS + 2 {
        assert!(route(&host, 0x28, true));
    }
    assert!(host.inner.lock().native_context_menu_keyboard.inbox.len() <= MAX_KEYS);
    assert!(take(&host).is_empty());
    assert!(!host.register_native_context_menu_keyboard(LEASE, POPUP, scope(2, 1)));
    assert!(route(&host, 0x28, false));
}

#[test]
fn context_menu_keyboard_input_gap_discards_the_queue_and_cannot_rebind() {
    let (_temp, host) = fixture(true);
    assert!(route(&host, 0x0D, true));
    host.input_was_busy.store(true, Ordering::Release);
    assert!(host.drain_native_context_menu_keys(LEASE).is_empty());
    assert!(!host.register_native_context_menu_keyboard(LEASE, POPUP, scope(2, 1)));
    assert!(
        !route(&host, 0x0D, false),
        "a missed release must not become a delayed action"
    );
}
