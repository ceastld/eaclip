use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors().nth(3).unwrap().join(".tmp/issue-110/activation");
    std::fs::create_dir_all(&directory).unwrap();
    let root = tempfile::tempdir_in(directory).unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().join("data")), 29065).unwrap();
    host.request_show();
    {
        let mut inner = host.inner.lock();
        inner.config.dismiss_behavior = "Hide".into();
        inner.window_pinned = false;
        inner.pointer_in_host = false;
        inner.expand_grace_until_ms = 0;
    }
    (root, host)
}

#[test]
fn stale_native_activation_and_failure_cannot_retire_a_new_show() {
    for hide_first in [true, false] {
        let (_root, host) = fixture();
        let old = host.native_clipboard_activation_stamp();
        if hide_first { host.window_hide(); }
        host.request_show();
        host.inner.lock().expand_grace_until_ms = 0;
        assert_ne!(host.native_clipboard_activation_stamp(), old);
        host.notify_native_clipboard_activation(old, false);
        assert!(!host.hide_native_clipboard_if_current(old));
        let decision = host.poll_hide();
        assert_eq!(decision.window_action, crate::hide_policy::HostDismissAction::None);
        assert!(host.hide_snapshot().visible && host.hide_snapshot().has_fake_focus);
    }
}

#[test]
fn current_native_deactivation_still_clears_focus_and_allows_hide() {
    let (_root, host) = fixture();
    let current = host.native_clipboard_activation_stamp();
    host.notify_native_clipboard_activation(current, true);
    host.notify_native_clipboard_activation(current, false);
    assert!(!host.hide_snapshot().has_fake_focus);
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
    assert!(!host.hide_snapshot().visible);
}

#[test]
fn overlay_and_input_changes_do_not_reset_the_activation_session() {
    let (_root, host) = fixture();
    let current = host.native_clipboard_activation_stamp();
    host.apply_window_action_for("setSpaOverlayOpen", true, "clipboard");
    assert_eq!(host.native_clipboard_activation_stamp(), current);
    host.notify_native_clipboard_activation(current, false);
    assert!(host.hide_snapshot().has_fake_focus);
    host.apply_window_action_for("setSpaOverlayOpen", false, "clipboard");
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
}

#[test]
fn hidden_or_replaced_hwnd_rejects_old_native_observations() {
    let (_root, host) = fixture();
    let old = host.native_clipboard_activation_stamp();
    host.window_hide();
    host.notify_native_clipboard_activation(old, true);
    assert!(!host.hide_snapshot().visible && !host.hide_snapshot().is_active);
    host.request_show();
    let before_replacement = host.native_clipboard_activation_stamp();
    host.set_host_hwnd(12345);
    host.notify_native_clipboard_activation(before_replacement, false);
    assert!(!host.hide_native_clipboard_if_current(before_replacement));
    assert!(host.hide_snapshot().has_fake_focus);
    host.set_host_hwnd(0);
}

#[test]
fn delayed_foreground_refresh_after_show_does_not_dismiss_edge_hide() {
    let (_root, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.config.dismiss_behavior = "EdgeHide".into();
        inner.config.enable_edge_hide = false; // No edge: dismissal falls back to Hide.
    }
    host.on_foreground_hwnd(101); // Previously observed background game.
    host.window_hide();
    host.request_show_with_foreground(false, Some(101), Some(202)); // QQ already foreground.
    assert_eq!(host.inner.lock().last_non_host_fg, 101, "explicit paste target is independent");
    host.on_foreground_hwnd(202); // Late refresh of the same live foreground.
    assert!(host.hide_snapshot().visible && host.hide_snapshot().has_fake_focus);
    host.on_foreground_hwnd(303); // A genuine subsequent external switch still dismisses.
    assert!(!host.hide_snapshot().visible);
}
