use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    host.favor_folder_create(&json!({"title": "Input fixture"})).unwrap();
    host.panel_show(&json!({})).unwrap();
    host.show_web_route("/settings/clip");
    host.inner.lock().panel_web_view_ready = true;
    drain(&host);
    (root, host)
}

fn drain(host: &HostState) {
    while host.take_chrome_command().is_some() {}
}

fn prepare_digit_status(host: &HostState) {
    let mut inner = host.inner.lock();
    inner.window_visible = true;
    inner.window_pinned = true;
    inner.has_host_fake_focus = true;
    inner.no_focus_session_active = true;
    inner.clipboard_web_view_ready = true;
    inner.ui.panel_visible = false;
    inner.ui.panel_fake_focus = false;
    inner.ui.clipboard_editing = false;
    inner.spa_modal_open = false;
    inner.spa_overlay_open = false;
    inner.config.using_number_key_to_paste = true;
    inner.qp.overlay_visible = false;
}

#[test]
fn digit_routing_status_is_read_only_and_matches_the_live_digit_route() {
    let (_root, host) = fixture();
    prepare_digit_status(&host);
    drain(&host);
    for _ in 0..3 { assert!(host.clipboard_digit_routing_active()); }
    assert!(host.take_chrome_command().is_none(), "reading the indicator must never queue input");
    assert!(host.hide_snapshot().has_fake_focus);
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 987654));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
}

#[test]
fn digit_routing_status_preserves_fold_and_releases_for_search_or_editing() {
    let (_root, host) = fixture();
    prepare_digit_status(&host);
    {
        let mut inner = host.inner.lock();
        inner.config.dismiss_behavior = "Fold".into();
        inner.is_collapsed = true;
    }
    assert!(host.clipboard_digit_routing_active(), "Fold keeps its existing no-focus ownership");
    host.inner.lock().ui.clipboard_editing = true;
    assert!(!host.clipboard_digit_routing_active());
    host.inner.lock().ui.clipboard_editing = false;
    focus(&host, "clipboard");
    assert!(!host.clipboard_digit_routing_active(), "native search focus owns number input");
}

#[test]
fn digit_routing_status_is_paused_by_hidden_loading_or_other_input_owners() {
    let (_root, host) = fixture();
    for gate in ["hidden", "not-ready", "overlay", "panel", "menu", "drag", "number-disabled", "quick-paste"] {
        prepare_digit_status(&host);
        host.set_window_runtime_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.super_menu.visible = false;
            inner.ole_drag_active = false;
            match gate {
                "hidden" => inner.window_visible = false,
                "not-ready" => inner.window_runtime_ready = false,
                "overlay" => inner.spa_overlay_open = true,
                "panel" => { inner.ui.panel_visible = true; inner.ui.panel_fake_focus = true; }
                "menu" => inner.super_menu.visible = true,
                "drag" => inner.ole_drag_active = true,
                "number-disabled" => inner.config.using_number_key_to_paste = false,
                "quick-paste" => inner.qp.overlay_visible = true,
                _ => unreachable!(),
            }
        }
        assert!(!host.clipboard_digit_routing_active(), "indicator must pause for {gate}");
    }
}

fn focus(host: &HostState, source: &str) -> Value {
    host.chrome_web_action(&json!({
        "kind": "clipboardHostChrome", "sourceHostWindow": source,
        "route": "/settings/clip", "isClipboardEditing": false,
        "requestHostKeyboardFocus": true,
    }))
}

fn assert_keys_reach_input(host: &HostState) {
    for (vk, ctrl) in [(0x31, false), (0x41, false), (0x28, false),
                       (0x0D, false), (0x1B, false), (0x46, true)] {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk, down, ctrl, alt: false, shift: false, win: false, now_ms: 1,
            }, 0), "input lost vk={vk:x}, down={down}");
            assert!(host.take_chrome_command().is_none());
        }
    }
}

#[test]
fn panel_input_focus_activates_panel_without_mutating_clipboard_route() {
    let (_root, host) = fixture();
    host.inner.lock().ui.clipboard_editing = true;
    assert_eq!(focus(&host, "panel")["ok"], true);
    {
        let inner = host.inner.lock();
        assert!(!inner.ui.panel_no_focus_session, "panel must leave no-activate mode");
        assert_eq!(inner.ui.clipboard_route, "/clipboard");
        assert!(inner.ui.clipboard_editing);
        assert!(!inner.no_focus_session_active);
    }
    assert_eq!(format!("{:?}", host.take_chrome_command()), "Some(HostKeyboardFocus(\"panel\"))");
    assert_keys_reach_input(&host);
    focus(&host, "panel");
    assert!(host.take_chrome_command().is_none(), "focusin within a focused panel must not reactivate it");
}

#[test]
fn main_input_focus_releases_both_fake_focus_windows() {
    let (_root, host) = fixture();
    assert_eq!(focus(&host, "main")["ok"], true);
    assert_eq!(host.inner.lock().ui.clipboard_route, "/clipboard");
    assert_eq!(format!("{:?}", host.take_chrome_command()), "Some(HostKeyboardFocus(\"main\"))");
    assert_keys_reach_input(&host);
}

#[test]
fn clipboard_input_focus_releases_panel_navigation() {
    let (_root, host) = fixture();
    focus(&host, "clipboard");
    assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::ClipboardKeyboardFocus)));
    assert_keys_reach_input(&host);
}

#[test]
fn hidden_and_invalid_focus_sources_cannot_activate_clipboard() {
    let (_root, host) = fixture();
    host.mark_panel_hidden();
    host.mark_main_hidden();
    for source in ["panel", "main", "super-menu", "bogus"] {
        focus(&host, source);
        assert!(host.take_chrome_command().is_none(), "hidden/invalid source {source}");
        assert!(host.inner.lock().no_focus_session_active);
        assert_eq!(host.inner.lock().ui.clipboard_route, "/clipboard");
    }
}

#[test]
fn latest_input_window_supersedes_queued_focus_and_search() {
    let (_root, host) = fixture();
    focus(&host, "clipboard");
    host.enqueue_chrome(ChromeCommand::ClipboardActivateSearch);
    host.enqueue_chrome(ChromeCommand::ClipboardAppendSearch('x'));
    host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 1, repeat: false });
    host.enqueue_chrome(ChromeCommand::ClipboardNavKey { key: "Enter", shift: false });
    host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
    focus(&host, "panel");
    focus(&host, "main");
    assert_eq!(format!("{:?}", host.take_chrome_command()), "Some(HostKeyboardFocus(\"main\"))");
    assert!(host.take_chrome_command().is_none());
    assert_keys_reach_input(&host);
}

#[test]
fn hidden_or_reopened_panel_rejects_delayed_focus() {
    let (_root, host) = fixture();
    focus(&host, "panel");
    assert!(host.host_keyboard_focus_allowed("panel"));
    assert!(!host.clipboard_keyboard_focus_allowed());
    host.mark_panel_hidden();
    assert!(!host.host_keyboard_focus_allowed("panel"));
    host.panel_show(&json!({})).unwrap();
    assert!(!host.host_keyboard_focus_allowed("panel"));
    drain(&host);
    focus(&host, "panel");
    assert!(host.host_keyboard_focus_allowed("panel"));
    drain(&host);
    focus(&host, "clipboard");
    assert!(!host.host_keyboard_focus_allowed("panel"));
    assert!(host.clipboard_keyboard_focus_allowed());
}

#[test]
fn clipboard_input_deactivation_releases_fake_focus_and_auto_dismiss() {
    use crate::hide_policy::HostDismissAction;
    let mut results = Vec::new();
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        let (_root, host) = fixture();
        focus(&host, "clipboard");
        {
            let mut inner = host.inner.lock();
            inner.config.dismiss_behavior = behavior.into();
            inner.config.enable_edge_hide = true;
            inner.window_pinned = false;
            inner.pointer_in_host = false;
            inner.is_layout_busy = false;
            inner.expand_grace_until_ms = 0;
        }
        assert!(host.hide_snapshot().is_active);
        host.notify_deactivated();
        let inactive = host.hide_snapshot();
        let action = host.poll_hide().window_action;
        results.push((inactive.is_active, inactive.has_fake_focus, action));
    }
    assert_eq!(
        results,
        vec![
            (false, false, HostDismissAction::Hide),
            (false, false, HostDismissAction::Fold),
            (false, false, HostDismissAction::EdgeHide),
        ],
        "leaving text input must release engagement before hide policy evaluates"
    );
}

#[test]
fn repeated_input_focus_does_not_revive_deactivated_clipboard() {
    use crate::hide_policy::HostDismissAction;
    for (behavior, expected) in [("Hide", HostDismissAction::Hide),
        ("Fold", HostDismissAction::Fold), ("EdgeHide", HostDismissAction::EdgeHide)] {
        let (_root, host) = deferred_deactivation_fixture(behavior, true);
        host.notify_deactivated();
        let before = host.hide_snapshot();
        assert!(!before.is_active && !before.has_fake_focus);
        // AppShell and the page can both report the same editable focusin.
        // After the real blur, its late duplicate must not reacquire input.
        focus(&host, "clipboard");
        assert!(host.take_chrome_command().is_none(), "duplicate focus queued native activation");
        let after = host.hide_snapshot();
        assert!(!after.is_active && !after.has_fake_focus,
            "duplicate focus revived an already-deactivated input window: {behavior}");
        assert_eq!(after.foreground_is_host_related, before.foreground_is_host_related);
        assert_eq!(host.poll_hide().window_action, expected);
    }
}

#[test]
fn repeated_input_focus_keeps_deferred_native_input_deactivation() {
    use crate::hide_policy::HostDismissAction;
    for (behavior, expected) in [("Hide", HostDismissAction::Hide),
        ("Fold", HostDismissAction::Fold), ("EdgeHide", HostDismissAction::EdgeHide)] {
        let (_root, host) = deferred_deactivation_fixture(behavior, true);
        host.apply_window_action_for("setSpaOverlayOpen", true, "clipboard");
        host.notify_deactivated();
        assert!(host.inner.lock().clipboard_deactivation_pending);
        focus(&host, "clipboard");
        assert!(host.inner.lock().clipboard_deactivation_pending,
            "duplicate editable focus forgot a deferred native blur: {behavior}");
        assert!(host.hide_snapshot().has_fake_focus && !host.hide_snapshot().is_active);
        host.apply_window_action_for("setSpaOverlayOpen", false, "clipboard");
        assert_eq!(host.poll_hide().window_action, expected);
        assert!(!host.hide_snapshot().has_fake_focus);
    }
}

#[test]
fn repeated_input_focus_still_releases_other_window_no_focus_input() {
    for source in ["clipboard", "panel", "main"] {
        let (_root, host) = fixture();
        focus(&host, source);
        drain(&host);
        if source == "clipboard" {
            host.panel_show(&json!({})).unwrap();
            assert!(host.inner.lock().ui.panel_no_focus_session);
            assert!(host.inner.lock().ui.panel_fake_focus);
        } else {
            host.request_show();
            assert!(host.hide_snapshot().no_focus_session_active);
            assert!(host.hide_snapshot().has_fake_focus);
        }
        drain(&host);
        focus(&host, source);
        let inner = host.inner.lock();
        assert!(!inner.no_focus_session_active && !inner.ui.panel_no_focus_session);
        assert!(source == "clipboard" || !inner.has_host_fake_focus);
        assert!(source == "panel" || !inner.ui.panel_fake_focus);
        drop(inner);
        assert!(host.take_chrome_command().is_none(), "same input owner needs no new focus command");
        assert_keys_reach_input(&host);
    }
}

fn deferred_deactivation_fixture(behavior: &str, native_input: bool) -> (tempfile::TempDir, HostState) {
    let (root, host) = fixture();
    host.mark_panel_hidden();
    if native_input { focus(&host, "clipboard"); }
    {
        let mut inner = host.inner.lock();
        inner.config.dismiss_behavior = behavior.into();
        inner.config.enable_edge_hide = true;
        inner.window_pinned = false;
        inner.pointer_in_host = false;
        inner.is_layout_busy = false;
        inner.expand_grace_until_ms = 0;
        inner.no_focus_show_grace_until_ms = 0;
    }
    drain(&host);
    (root, host)
}

#[test]
fn clipboard_deferred_deactivation_releases_after_last_overlay_closes() {
    use crate::hide_policy::HostDismissAction;
    for native_input in [false, true] {
        for (behavior, expected) in [("Hide", HostDismissAction::Hide),
            ("Fold", HostDismissAction::Fold), ("EdgeHide", HostDismissAction::EdgeHide)] {
            let (_root, host) = deferred_deactivation_fixture(behavior, native_input);
            host.apply_window_action_for("setSpaModalOpen", true, "clipboard");
            host.apply_window_action_for("setSpaOverlayOpen", true, "clipboard");
            host.notify_deactivated();
            assert!(host.hide_snapshot().has_fake_focus);
            assert_eq!(host.poll_hide().window_action, HostDismissAction::None);
            host.apply_window_action_for("setSpaModalOpen", false, "clipboard");
            assert_eq!(host.poll_hide().window_action, HostDismissAction::None);
            assert!(host.hide_snapshot().has_fake_focus, "another overlay still protects the host");
            host.apply_window_action_for("setSpaOverlayOpen", false, "clipboard");
            let action = host.poll_hide().window_action;
            assert!(!host.hide_snapshot().has_fake_focus,
                "deferred deactivation did not release after overlays closed: {behavior}, native_input={native_input}");
            assert_eq!(action, expected);
        }
    }
}

#[test]
fn clipboard_deferred_deactivation_releases_after_show_grace_expires() {
    use crate::hide_policy::HostDismissAction;
    let (_root, host) = deferred_deactivation_fixture("Hide", false);
    // A no-activate host with no deactivation event keeps its intended fake focus.
    assert_eq!(host.poll_hide().window_action, HostDismissAction::None);
    host.inner.lock().no_focus_show_grace_until_ms = chrono::Utc::now().timestamp_millis() + 60_000;
    host.notify_deactivated();
    assert!(host.hide_snapshot().has_fake_focus);
    assert_eq!(host.poll_hide().window_action, HostDismissAction::None);
    host.inner.lock().no_focus_show_grace_until_ms = 0;
    assert_eq!(host.poll_hide().window_action, HostDismissAction::Hide,
        "show grace retained the old deactivation forever");
    assert!(!host.hide_snapshot().has_fake_focus);
}

#[test]
fn clipboard_deferred_deactivation_preserves_new_engagement_and_pinned_visibility() {
    for resume in ["click", "show", "activate", "input", "pin"] {
        let (_root, host) = deferred_deactivation_fixture("Hide", false);
        host.apply_window_action_for("setSpaOverlayOpen", true, "clipboard");
        host.notify_deactivated();
        match resume {
            "click" => { host.set_pointer_in_host(true); crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0); }
            "show" => host.request_show(),
            "activate" => host.notify_activated(),
            "input" => { focus(&host, "clipboard"); }
            "pin" => { host.apply_window_action_for("setPinned", true, "clipboard"); }
            _ => unreachable!(),
        }
        host.set_pointer_in_host(false);
        host.apply_window_action_for("setSpaOverlayOpen", false, "clipboard");
        host.poll_hide();
        assert!(host.hide_snapshot().has_fake_focus && host.hide_snapshot().visible,
            "old deferred deactivation canceled newer {resume} engagement");
    }
    let (_root, host) = deferred_deactivation_fixture("Hide", false);
    host.apply_window_action_for("setPinned", true, "clipboard");
    host.apply_window_action_for("setSpaOverlayOpen", true, "clipboard");
    host.notify_deactivated();
    host.enqueue_chrome(ChromeCommand::ClipboardNavKey { key: "Enter", shift: false });
    host.enqueue_chrome(ChromeCommand::ShowMain);
    host.apply_window_action_for("setSpaOverlayOpen", false, "clipboard");
    host.poll_hide();
    assert!(host.hide_snapshot().visible && !host.hide_snapshot().has_fake_focus,
        "pinned deferred deactivation must release input without hiding");
    assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x28, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 0));
    let commands: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
    assert!(!commands.contains(&ChromeCommand::ClipboardNavKey { key: "Enter", shift: false }));
    assert!(commands.contains(&ChromeCommand::ShowMain));
}

#[test]
fn clipboard_toggle_after_input_deactivation_reopens_no_focus_session() {
    let (_root, host) = fixture();
    focus(&host, "clipboard");
    host.inner.lock().config.dismiss_behavior = "Hide".into();
    host.notify_deactivated();
    assert!(
        host.request_toggle(false),
        "toggle should bring back the disengaged window"
    );
    assert!(host.clipboard_no_activate());
    assert!(!host.clipboard_keyboard_focus_allowed());
    assert!(host.hide_snapshot().has_fake_focus);
}

#[test]
fn stale_foreground_and_hidden_input_requests_do_not_revive_activation() {
    let (_root, host) = fixture();
    focus(&host, "clipboard");
    {
        // Simulate a WinEvent still naming the old host after deactivation.
        let mut inner = host.inner.lock();
        inner.host_hwnd = 1;
        inner.last_fg = 1;
    }
    host.notify_deactivated();
    assert!(!host.hide_snapshot().is_active);
    assert!(!host.hide_snapshot().has_fake_focus);
    host.set_host_hwnd(0);
    host.window_hide();
    drain(&host);
    focus(&host, "clipboard");
    host.notify_activated();
    assert!(!host.hide_snapshot().is_active);
    assert!(!host.hide_snapshot().has_fake_focus);
    assert!(host.take_chrome_command().is_none());
    host.request_show();
    assert!(host.clipboard_no_activate());
    assert!(!host.hide_snapshot().is_active);
    assert!(host.hide_snapshot().has_fake_focus);
}

#[test]
fn ordinary_mode_input_focus_does_not_create_fake_focus() {
    let (_root, host) = fixture();
    host.window_hide();
    host.inner.lock().config.enable_no_focus_mode = false;
    host.request_show();
    assert!(!host.hide_snapshot().has_fake_focus);
    focus(&host, "clipboard");
    assert!(
        !host.hide_snapshot().has_fake_focus,
        "C# skips the fake-focus handoff when no no-focus session is active"
    );
    host.notify_deactivated();
    assert!(
        host.request_toggle(false),
        "the inactive ordinary window should reopen"
    );
    assert!(!host.clipboard_no_activate());
}

#[test]
fn queued_clipboard_input_does_not_replay_after_hide_and_reopen() {
    let (_root, host) = fixture();
    for command in [
        ChromeCommand::RouteDigit { digit: 1, repeat: false },
        ChromeCommand::ClipboardNavKey { key: "Enter", shift: false },
        ChromeCommand::ClipboardAppendSearch('x'),
        ChromeCommand::ClipboardSearchBackspace,
        ChromeCommand::ClipboardSwitchTab { reverse: false },
        ChromeCommand::ClipboardHandleEscape,
        ChromeCommand::ClipboardKeyboardFocus,
        ChromeCommand::ClipboardActivateSearch,
    ] {
        host.enqueue_chrome(command);
    }
    host.enqueue_chrome(ChromeCommand::PanelNavKey("ArrowDown"));
    host.enqueue_chrome(ChromeCommand::ShowMain);
    host.window_hide();
    host.request_show();
    let mut pending = Vec::new();
    while let Some(command) = host.take_chrome_command() {
        pending.push(command);
    }
    assert_eq!(
        pending,
        vec![
            ChromeCommand::PanelNavKey("ArrowDown"),
            ChromeCommand::ShowMain,
            ChromeCommand::Show
        ],
        "old clipboard input must not act on a fresh session; other windows remain independent"
    );
}

#[test]
fn queued_panel_input_does_not_replay_after_either_hide_path() {
    for native in [false, true] {
        let (_root, host) = fixture();
        host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
        host.enqueue_chrome(ChromeCommand::HostKeyboardFocus("panel"));
        host.enqueue_chrome(ChromeCommand::ClipboardAppendSearch('x'));
        if native {
            host.mark_panel_hidden();
        } else {
            host.apply_panel_hide();
        }
        host.panel_show(&json!({})).unwrap();
        let mut pending = Vec::new();
        while let Some(command) = host.take_chrome_command() {
            pending.push(command);
        }
        assert!(
            !pending.iter().any(|command| matches!(
                command,
                ChromeCommand::PanelNavKey(_) | ChromeCommand::HostKeyboardFocus("panel")
            )),
            "old panel Enter/focus must not target a reopened panel: {pending:?}"
        );
        assert!(pending.contains(&ChromeCommand::ClipboardAppendSearch('x')));
        assert!(pending.contains(&ChromeCommand::ShowPanel));
    }
}

#[test]
fn pinned_panel_outside_click_cancels_queued_navigation_before_reengagement() {
    for button in [0x0201, 0x0204] {
        let (_root, host) = fixture();
        host.apply_window_action_for("setPinned", true, "panel");
        host.apply_window_action_for("setPinned", true, "clipboard");
        drain(&host);
        for vk in [0x28, 0x0D] {
            assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
            }, 0));
        }
        host.enqueue_chrome(ChromeCommand::ClipboardAppendSearch('x'));
        host.enqueue_chrome(ChromeCommand::ShowMain);
        crate::hooks::dispatch_mouse_down(&host, button, 99999, 99999);
        {
            let inner = host.inner.lock();
            assert!(inner.ui.panel_visible && inner.ui.panel_no_focus_session);
            assert!(!inner.ui.panel_fake_focus);
        }
        // Reacquiring the same visible panel must not revive its earlier Enter.
        host.panel_show(&json!({})).unwrap();
        let pending: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
        assert!(!pending.iter().any(|command| matches!(command, ChromeCommand::PanelNavKey(_))),
            "old panel navigation survived fake-focus release: {pending:?}");
        // The same outside click narrows the pinned clipboard to digits and
        // retires its old search input. Both windows keep lifecycle commands.
        assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
        assert_eq!(pending, vec![ChromeCommand::ShowMain, ChromeCommand::ShowPanel]);
        assert!(!host.chrome_command_input_allowed(ChromeCommand::ClipboardAppendSearch('x')));
        assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
            vk: 0x28, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 2,
        }, 0));
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PanelNavKey("ArrowDown")));
        assert!(host.take_chrome_command().is_none());
    }
}

#[test]
fn pinned_panel_dequeued_navigation_rechecks_ownership_and_preserves_real_input_focus() {
    let (_root, host) = fixture();
    host.apply_window_action_for("setPinned", true, "panel");
    drain(&host);
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x0D, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 0));
    let old_enter = host.take_chrome_command().unwrap();
    assert_eq!(old_enter, ChromeCommand::PanelNavKey("Enter"));
    assert!(host.chrome_command_input_allowed(old_enter));
    crate::hooks::dispatch_mouse_down(&host, 0x0204, 99999, 99999);
    assert!(host.inner.lock().ui.panel_visible);
    assert!(!host.chrome_command_input_allowed(old_enter),
        "already-dequeued panel navigation survived fake-focus release");
    assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
    assert!(!host.chrome_command_input_allowed(ChromeCommand::ClipboardAppendSearch('x')),
        "outside click must also retire the clipboard's old search-input scope");
    assert!(host.chrome_command_input_allowed(ChromeCommand::RouteDigit { digit: 1, repeat: false }),
        "the visible clipboard retains its narrower digit input scope");
    assert!(host.chrome_command_input_allowed(ChromeCommand::ShowPanel));
    assert_eq!(focus(&host, "panel")["ok"], true);
    assert!(host.host_keyboard_focus_allowed("panel"));
    let focus_command = host.take_chrome_command().unwrap();
    assert_eq!(focus_command, ChromeCommand::HostKeyboardFocus("panel"));
    assert!(host.chrome_command_input_allowed(focus_command));
    assert!(!host.chrome_command_input_allowed(old_enter), "real input must not receive synthetic list Enter");
    assert_keys_reach_input(&host);
}

fn pinned_clipboard_fixture(behavior: &str) -> (tempfile::TempDir, HostState) {
    let (root, host) = fixture();
    host.mark_panel_hidden();
    host.apply_window_action_for("setPinned", true, "clipboard");
    {
        let mut inner = host.inner.lock();
        inner.config.dismiss_behavior = behavior.into();
        inner.no_focus_show_grace_until_ms = 0;
        inner.pointer_in_host = false;
    }
    drain(&host);
    (root, host)
}

fn clipboard_test_key(vk: u32) -> KeyEvent {
    KeyEvent { vk, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1 }
}

fn assert_external_clipboard_key_passes(host: &HostState, key: KeyEvent) {
    assert!(!host.dispatch_low_level_key_for_foreground(key, 0x222), "external KeyDown was captured: {key:?}");
    assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent { down: false, now_ms: 2, ..key }, 0x222),
        "external KeyUp was captured: {key:?}");
    assert!(host.take_chrome_command().is_none(), "external typing must not queue clipboard input or focus");
}

fn enter_external_clipboard_digits(host: &HostState, button: u32) {
    host.set_pointer_in_host(false);
    crate::hooks::dispatch_mouse_down(host, button, 99999, 99999);
    assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
    drain(host);
}

fn queue_clipboard_list_keys(host: &HostState) {
    for vk in [0x31, 0x0D, 0x28, 0x09, 0x08, 0x41, 0x1B] {
        assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
            vk, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
        }, 0));
        assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
            vk, down: false, ctrl: false, alt: false, shift: false, win: false, now_ms: 2,
        }, 0));
    }
}

#[test]
fn pinned_clipboard_deactivation_cancels_queued_list_input_before_reengagement() {
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        let (_root, host) = pinned_clipboard_fixture(behavior);
        queue_clipboard_list_keys(&host);
        host.enqueue_chrome(ChromeCommand::ClipboardKeyboardFocus);
        host.enqueue_chrome(ChromeCommand::HostKeyboardFocus("main"));
        host.enqueue_chrome(ChromeCommand::ShowMain);
        host.notify_deactivated();
        assert!(host.hide_snapshot().visible);
        assert!(!host.hide_snapshot().has_fake_focus);
        // The pinned window stays visible; reopening must not revive its old Enter.
        host.request_show();
        let pending: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
        assert!(!pending.iter().any(|command| command.is_clipboard_list_input()),
            "old clipboard input survived fake-focus release: {behavior}, {pending:?}");
        assert_eq!(pending, vec![ChromeCommand::ClipboardKeyboardFocus,
            ChromeCommand::HostKeyboardFocus("main"), ChromeCommand::ShowMain, ChromeCommand::Show]);
        queue_clipboard_list_keys(&host);
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
    }
}

#[test]
fn pinned_clipboard_dequeued_input_rechecks_engagement_and_preserves_search_handoff() {
    let (_root, host) = pinned_clipboard_fixture("EdgeHide");
    queue_clipboard_list_keys(&host);
    let dequeued: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
    assert_eq!(dequeued.len(), 7);
    for &command in &dequeued { assert!(host.chrome_command_input_allowed(command)); }
    host.notify_deactivated();
    for &command in &dequeued {
        assert!(!host.chrome_command_input_allowed(command),
            "already-dequeued clipboard input survived fake-focus release: {command:?}");
    }
    assert!(host.chrome_command_input_allowed(ChromeCommand::ShowMain));
    assert!(host.chrome_command_input_allowed(ChromeCommand::ClipboardKeyboardFocus));
    host.set_pointer_in_host(true);
    assert!(!host.hide_snapshot().has_fake_focus, "hover must not create persistent fake focus");
    // Hover restores temporary participation even while pinned. Its input epoch
    // still excludes tasks accepted before deactivation.
    queue_clipboard_list_keys(&host);
    let fresh: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
    for command in fresh { assert!(host.chrome_command_input_allowed(command)); }
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x46, down: true, ctrl: true, alt: false, shift: false, win: false, now_ms: 2,
    }, 0));
    assert!(host.clipboard_keyboard_focus_allowed());
    assert!(!host.clipboard_no_activate());
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardKeyboardFocus));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardActivateSearch));
    assert!(host.chrome_command_input_allowed(ChromeCommand::ClipboardActivateSearch),
        "Ctrl+F search activation must survive the intended real-focus handoff");
    for command in dequeued { assert!(!host.chrome_command_input_allowed(command)); }
}

#[test]
fn clipboard_hover_leave_cancels_queued_input_before_return() {
    let (_root, host) = pinned_clipboard_fixture("Fold");
    host.notify_deactivated();
    host.set_pointer_in_host(true);
    // A pinned host participates by hover without creating persistent fake focus.
    queue_clipboard_list_keys(&host);
    assert!(!host.hide_snapshot().has_fake_focus);
    drain(&host);
    queue_clipboard_list_keys(&host);
    host.enqueue_chrome(ChromeCommand::ShowMain);
    host.set_pointer_in_host(false);
    host.set_pointer_in_host(true);
    let pending: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
    assert_eq!(pending, vec![ChromeCommand::ShowMain],
        "old hover input survived pointer-engagement release");
    queue_clipboard_list_keys(&host);
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
}

fn assert_clipboard_navigation_after_pointer_leave(host: &HostState) {
    host.set_pointer_in_host(false);
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x28, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 0), "click engagement must survive leaving the window");
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false }));
    assert!(host.take_chrome_command().is_none());
}

#[test]
fn pinned_clipboard_left_click_restores_persistent_keyboard_engagement() {
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        let (_root, host) = pinned_clipboard_fixture(behavior);
        host.notify_deactivated();
        assert!(!host.hide_snapshot().has_fake_focus);
        host.set_pointer_in_host(true);
        crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
        assert!(host.hide_snapshot().has_fake_focus,
            "pinned clipboard click did not restore fake focus: {behavior}");
        assert_clipboard_navigation_after_pointer_leave(&host);
        crate::hooks::dispatch_mouse_down(&host, 0x0201, 99999, 99999);
        let snapshot = host.hide_snapshot();
        assert!(snapshot.visible && snapshot.has_fake_focus && !snapshot.is_collapsed,
            "pinned outside click must keep the window and keyboard: {behavior}");
        assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
            vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 3,
        }, 0x222), "pinned clipboard stopped digit routing after outside click: {behavior}");
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
        assert!(host.take_chrome_command().is_none());
    }
}

#[test]
fn pinned_clipboard_outside_click_keeps_digits_but_ctrl_f_goes_to_the_external_app() {
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        for button in [0x0201, 0x0204] {
            let (_root, host) = pinned_clipboard_fixture(behavior);
            host.inner.lock().last_non_host_fg = 0x111;
            host.set_pointer_in_host(true);
            crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
            host.set_pointer_in_host(false);
            crate::hooks::dispatch_mouse_down(&host, button, 99999, 99999);
            assert!(host.hide_snapshot().has_fake_focus);
            assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
            }, 0x222), "outside click ended digit routing: {behavior}, button={button:#x}");
            assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
            host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk: 0x31, down: false, ctrl: false, alt: false, shift: false, win: false, now_ms: 2,
            }, 0x222);
            assert_external_clipboard_key_passes(&host, KeyEvent { ctrl: true, ..clipboard_test_key(0x46) });
            assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Released);
            assert!(host.clipboard_external_input_released());
            assert!(!host.clipboard_digit_routing_active());
            for vk in [0x41, 0x31, 0x28, 0x09] {
                assert_external_clipboard_key_passes(&host, clipboard_test_key(vk));
            }
        }
    }
}

#[test]
fn external_clipboard_digits_only_routes_unmodified_digits_and_plain_escape() {
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        let (_root, host) = pinned_clipboard_fixture(behavior);
        enter_external_clipboard_digits(&host, 0x0201);
        for (vk, digit) in [(0x31, 1), (0x30, 0), (0x61, 1), (0x60, 0)] {
            let key = clipboard_test_key(vk);
            assert!(host.dispatch_low_level_key_for_foreground(key, 0x222));
            assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit, repeat: false }));
            host.dispatch_low_level_key_for_foreground(KeyEvent { down: false, now_ms: 2, ..key }, 0x222);
            assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
        }
        for key in [
            KeyEvent { ctrl: true, ..clipboard_test_key(0x1B) },
            KeyEvent { alt: true, ..clipboard_test_key(0x1B) },
            KeyEvent { shift: true, ..clipboard_test_key(0x1B) },
            KeyEvent { win: true, ..clipboard_test_key(0x1B) },
        ] {
            let (_root, modified) = pinned_clipboard_fixture(behavior);
            enter_external_clipboard_digits(&modified, 0x0201);
            assert_external_clipboard_key_passes(&modified, key);
        }
        assert!(host.dispatch_low_level_key_for_foreground(clipboard_test_key(0x1B), 0x222));
        let escape = host.take_chrome_task().unwrap();
        assert_eq!(escape.command(), ChromeCommand::ClipboardHandleEscape);
        host.clipboard_escape_unhandled(Some(&escape.escape_session_token()), escape.escape_input_token().as_deref());
        assert!(!host.hide_snapshot().visible, "plain Escape must still hide a pinned {behavior} host");
    }
}

#[test]
fn external_clipboard_text_releases_before_the_first_letter_and_abc123_all_passes() {
    let (_root, host) = pinned_clipboard_fixture("Hide");
    enter_external_clipboard_digits(&host, 0x0201);
    for vk in [0x41, 0x42, 0x43, 0x31, 0x32, 0x33] {
        assert_external_clipboard_key_passes(&host, clipboard_test_key(vk));
        assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Released);
    }
    assert!(host.hide_snapshot().visible, "typing must not close the pinned clipboard");
}

#[test]
fn external_clipboard_editing_keys_release_without_stealing_the_first_key() {
    for key in [
        clipboard_test_key(0x08), clipboard_test_key(0x2E), clipboard_test_key(0x0D),
        clipboard_test_key(0x20), clipboard_test_key(0xBA), clipboard_test_key(0xE5),
        KeyEvent { ctrl: true, ..clipboard_test_key(0x46) },
        KeyEvent { ctrl: true, ..clipboard_test_key(0x56) },
        KeyEvent { ctrl: true, alt: true, ..clipboard_test_key(0x51) },
    ] {
        let (_root, host) = pinned_clipboard_fixture("Fold");
        enter_external_clipboard_digits(&host, 0x0201);
        assert_external_clipboard_key_passes(&host, key);
        assert!(host.clipboard_external_input_released(), "editing key did not release: {key:?}");
        assert_external_clipboard_key_passes(&host, clipboard_test_key(0x31));
    }
}

#[test]
fn external_clipboard_navigation_and_shift_selection_preserve_digit_only_mode() {
    let (_root, host) = pinned_clipboard_fixture("Hide");
    enter_external_clipboard_digits(&host, 0x0201);
    for key in [
        clipboard_test_key(0x25), clipboard_test_key(0x26), clipboard_test_key(0x27), clipboard_test_key(0x28),
        clipboard_test_key(0x24), clipboard_test_key(0x23), clipboard_test_key(0x09),
        KeyEvent { shift: true, ..clipboard_test_key(0x25) },
        KeyEvent { shift: true, ..clipboard_test_key(0x24) },
        KeyEvent { shift: true, ..clipboard_test_key(0x23) },
        KeyEvent { shift: true, ..clipboard_test_key(0x09) },
        KeyEvent { ctrl: true, ..clipboard_test_key(0x11) },
        KeyEvent { alt: true, ..clipboard_test_key(0x12) },
    ] {
        assert_external_clipboard_key_passes(&host, key);
        assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
        let digit = clipboard_test_key(0x31);
        assert!(host.dispatch_low_level_key_for_foreground(digit, 0x222));
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
        host.dispatch_low_level_key_for_foreground(KeyEvent { down: false, now_ms: 2, ..digit }, 0x222);
    }
}

#[test]
fn external_clipboard_shift_digits_are_text_and_release_digit_ownership() {
    for vk in [0x30, 0x31, 0x39] {
        let (_root, host) = pinned_clipboard_fixture("Hide");
        enter_external_clipboard_digits(&host, 0x0201);
        assert_external_clipboard_key_passes(&host, KeyEvent { shift: true, ..clipboard_test_key(vk) });
        assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Released);
        assert_external_clipboard_key_passes(&host, clipboard_test_key(0x31));
    }
}

#[test]
fn external_clipboard_search_digits_pass_without_mutating_the_query_or_releasing_scope() {
    let (_root, host) = pinned_clipboard_fixture("Hide");
    host.set_clipboard_search_active(true);
    enter_external_clipboard_digits(&host, 0x0201);
    for vk in [0x31, 0x30, 0x61] {
        assert_external_clipboard_key_passes(&host, clipboard_test_key(vk));
        assert!(host.inner.lock().clipboard_search_active,
            "external digits must not clear the query's active state");
        assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
        assert!(!host.clipboard_digit_routing_active());
    }
    host.set_clipboard_search_active(false);
    assert!(host.dispatch_low_level_key_for_foreground(clipboard_test_key(0x31), 0x222));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));

    let (_root, favorites) = pinned_clipboard_fixture("Hide");
    favorites.inner.lock().ui.clipboard_route = "/favorites".into();
    assert_eq!(favorites.clipboard_keyboard_mode(), ClipboardKeyboardMode::Full);
    assert!(!favorites.clipboard_digit_routing_active(), "favorites digits search instead of pasting history");
    let digit = clipboard_test_key(0x31);
    assert!(favorites.dispatch_low_level_key_for_foreground(digit, 0x222));
    assert_eq!(favorites.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
    favorites.dispatch_low_level_key_for_foreground(KeyEvent { down: false, now_ms: 2, ..digit }, 0x222);
    enter_external_clipboard_digits(&favorites, 0x0201);
    assert_external_clipboard_key_passes(&favorites, digit);
    assert_eq!(favorites.clipboard_keyboard_mode(), ClipboardKeyboardMode::DigitsOnly);
    assert!(!favorites.clipboard_digit_routing_active());
    assert_external_clipboard_key_passes(&favorites, clipboard_test_key(0x41));
    assert_eq!(favorites.clipboard_keyboard_mode(), ClipboardKeyboardMode::Released);
    favorites.inner.lock().ui.clipboard_route = "/clipboard".into();
    assert_external_clipboard_key_passes(&favorites, digit);
    favorites.request_show();
    drain(&favorites);
    assert_eq!(favorites.clipboard_keyboard_mode(), ClipboardKeyboardMode::Full);
    assert!(favorites.dispatch_low_level_key_for_foreground(digit, 0x222));
    assert_eq!(favorites.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
}

#[test]
fn external_clipboard_release_survives_hover_fold_expansion_and_more_outside_clicks() {
    let (_root, host) = pinned_clipboard_fixture("Fold");
    enter_external_clipboard_digits(&host, 0x0201);
    assert_external_clipboard_key_passes(&host, clipboard_test_key(0x41));
    host.inner.lock().is_collapsed = true;
    host.set_pointer_in_host(true);
    let decision = host.poll_hide();
    assert_eq!(decision.window_action, crate::hide_policy::HostDismissAction::Expand);
    assert!(!host.hide_snapshot().is_collapsed);
    drain(&host);
    assert_external_clipboard_key_passes(&host, clipboard_test_key(0x31));
    host.set_pointer_in_host(false);
    crate::hooks::dispatch_mouse_down(&host, 0x0204, 99999, 99999);
    assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Released);
    assert_external_clipboard_key_passes(&host, clipboard_test_key(0x31));
    host.set_pointer_in_host(true);
    crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
    assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Full);
    assert_clipboard_navigation_after_pointer_leave(&host);
    enter_external_clipboard_digits(&host, 0x0201);
    assert_external_clipboard_key_passes(&host, clipboard_test_key(0x41));
    host.request_show();
    drain(&host);
    assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Full);
    assert!(host.dispatch_low_level_key_for_foreground(clipboard_test_key(0x31), 0x222));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
}

#[test]
fn external_clipboard_mode_changes_retire_queued_and_dequeued_input_permanently() {
    for release_by_typing in [false, true] {
        let (_root, host) = pinned_clipboard_fixture("Hide");
        if release_by_typing { enter_external_clipboard_digits(&host, 0x0201); }
        host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 1, repeat: false });
        let old = host.take_chrome_task().unwrap();
        host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 2, repeat: false });
        host.enqueue_chrome(ChromeCommand::ClipboardAppendSearch('x'));
        host.enqueue_chrome(ChromeCommand::ShowMain);
        if release_by_typing {
            assert!(!host.dispatch_low_level_key_for_foreground(clipboard_test_key(0x41), 0x222));
        } else {
            host.set_pointer_in_host(false);
            crate::hooks::dispatch_mouse_down(&host, 0x0201, 99999, 99999);
        }
        let pending: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
        assert!(pending.contains(&ChromeCommand::ShowMain));
        assert!(!pending.iter().any(|command| command.is_clipboard_list_input()),
            "mode transition retained stale list work: {pending:?}");
        assert!(!host.chrome_task_input_allowed(old));
        host.request_show();
        drain(&host);
        assert!(!host.chrome_task_input_allowed(old), "a new Full session revived old input");
    }
}

#[test]
fn external_clipboard_unpinned_left_click_preserves_dismiss_modes_and_right_click_does_not_dismiss() {
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        for button in [0x0201, 0x0204] {
            let (_root, host) = pinned_clipboard_fixture(behavior);
            host.apply_window_action_for("setPinned", false, "clipboard");
            host.inner.lock().config.enable_edge_hide = false;
            drain(&host);
            crate::hooks::dispatch_mouse_down(&host, button, 99999, 99999);
            let snapshot = host.hide_snapshot();
            if button == 0x0204 {
                assert!(snapshot.visible && !snapshot.is_collapsed, "outside right click dismissed {behavior}");
                drain(&host);
                assert!(host.dispatch_low_level_key_for_foreground(clipboard_test_key(0x31), 0x222));
                assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
            } else {
                assert!(!snapshot.has_fake_focus);
                assert_eq!(snapshot.visible, behavior == "Fold");
                assert_eq!(snapshot.is_collapsed, behavior == "Fold");
                drain(&host);
                assert_external_clipboard_key_passes(&host, clipboard_test_key(0x31));
            }
        }
    }
}

#[test]
fn dequeued_clipboard_input_survives_foreground_change_but_not_deactivation() {
    let (_root, host) = pinned_clipboard_fixture("Fold");
    host.inner.lock().last_non_host_fg = 0x111;
    host.set_pointer_in_host(true);
    crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
    host.set_pointer_in_host(false);
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x0D, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 0x111));
    let enter = host.take_chrome_task().unwrap();
    assert!(host.chrome_command_input_allowed_for_foreground(enter.command(), 0x222),
        "a current list task must survive a different external foreground");
    assert!(host.prepare_chrome_task(enter));
    assert!(host.hide_snapshot().visible && host.hide_snapshot().has_fake_focus);
    host.on_foreground_hwnd(0x111);
    host.on_foreground_hwnd(0x333);
    assert!(host.prepare_chrome_task(enter));
    host.notify_deactivated();
    assert!(!host.prepare_chrome_task(enter), "actual deactivation must still retire accepted input");
    host.set_pointer_in_host(true);
    crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
    assert!(!host.prepare_chrome_task(enter), "re-engagement must not revive a retired task");
}

#[test]
fn clipboard_caps_guide_releases_when_keyup_reaches_another_foreground() {
    let (_root, host) = pinned_clipboard_fixture("Fold");
    host.inner.lock().last_non_host_fg = 0x111;
    host.set_pointer_in_host(true);
    crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
    host.set_pointer_in_host(false);
    host.inner.lock().keyboard_target_rebind_until_ms = 0;
    assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x14, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 0x111));
    assert!(host.inner.lock().caps_guide_until_keyup);
    assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x14, down: false, ctrl: false, alt: false, shift: false, win: false, now_ms: 2,
    }, 0x222), "CapsLock release must still pass to the new foreground");
    assert!(!host.inner.lock().caps_guide_until_keyup,
        "the released CapsLock left a stale guide in the still-visible pinned host");
    assert!(host.take_chrome_command().is_none());
}

#[test]
fn clipboard_keyboard_routes_digits_across_external_foregrounds() {
    for pinned in [false, true] {
        for behavior in ["Hide", "Fold", "EdgeHide"] {
            let (_root, host) = pinned_clipboard_fixture(behavior);
            host.apply_window_action_for("setPinned", pinned, "clipboard");
            host.inner.lock().last_non_host_fg = 0x111;
            host.set_pointer_in_host(true);
            crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
            host.set_pointer_in_host(false);
            host.inner.lock().keyboard_target_rebind_until_ms = 0;
            drain(&host);
            for (index, foreground) in [0x111, 0x222, 0x333].into_iter().enumerate() {
                let down = KeyEvent {
                    vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: index as u64,
                };
                assert!(host.dispatch_low_level_key_for_foreground(down, foreground),
                    "foreground change ended digit routing: {behavior}, pinned={pinned}");
                assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
                assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent { down: false, ..down }, foreground));
                assert!(host.hide_snapshot().has_fake_focus);
            }
            assert_eq!(host.inner.lock().keyboard_target_hwnd, 0x111,
                "keyboard routing must not rewrite the captured paste fallback");
        }
    }
}

#[test]
fn foreground_decision_cannot_release_new_keyboard_engagement() {
    use std::sync::{Arc, Barrier};

    for resume in ["show", "click", "input"] {
        let (_root, host) = pinned_clipboard_fixture("EdgeHide");
        {
            let mut inner = host.inner.lock();
            inner.window_pinned = false;
            inner.config.enable_edge_hide = true;
            inner.has_host_fake_focus = true;
            inner.pointer_in_host = false;
            inner.foreground_is_host_related = false;
            inner.expand_grace_until_ms = 0;
        }
        let decision_ready = Arc::new(Barrier::new(2));
        let apply_decision = Arc::new(Barrier::new(2));
        let foreground_host = host.clone();
        let observed = decision_ready.clone();
        let apply = apply_decision.clone();
        let foreground = std::thread::spawn(move || {
            foreground_host.evaluate_foreground_change_current_policy(Some(&|decision| {
                assert_eq!(decision.window_action, crate::hide_policy::HostDismissAction::EdgeHide);
                assert_eq!(decision.fake_focus_action, HostFakeFocusAction::Clear,
                    "an ordinary foreground change must retain its existing dismiss behavior");
                observed.wait();
                apply.wait();
            }));
        });
        decision_ready.wait();
        let renew = move |host: HostState| match resume {
            "show" => host.request_show(),
            "click" => {
                host.set_pointer_in_host(true);
                host.notify_outside_click_at(0, 0);
            }
            "input" => { focus(&host, "clipboard"); }
            _ => unreachable!(),
        };
        // Before the fix, the snapshot was already unlocked: complete the new
        // engagement now to reproduce the stale cleanup deterministically.
        // With the transaction held, the same engagement must run after it.
        let transaction_held = host.inner.try_lock().is_none();
        let renewal = if transaction_held {
            let renew_host = host.clone();
            Some(std::thread::spawn(move || renew(renew_host)))
        } else {
            renew(host.clone());
            None
        };
        apply_decision.wait();
        foreground.join().unwrap();
        if let Some(renewal) = renewal { renewal.join().unwrap(); }
        let state = host.hide_snapshot();
        assert!(state.visible && state.has_fake_focus,
            "an older foreground decision released the newer {resume} engagement");
        if resume != "input" {
            host.set_pointer_in_host(false);
            drain(&host);
            assert!(host.dispatch_low_level_key_for_foreground(clipboard_test_key(0x31), 0x222),
                "digits passed through after the newer {resume} engagement");
            assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
        }
        assert!(transaction_held, "foreground policy released its snapshot before applying it");
    }
}

#[test]
fn clipboard_foreground_switch_keeps_keyboard_while_original_mode_stays_visible() {
    for behavior in ["Hide", "Fold", "EdgeHide"] {
        for pinned in [false, true] {
            if behavior == "EdgeHide" && !pinned { continue; }
            let (_root, host) = pinned_clipboard_fixture(behavior);
            host.apply_window_action_for("setPinned", pinned, "clipboard");
            host.inner.lock().last_non_host_fg = 0x111;
            host.set_pointer_in_host(true);
            crate::hooks::dispatch_mouse_down(&host, 0x0201, 0, 0);
            host.set_pointer_in_host(false);
            host.inner.lock().keyboard_target_rebind_until_ms = 0;
            host.on_foreground_hwnd(0x111);
            host.on_foreground_hwnd(0x333);
            let snapshot = host.hide_snapshot();
            assert!(snapshot.visible && snapshot.has_fake_focus,
                "foreground change must preserve participation: {behavior}, pinned={pinned}");
            drain(&host);
            assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
            }, 0x333));
            assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
        }
    }
}

#[test]
fn clipboard_fold_auto_collapse_keeps_digit_routing_until_outside_click() {
    let (_root, host) = pinned_clipboard_fixture("Fold");
    host.apply_window_action_for("setPinned", false, "clipboard");
    {
        let mut inner = host.inner.lock();
        inner.expand_grace_until_ms = 0;
        inner.pointer_in_host = false;
    }
    drain(&host);
    assert_eq!(host.poll_hide().window_action, crate::hide_policy::HostDismissAction::Fold);
    let snapshot = host.hide_snapshot();
    assert!(snapshot.visible && snapshot.is_collapsed && snapshot.has_fake_focus);
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::Fold));
    let down = KeyEvent { vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1 };
    assert!(host.dispatch_low_level_key_for_foreground(down, 0x222));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
    host.dispatch_low_level_key_for_foreground(KeyEvent { down: false, ..down }, 0x222);
    crate::hooks::dispatch_mouse_down(&host, 0x0201, 99999, 99999);
    assert!(!host.hide_snapshot().has_fake_focus);
    assert!(!host.dispatch_low_level_key_for_foreground(down, 0x222));
}

#[test]
fn clipboard_right_click_restores_engagement_without_outside_dismiss() {
    for pinned in [false, true] {
        for behavior in ["Hide", "Fold", "EdgeHide"] {
            let (_root, host) = pinned_clipboard_fixture(behavior);
            host.apply_window_action_for("setPinned", pinned, "clipboard");
            drain(&host);
            host.notify_deactivated();
            host.set_pointer_in_host(true);
            crate::hooks::dispatch_mouse_down(&host, 0x0204, 0, 0);
            assert!(host.hide_snapshot().has_fake_focus,
                "clipboard right click did not restore fake focus: {behavior}, pinned={pinned}");
            assert_clipboard_navigation_after_pointer_leave(&host);
            crate::hooks::dispatch_mouse_down(&host, 0x0204, 99999, 99999);
            let snapshot = host.hide_snapshot();
            assert!(snapshot.visible && !snapshot.is_collapsed,
                "outside right click must not dismiss the clipboard: {behavior}, pinned={pinned}");
            // External right clicks never dismiss or release clipboard participation.
            assert!(snapshot.has_fake_focus, "{behavior}, pinned={pinned}");
            assert!(host.take_chrome_command().is_none());
        }
    }
}

#[test]
fn pinning_clipboard_restores_focus_only_when_changed_in_visible_no_focus_session() {
    let (_root, host) = pinned_clipboard_fixture("Hide");
    host.apply_window_action_for("setPinned", false, "clipboard");
    drain(&host);
    host.notify_deactivated();
    assert!(!host.hide_snapshot().has_fake_focus);
    host.apply_window_action_for("setPinned", true, "clipboard");
    assert!(host.hide_snapshot().has_fake_focus,
        "new clipboard pin did not restore fake focus");
    drain(&host);
    assert_clipboard_navigation_after_pointer_leave(&host);
    host.notify_deactivated();
    host.apply_window_action_for("setPinned", true, "clipboard");
    assert!(!host.hide_snapshot().has_fake_focus, "repeated pin value must not reacquire input");
    host.apply_window_action_for("setPinned", false, "clipboard");
    focus(&host, "clipboard");
    host.notify_deactivated();
    assert!(!host.hide_snapshot().no_focus_session_active);
    host.apply_window_action_for("setPinned", true, "clipboard");
    assert!(!host.hide_snapshot().has_fake_focus, "ordinary focused input must not acquire fake focus");
    host.window_hide();
    host.apply_window_action_for("setPinned", false, "clipboard");
    host.apply_window_action_for("setPinned", true, "clipboard");
    assert!(!host.hide_snapshot().visible && !host.hide_snapshot().has_fake_focus);
}

#[test]
fn queued_digit_from_hook_is_canceled_by_policy_hide_before_reopen() {
    let (_root, host) = fixture();
    host.mark_panel_hidden();
    assert!(host.dispatch_low_level_key_for_foreground(
        KeyEvent {
            vk: 0x31,
            down: true,
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            now_ms: 1,
        },
        0
    ));
    host.apply_window_action("hide", false);
    host.request_show();
    let mut commands = Vec::new();
    while let Some(command) = host.take_chrome_command() {
        commands.push(command);
    }
    assert!(
        !commands.contains(&ChromeCommand::RouteDigit { digit: 1, repeat: false }),
        "dismissed digit paste must not reach the new session"
    );
    assert!(!commands.contains(&ChromeCommand::Hide), "the later show supersedes the old hide");
    assert!(commands.contains(&ChromeCommand::Show));
}

#[test]
fn input_dispatch_rechecks_hidden_or_reloading_webview_and_keeps_lifecycle_commands() {
    let (_root, host) = fixture();
    let digit = ChromeCommand::RouteDigit { digit: 1, repeat: false };
    let enter = ChromeCommand::PanelNavKey("Enter");
    assert!(host.chrome_command_input_allowed(digit));
    assert!(host.chrome_command_input_allowed(enter));
    host.enqueue_chrome(digit);
    let already_dequeued = host.take_chrome_command().unwrap();
    host.window_hide();
    host.mark_panel_hidden();
    host.mark_main_hidden();
    assert!(
        !host.chrome_command_input_allowed(already_dequeued),
        "hide may happen after dequeue but before WebView execution"
    );
    assert!(!host.chrome_command_input_allowed(enter));
    assert!(!host.chrome_command_input_allowed(ChromeCommand::HostKeyboardFocus("main")));
    for command in [
        ChromeCommand::Hide,
        ChromeCommand::Show,
        ChromeCommand::HidePanel,
        ChromeCommand::ShowPanel,
        ChromeCommand::ShowMain,
        ChromeCommand::PushWindowState,
        ChromeCommand::Exit,
    ] {
        assert!(host.chrome_command_input_allowed(command));
    }
    host.request_show();
    host.panel_show(&json!({})).unwrap();
    host.set_clipboard_web_view_ready(false);
    host.set_panel_web_view_ready(false);
    assert!(!host.chrome_command_input_allowed(digit));
    assert!(!host.chrome_command_input_allowed(enter));
    host.set_clipboard_web_view_ready(true);
    host.set_panel_web_view_ready(true);
    assert!(host.chrome_command_input_allowed(digit));
    assert!(host.chrome_command_input_allowed(enter));
}


#[test]
fn webview_reload_cancels_only_its_queued_input_before_a_fast_finish() {
    for source in ["clipboard", "panel"] {
        let (_root, host) = fixture();
        let mut queued = pending_list_commands().to_vec();
        queued.extend([ChromeCommand::ClipboardKeyboardFocus,
            ChromeCommand::PanelNavKey("Enter"), ChromeCommand::HostKeyboardFocus("panel"),
            ChromeCommand::HostKeyboardFocus("main"), ChromeCommand::PushWindowState,
            ChromeCommand::ShowMain]);
        for command in &queued {
            assert!(host.chrome_command_input_allowed(*command));
            host.enqueue_chrome(*command);
        }
        if source == "clipboard" { host.set_clipboard_web_view_ready(false); }
        else { host.set_panel_web_view_ready(false); }
        for command in &queued {
            assert_eq!(host.chrome_command_input_allowed(*command), command.input_window() != Some(source));
        }
        // No UI draining during the load: finishing must not revive the old keys.
        if source == "clipboard" { host.set_clipboard_web_view_ready(true); }
        else { host.set_panel_web_view_ready(true); }
        let expected: Vec<_> = queued.into_iter()
            .filter(|command| command.input_window() != Some(source)).collect();
        let delivered: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
        assert_eq!(delivered, expected, "{source} reload replayed old input");
        let fresh = if source == "clipboard" { ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false } }
            else { ChromeCommand::PanelNavKey("ArrowDown") };
        host.enqueue_chrome(fresh);
        // A repeated completed-page notification cannot cancel current input.
        if source == "clipboard" { host.set_clipboard_web_view_ready(true); }
        else { host.set_panel_web_view_ready(true); }
        assert_eq!(host.take_chrome_command(), Some(fresh));
        assert!(host.chrome_command_input_allowed(fresh));
        assert!(host.take_chrome_command().is_none());
    }
}


#[test]
fn loading_panel_never_falls_back_to_clipboard_and_still_allows_escape() {
    let (_root, host) = fixture();
    let session_state = || {
        let inner = host.inner.lock();
        (inner.window_visible, inner.no_focus_session_active, inner.has_host_fake_focus,
            inner.ui.panel_visible, inner.ui.panel_no_focus_session, inner.ui.panel_fake_focus,
            inner.ui.keyboard_focus_host)
    };
    let before = session_state();
    assert!(before.0 && before.1 && before.2 && before.3 && before.4 && before.5);
    host.set_panel_web_view_ready(false);
    assert!(host.hide_snapshot().web_view_ready, "the independent clipboard remains ready");
    for (vk, shift, ctrl) in [(0x31, false, false), (0x09, false, false),
        (0x09, true, false), (0x08, false, false), (0x41, false, false),
        (0x41, true, false), (0x46, false, true), (0x28, false, false), (0x0D, false, false)] {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk, down, shift, ctrl, alt: false, win: false, now_ms: 1,
            }, 0), "loading panel misrouted vk={vk:x}, down={down}");
            assert!(host.take_chrome_command().is_none(), "loading input reached the clipboard queue");
            assert_eq!(session_state(), before, "loading input changed a window's focus/session");
        }
    }
    host.set_panel_web_view_ready(true);
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x28, down: true, shift: false, ctrl: false, alt: false, win: false, now_ms: 2,
    }, 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PanelNavKey("ArrowDown")));
    assert!(host.take_chrome_command().is_none());
    // Native Esc dismissal must remain available even if loading never finishes.
    host.set_panel_web_view_ready(false);
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x1B, down: true, shift: false, ctrl: false, alt: false, win: false, now_ms: 3,
    }, 0));
    assert!(!host.inner.lock().ui.panel_visible);
    assert!(host.inner.lock().window_visible);
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::HidePanel));
    assert!(host.take_chrome_command().is_none());
    // The still-visible clipboard accepts fresh input only after panel dismissal.
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x31, down: true, shift: false, ctrl: false, alt: false, win: false, now_ms: 4,
    }, 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
    assert!(host.take_chrome_command().is_none());
}

fn assert_ready_panel_passes_keys_without_clipboard_fallback(keys: &[(u32, bool, bool)]) {
    let (_root, host) = fixture();
    let session_state = || {
        let inner = host.inner.lock();
        (inner.window_visible, inner.no_focus_session_active, inner.has_host_fake_focus,
            inner.ui.panel_visible, inner.ui.panel_no_focus_session, inner.ui.panel_fake_focus,
            inner.ui.keyboard_focus_host)
    };
    let before = session_state();
    assert!(before.0 && before.1 && before.2 && before.3 && before.4 && before.5);
    assert!(host.inner.lock().panel_web_view_ready);
    for &(vk, shift, ctrl) in keys {
        for down in [true, false] {
            assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
                vk, down, shift, ctrl, alt: false, win: false, now_ms: 1,
            }, 0), "ready panel misrouted vk={vk:x}, down={down}, shift={shift}, ctrl={ctrl}");
            assert!(host.take_chrome_command().is_none(), "panel input reached the clipboard queue");
            assert_eq!(session_state(), before, "panel input changed a window's focus/session");
        }
    }
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x28, down: true, shift: false, ctrl: false, alt: false, win: false, now_ms: 2,
    }, 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PanelNavKey("ArrowDown")));
    assert!(host.take_chrome_command().is_none());
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x1B, down: true, shift: false, ctrl: false, alt: false, win: false, now_ms: 3,
    }, 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::HidePanel));
    assert!(host.take_chrome_command().is_none());
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x31, down: true, shift: false, ctrl: false, alt: false, win: false, now_ms: 4,
    }, 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::RouteDigit { digit: 1, repeat: false }));
    assert!(host.take_chrome_command().is_none());
}

#[test]
fn ready_panel_unsupported_keys_never_operate_the_clipboard_behind_it() {
    assert_ready_panel_passes_keys_without_clipboard_fallback(&[
        (0x31, false, false), (0x61, false, false), (0x41, false, false),
        (0x41, true, false), (0x09, false, false), (0x09, true, false),
        (0x08, false, false), (0x46, false, true),
    ]);
}

#[test]
fn ready_panel_modified_navigation_never_falls_back_to_clipboard_navigation() {
    assert_ready_panel_passes_keys_without_clipboard_fallback(&[
        (0x28, true, false), (0x0D, true, false), (0x20, true, false),
        (0x24, true, false), (0x23, true, false), (0x28, false, true),
    ]);
}


fn pending_list_commands() -> [ChromeCommand; 8] {
    [ChromeCommand::RouteDigit { digit: 1, repeat: false }, ChromeCommand::ClipboardNavKey { key: "Enter", shift: false },
     ChromeCommand::ClipboardSwitchTab { reverse: false },
     ChromeCommand::ClipboardSearchBackspace, ChromeCommand::ClipboardAppendSearch('x'),
     ChromeCommand::ClipboardHandleEscape, ChromeCommand::ClipboardActivateSearch,
     ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false }]
}

#[test]
fn pending_list_input_is_canceled_on_modal_open_without_losing_editable_focus() {
    for action in ["setSpaModalOpen", "setSpaOverlayOpen"] {
        let (_root, host) = fixture();
        for command in pending_list_commands() { host.enqueue_chrome(command); }
        host.enqueue_chrome(ChromeCommand::ClipboardKeyboardFocus);
        host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
        host.enqueue_chrome(ChromeCommand::ShowMain);
        host.apply_window_action_for(action, true, "clipboard");
        host.apply_window_action_for(action, false, "clipboard");
        let pending: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
        assert_eq!(pending, vec![ChromeCommand::ClipboardKeyboardFocus,
            ChromeCommand::PanelNavKey("Enter"), ChromeCommand::ShowMain, ChromeCommand::PushWindowState],
            "old list input must not replay after closing {action}");
        assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
            vk: 0x28, down: true, ctrl: false, alt: false, shift: false,
            win: false, now_ms: 1,
        }, 0), "fresh navigation must resume after closing {action}");
        // The independently visible panel currently owns fake-focus navigation.
        assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::PanelNavKey("ArrowDown"))));
    }
}

#[test]
fn pending_list_input_dispatch_defers_to_open_overlay_and_preserves_focus_handoff() {
    for action in ["setSpaModalOpen", "setSpaOverlayOpen"] {
        let (_root, host) = fixture();
        let commands = pending_list_commands();
        for command in commands { assert!(host.chrome_command_input_allowed(command)); }
        host.apply_window_action_for(action, true, "clipboard");
        for command in commands {
            assert!(!host.chrome_command_input_allowed(command),
                "already-dequeued {command:?} must not act behind {action}");
        }
        assert!(host.chrome_command_input_allowed(ChromeCommand::ClipboardKeyboardFocus));
        assert!(host.chrome_command_input_allowed(ChromeCommand::PanelNavKey("Enter")));
        assert!(host.chrome_command_input_allowed(ChromeCommand::ShowMain));
    }
}

#[test]
fn pending_list_input_is_canceled_when_editing_starts_in_already_focused_clipboard() {
    let (_root, host) = fixture();
    focus(&host, "clipboard");
    drain(&host);
    for command in pending_list_commands() { host.enqueue_chrome(command); }
    host.enqueue_chrome(ChromeCommand::ClipboardKeyboardFocus);
    let response = host.chrome_web_action(&json!({
        "kind": "clipboardHostChrome", "sourceHostWindow": "clipboard",
        "route": "/clipboard", "isClipboardEditing": true,
        "requestHostKeyboardFocus": true,
    }));
    assert_eq!(response["ok"], true);
    assert!(host.clipboard_keyboard_focus_allowed());
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardKeyboardFocus));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PushWindowState));
    assert!(host.take_chrome_command().is_none(), "same-window editing must invalidate old list input");
    for command in pending_list_commands() {
        assert!(!host.chrome_command_input_allowed(command));
    }
    assert_keys_reach_input(&host);
}


#[test]
fn panel_layer_keys_reach_dialog_without_falling_back_to_clipboard() {
    let (_root, host) = fixture();
    host.apply_window_action_for("setSpaOverlayOpen", true, "panel");
    for vk in [0x0D, 0x1B, 0x28, 0x08, 0x31, 0x46] {
        assert!(!host.dispatch_low_level_key_for_foreground(KeyEvent {
            vk, down: true, ctrl: vk == 0x46, alt: false, shift: false, win: false, now_ms: 1,
        }, 0), "panel dialog lost vk={vk:x}");
        assert!(host.take_chrome_command().is_none());
        assert!(host.inner.lock().ui.panel_visible);
        assert!(host.inner.lock().window_visible);
    }
    host.apply_window_action_for("setSpaOverlayOpen", false, "panel");
    assert!(host.dispatch_low_level_key_for_foreground(KeyEvent {
        vk: 0x28, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
    }, 0));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PanelNavKey("ArrowDown")));
}

#[test]
fn panel_layer_open_cancels_old_navigation_and_preserves_native_focus() {
    let (_root, host) = fixture();
    host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
    host.enqueue_chrome(ChromeCommand::HostKeyboardFocus("panel"));
    host.enqueue_chrome(ChromeCommand::ClipboardAppendSearch('x'));
    host.apply_window_action_for("setSpaOverlayOpen", true, "panel");
    assert!(!host.chrome_command_input_allowed(ChromeCommand::PanelNavKey("Enter")));
    assert!(host.chrome_command_input_allowed(ChromeCommand::HostKeyboardFocus("panel")));
    assert!(host.chrome_command_input_allowed(ChromeCommand::ClipboardAppendSearch('x')));
    assert!(!host.hide_snapshot().spa_modal_open, "panel layers must not freeze clipboard policy");
    host.apply_window_action_for("setSpaOverlayOpen", false, "panel");
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::HostKeyboardFocus("panel")));
    assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardAppendSearch('x')));
    assert!(host.take_chrome_command().is_none());
}

#[test]
fn panel_layer_protects_outside_dismiss_until_closed() {
    let (_root, host) = fixture();
    host.apply_window_action_for("setSpaOverlayOpen", true, "panel");
    host.notify_panel_outside_click_at(0, 0);
    assert!(host.inner.lock().ui.panel_visible, "dialog must protect panel from automatic outside dismiss");
    host.apply_window_action_for("setSpaOverlayOpen", false, "panel");
    host.notify_panel_outside_click_at(0, 0);
    assert!(!host.inner.lock().ui.panel_visible);
    assert!(!host.inner.lock().ui.panel_fake_focus);
}


#[test]
fn dequeued_input_stays_cancelled_after_hide_and_reopen() {
    for source in ["clipboard", "panel", "main"] {
        let (_root, host) = fixture();
        let command = match source {
            "clipboard" => ChromeCommand::ClipboardNavKey { key: "Enter", shift: false },
            "panel" => ChromeCommand::PanelNavKey("Enter"),
            _ => ChromeCommand::HostKeyboardFocus("main"),
        };
        host.enqueue_chrome(command);
        let old = host.take_chrome_task().unwrap();
        assert!(host.chrome_task_input_allowed(old));
        match source {
            "clipboard" => { host.window_hide(); host.request_show(); }
            "panel" => { host.apply_panel_hide(); host.panel_show(&json!({})).unwrap(); }
            _ => { host.apply_main_hide(); host.show_web_route("/settings/clip"); }
        }
        drain(&host);
        assert!(!host.chrome_task_input_allowed(old), "old {source} input revived after reopening");
        assert!(!host.prepare_chrome_task(old), "the UI must also reject cancelled input");
        host.enqueue_chrome(command);
        let fresh = host.take_chrome_task().unwrap();
        assert!(host.chrome_task_input_allowed(fresh), "new {source} input must remain valid");
        assert!(host.prepare_chrome_task(fresh));
    }
}

#[test]
fn dequeued_list_input_stays_cancelled_after_layers_close() {
    for source in ["clipboard", "panel"] {
        let (_root, host) = fixture();
        let command = if source == "clipboard" { ChromeCommand::ClipboardNavKey { key: "Enter", shift: false } }
            else { ChromeCommand::PanelNavKey("Enter") };
        host.enqueue_chrome(command);
        let old = host.take_chrome_task().unwrap();
        host.enqueue_chrome(ChromeCommand::HostKeyboardFocus(source));
        let editor = host.take_chrome_task().unwrap();
        host.apply_window_action_for("setSpaOverlayOpen", true, source);
        assert!(!host.chrome_task_input_allowed(old));
        assert!(host.chrome_task_input_allowed(editor), "opening a layer must preserve its input handoff");
        host.apply_window_action_for("setSpaOverlayOpen", false, source);
        assert!(!host.chrome_task_input_allowed(old), "old {source} Enter revived when the layer closed");
        assert!(!host.prepare_chrome_task(old));
        assert!(host.chrome_task_input_allowed(editor));
        host.enqueue_chrome(command);
        assert!(host.chrome_task_input_allowed(host.take_chrome_task().unwrap()));
    }
}

#[test]
fn dequeued_input_stays_cancelled_after_page_reload() {
    for source in ["clipboard", "panel"] {
        let (_root, host) = fixture();
        let (own, other) = if source == "clipboard" {
            (ChromeCommand::ClipboardNavKey { key: "Enter", shift: false }, ChromeCommand::PanelNavKey("Enter"))
        } else { (ChromeCommand::PanelNavKey("Enter"), ChromeCommand::ClipboardNavKey { key: "Enter", shift: false }) };
        host.enqueue_chrome(own);
        let old = host.take_chrome_task().unwrap();
        host.enqueue_chrome(other);
        let independent = host.take_chrome_task().unwrap();
        if source == "clipboard" { host.set_clipboard_web_view_ready(false); }
        else { host.set_panel_web_view_ready(false); }
        assert!(!host.chrome_task_input_allowed(old));
        assert!(host.chrome_task_input_allowed(independent));
        if source == "clipboard" { host.set_clipboard_web_view_ready(true); }
        else { host.set_panel_web_view_ready(true); }
        assert!(!host.chrome_task_input_allowed(old), "old {source} input revived after reloading");
        assert!(!host.prepare_chrome_task(old));
        assert!(host.chrome_task_input_allowed(independent));
        host.enqueue_chrome(own);
        assert!(host.chrome_task_input_allowed(host.take_chrome_task().unwrap()));
    }
}

#[test]
fn dequeued_panel_navigation_stays_cancelled_after_reengagement() {
    let (_root, host) = fixture();
    host.apply_window_action_for("setPinned", true, "panel");
    drain(&host);
    host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
    let old = host.take_chrome_task().unwrap();
    host.enqueue_chrome(ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false });
    let other = host.take_chrome_task().unwrap();
    host.enqueue_chrome(ChromeCommand::HostKeyboardFocus("panel"));
    let editor = host.take_chrome_task().unwrap();
    host.apply_panel_outside_click(99999, 99999);
    assert!(host.inner.lock().ui.panel_visible);
    assert!(!host.chrome_task_input_allowed(old));
    assert!(host.chrome_task_input_allowed(editor), "list cancellation must preserve native input focus");
    host.panel_show(&json!({})).unwrap();
    drain(&host);
    assert!(!host.chrome_task_input_allowed(old), "reengagement must not revive an old Enter");
    assert!(!host.prepare_chrome_task(old));
    assert!(host.chrome_task_input_allowed(other), "panel cancellation must not affect clipboard input");
    // Showing the panel starts a new window-input lifetime, beyond the list
    // cancellation above. Its earlier focus request must not target this show.
    assert!(!host.chrome_task_input_allowed(editor), "reopening must cancel the earlier native focus request");
    host.enqueue_chrome(ChromeCommand::HostKeyboardFocus("panel"));
    assert!(host.prepare_chrome_task(host.take_chrome_task().unwrap()));
    host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
    assert!(host.prepare_chrome_task(host.take_chrome_task().unwrap()));
}

#[test]
fn dequeued_focus_is_superseded_once_but_duplicate_focus_keeps_new_work() {
    let (_root, host) = fixture();
    let commands = [ChromeCommand::ClipboardKeyboardFocus,
        ChromeCommand::HostKeyboardFocus("panel"), ChromeCommand::HostKeyboardFocus("main")];
    let old: Vec<_> = commands.into_iter().map(|command| {
        host.enqueue_chrome(command);
        host.take_chrome_task().unwrap()
    }).collect();
    focus(&host, "main");
    drain(&host);
    host.request_show();
    host.panel_show(&json!({})).unwrap();
    drain(&host);
    for task in old {
        assert!(!host.chrome_task_input_allowed(task), "new owner must cancel a dequeued focus handoff");
        assert!(!host.prepare_chrome_task(task));
    }
    focus(&host, "clipboard");
    let current = host.take_chrome_task().unwrap();
    assert_eq!(current.command(), ChromeCommand::ClipboardKeyboardFocus);
    focus(&host, "clipboard");
    assert!(host.chrome_task_input_allowed(current), "duplicate focus must preserve current work");
    assert!(host.prepare_chrome_task(current));
    assert!(host.take_chrome_task().is_none());
}


#[test]
fn late_escape_callback_cannot_close_a_new_show() {
    for transition in ["hide-show", "show", "reload"] {
        let (_root, host) = fixture();
        host.mark_panel_hidden();
        let token = host.clipboard_escape_session_token();
        match transition {
            "hide-show" => { host.window_hide(); host.request_show(); }
            "show" => host.request_show(),
            _ => { host.set_clipboard_web_view_ready(false); host.set_clipboard_web_view_ready(true); }
        }
        drain(&host);
        host.clipboard_escape_unhandled(Some(&token), None);
        assert!(host.hide_snapshot().visible, "late Escape closed the new window after {transition}");
        assert!(host.take_chrome_command().is_none());
        let fresh = host.clipboard_escape_session_token();
        host.clipboard_escape_unhandled(Some(&fresh), None);
        assert!(!host.hide_snapshot().visible, "a current unhandled Escape must still hide");
    }
}

#[test]
fn unscoped_escape_callback_cannot_close_window() {
    for token in [None, Some(""), Some("old-or-invalid")] {
        let (_root, host) = fixture();
        host.mark_panel_hidden();
        drain(&host);
        host.clipboard_escape_unhandled(token, None);
        assert!(host.hide_snapshot().visible, "unscoped Escape closed the current window");
        assert!(host.take_chrome_command().is_none());
    }
}

#[test]
fn native_escape_callback_keeps_cancelled_input_cancelled_after_layer_closes() {
    for editing in [false, true] {
        let (_root, host) = fixture();
        host.mark_panel_hidden();
        host.enqueue_chrome(ChromeCommand::ClipboardHandleEscape);
        let old = host.take_chrome_task().unwrap();
        let token = old.escape_session_token();
        let input = old.escape_input_token().unwrap();
        assert!(host.prepare_chrome_task(old));
        if editing {
            host.chrome_web_action(&json!({"kind":"clipboardHostChrome","isClipboardEditing":true}));
            host.chrome_web_action(&json!({"kind":"clipboardHostChrome","isClipboardEditing":false}));
        } else {
            host.apply_window_action("setSpaOverlayOpen", true);
            host.apply_window_action("setSpaOverlayOpen", false);
        }
        assert_ne!(host.clipboard_escape_session_token(), token, "new layer must cancel every Escape route");
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PushWindowState));
        assert!(host.take_chrome_command().is_none());
        host.clipboard_escape_unhandled(Some(&token), Some(&input));
        assert!(host.hide_snapshot().visible, "a cancelled native Escape closed the current window");
        host.enqueue_chrome(ChromeCommand::ClipboardHandleEscape);
        let fresh = host.take_chrome_task().unwrap();
        host.clipboard_escape_unhandled(Some(&fresh.escape_session_token()), fresh.escape_input_token().as_deref());
        assert!(!host.hide_snapshot().visible, "fresh no-focus Escape must still hide");
    }
}

#[test]
fn native_escape_task_does_not_survive_showing_the_same_visible_window() {
    let (_root, host) = fixture();
    host.mark_panel_hidden();
    host.enqueue_chrome(ChromeCommand::ClipboardHandleEscape);
    let old = host.take_chrome_task().unwrap();
    assert!(host.prepare_chrome_task(old));
    host.request_show();
    assert!(!host.prepare_chrome_task(old), "a newly shown window must not receive the old Escape");
    drain(&host);
    host.apply_window_action("escapeUnhandled", false);
    assert!(host.hide_snapshot().visible, "the unscoped action must not bypass callback validation");
}


fn clipboard_escape_layer(host: &HostState, owner: &str, open: bool) {
    if owner == "editing" {
        host.chrome_web_action(&json!({"kind":"clipboardHostChrome", "isClipboardEditing":open}));
    } else {
        host.apply_window_action(owner, open);
    }
}

#[test]
fn late_focused_escape_cannot_hide_a_new_layer() {
    for owner in ["setSpaModalOpen", "setSpaOverlayOpen", "editing"] {
        let (_root, host) = fixture();
        host.mark_panel_hidden();
        let before = host.clipboard_escape_session_token();
        clipboard_escape_layer(&host, owner, true);
        drain(&host);
        host.clipboard_escape_unhandled(Some(&before), None);
        assert!(host.hide_snapshot().visible, "late focused Escape closed new layer: {owner}");
        assert!(host.take_chrome_command().is_none());
        let current = host.clipboard_escape_session_token();
        host.clipboard_escape_unhandled(Some(&current), None);
        assert!(host.hide_snapshot().visible, "active layer must retain its parent: {owner}");
    }
}

#[test]
fn late_focused_escape_stays_cancelled_after_new_layer_closes() {
    for owner in ["setSpaModalOpen", "setSpaOverlayOpen", "editing"] {
        let (_root, host) = fixture();
        host.mark_panel_hidden();
        let before = host.clipboard_escape_session_token();
        clipboard_escape_layer(&host, owner, true);
        clipboard_escape_layer(&host, owner, false);
        drain(&host);
        host.clipboard_escape_unhandled(Some(&before), None);
        assert!(host.hide_snapshot().visible, "late focused Escape revived after layer close: {owner}");
        assert!(host.take_chrome_command().is_none());
        let current = host.clipboard_escape_session_token();
        host.clipboard_escape_unhandled(Some(&current), None);
        assert!(!host.hide_snapshot().visible, "new focused Escape must still hide: {owner}");
    }
}


#[test]
fn new_layer_escape_scope_is_pushed_once_and_other_windows_do_not_cancel_it() {
    for owner in ["setSpaModalOpen", "setSpaOverlayOpen", "editing"] {
        let (_root, host) = fixture();
        host.mark_panel_hidden();
        let old = host.clipboard_escape_session_token();
        clipboard_escape_layer(&host, owner, true);
        let current = host.clipboard_escape_session_token();
        assert_ne!(old, current);
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::PushWindowState));
        assert!(host.take_chrome_command().is_none(), "one owner transition needs one state push");
        clipboard_escape_layer(&host, owner, true);
        assert_eq!(host.clipboard_escape_session_token(), current, "duplicate open cannot cancel new input");
        assert!(host.take_chrome_command().is_none());
        host.apply_window_action_for("setSpaOverlayOpen", true, "panel");
        host.apply_window_action_for("setSpaOverlayOpen", true, "main");
        assert_eq!(host.clipboard_escape_session_token(), current, "other window layers do not own clipboard Escape");
        assert!(host.take_chrome_command().is_none());
        clipboard_escape_layer(&host, owner, false);
        assert_eq!(host.clipboard_escape_session_token(), current, "closing a layer preserves fresh input");
        assert!(host.take_chrome_command().is_none());
    }
}
