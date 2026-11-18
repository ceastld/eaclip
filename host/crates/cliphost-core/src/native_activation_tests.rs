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

#[test]
fn refused_focus_does_not_become_a_delayed_hide_or_capture_external_keys() {
    for no_focus in [false, true] {
        for behavior in ["Hide", "EdgeHide"] {
            let (_root, host) = fixture();
            {
                let mut inner = host.inner.lock();
                inner.config.enable_no_focus_mode = no_focus;
                inner.config.dismiss_behavior = behavior.into();
                inner.config.enable_edge_hide = false;
            }
            let stamp = host.native_clipboard_activation_stamp();
            assert!(host.native_clipboard_focus_refused(stamp));
            host.inner.lock().expand_grace_until_ms = 0;
            for _ in 0..3 {
                assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::None);
            }
            let snapshot = host.hide_snapshot();
            assert!(snapshot.visible && !snapshot.has_fake_focus && !snapshot.is_active);
            assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Released);
            assert!(!host.clipboard_keyboard_focus_allowed());
            for vk in [0x31, 0x41, 0x1B] {
                assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
                    vk, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
                }, 987654), "refusal must leave QQ's input untouched");
            }
        }
    }
}

#[test]
fn refused_focus_still_allows_real_outside_left_click_to_dismiss() {
    for no_focus in [false, true] {
        let (_root, host) = fixture();
        host.inner.lock().config.enable_no_focus_mode = no_focus;
        host.native_clipboard_focus_refused(host.native_clipboard_activation_stamp());
        host.notify_panel_outside_click_at(8, 8); // External right click is not dismiss.
        assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::None);
        host.notify_outside_click_at(8, 8);
        assert!(!host.hide_snapshot().visible);
        assert!(!host.inner.lock().clipboard_focus_refused);
    }
}

#[test]
fn refused_focus_keeps_the_installed_input_router_and_cached_mouse_path_working() {
    use super::input_companion_cache::{with_test_window, InputWindowIdentity};
    let foreign = InputWindowIdentity { hwnd: 0x112233, pid: 424242, thread: 424243 };
    for no_focus in [false, true] {
        let (_root, host) = fixture();
        host.inner.lock().config.enable_no_focus_mode = no_focus;
        host.native_clipboard_focus_refused(host.native_clipboard_activation_stamp());
        with_test_window(foreign, || {
            host.refresh_input_companions_with([foreign.hwnd, 0], |_| false);
            for hovered in [false, true] {
                host.inner.lock().pointer_in_host = hovered;
                for vk in [0x31, 0x41, 0x1B] {
                    assert!(!host.try_dispatch_input_key_for_foreground(KeyEvent {
                        vk, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
                    }, foreign.hwnd));
                }
            }
            assert!(!host.try_dispatch_input_mouse(0x0204, -10000, -10000, 0));
            assert!(host.inner.lock().clipboard_focus_refused, "external right click is not engagement");
            assert!(!host.try_dispatch_input_mouse(0x0201, -10000, -10000, 0));
            assert!(!host.hide_snapshot().visible, "the installed hook must dismiss in ordinary focus mode too");
        });
    }
}

#[test]
fn refused_focus_ends_on_activation_inside_click_or_new_show() {
    for recovery in ["activate", "inside", "show"] {
        let (_root, host) = fixture();
        let stamp = host.native_clipboard_activation_stamp();
        host.native_clipboard_focus_refused(stamp);
        match recovery {
            "activate" => host.notify_native_clipboard_activation(stamp, true),
            "inside" => {
                host.inner.lock().pointer_in_host = true;
                host.notify_outside_click_at(8, 8);
            }
            "show" => host.request_show(),
            _ => unreachable!(),
        }
        assert!(!host.inner.lock().clipboard_focus_refused);
        if recovery == "activate" {
            host.notify_native_clipboard_activation(stamp, false);
            host.inner.lock().expand_grace_until_ms = 0;
            assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Hide);
        } else {
            assert!(host.hide_snapshot().has_fake_focus);
        }
    }
}

#[test]
fn refusal_does_not_override_fold_pin_explicit_hide_or_newer_sessions() {
    let (_root, host) = fixture();
    let old = host.native_clipboard_activation_stamp();
    host.inner.lock().config.dismiss_behavior = "Fold".into();
    host.native_clipboard_focus_refused(old);
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Fold);
    host.window_hide();
    assert!(!host.native_clipboard_focus_refused(old));
    host.request_show();
    assert!(!host.native_clipboard_focus_refused(old));
    let current = host.native_clipboard_activation_stamp();
    host.native_clipboard_focus_refused(current);
    host.inner.lock().window_pinned = true;
    host.notify_outside_click_at(8, 8);
    assert!(host.hide_snapshot().visible);
    host.window_hide();
    assert!(!host.hide_snapshot().visible);
    host.request_show();
    let stamp = host.native_clipboard_activation_stamp();
    host.set_host_hwnd(12345);
    assert!(!host.native_clipboard_focus_refused(stamp));
    assert!(!host.inner.lock().clipboard_focus_refused);
    host.set_host_hwnd(0);
}

#[test]
fn refused_focus_does_not_disable_real_edge_foreground_dismissal() {
    let (_root, host) = fixture();
    host.inner.lock().config.dismiss_behavior = "EdgeHide".into();
    host.inner.lock().config.enable_edge_hide = false;
    host.on_foreground_hwnd(101);
    host.native_clipboard_focus_refused(host.native_clipboard_activation_stamp());
    host.on_foreground_hwnd(202);
    assert!(!host.hide_snapshot().visible);
}
