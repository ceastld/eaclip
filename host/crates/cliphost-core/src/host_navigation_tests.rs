use super::*;
use tempfile::tempdir;

fn key(vk: u32) -> KeyEvent {
    KeyEvent { vk, down: true, alt: false, ctrl: false, shift: false, win: false, now_ms: 1 }
}

#[test]
fn native_close_clipboard_ends_fake_focus_and_allows_next_show() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.inner.lock().config.enable_no_focus_mode = true;
    for mode in ["Hide", "Fold", "EdgeHide"] {
        host.inner.lock().config.dismiss_behavior = mode.into();
        host.request_show();
        assert!(host.hide_snapshot().has_fake_focus);
        host.dismiss_native_window("clipboard");
        let commands = std::iter::from_fn(|| host.take_chrome_command()).collect::<Vec<_>>();
        assert!(matches!(commands.last(), Some(ChromeCommand::Hide)), "native close must follow a queued show");
        let state = host.hide_snapshot();
        assert!(!state.visible, "{mode}");
        assert!(!state.has_fake_focus, "{mode}");
        assert!(!host.clipboard_no_activate());
        assert!(!host.dispatch_low_level_key(key(0x28)), "hidden window swallowed Down");
        assert!(!host.dispatch_low_level_key(key(0x1B)), "hidden window swallowed Esc");
        host.request_show();
        assert!(host.hide_snapshot().visible);
        assert!(host.hide_snapshot().has_fake_focus);
    }
}

#[test]
fn native_close_main_and_panel_hide_only_the_requested_host() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.request_show();
    host.inner.lock().ui.main_visible = true;
    host.inner.lock().ui.panel_visible = true;
    host.inner.lock().ui.panel_no_focus_session = true;
    host.inner.lock().ui.panel_fake_focus = true;
    while host.take_chrome_command().is_some() {}
    host.dismiss_native_window("panel");
    assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::HidePanel)));
    {
        let inner = host.inner.lock();
        assert!(!inner.ui.panel_visible);
        assert!(!inner.ui.panel_no_focus_session);
        assert!(!inner.ui.panel_fake_focus);
        assert!(inner.ui.main_visible);
    }
    host.dismiss_native_window("main");
    assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::HideMain)));
    assert!(!host.inner.lock().ui.main_visible);
    assert!(host.hide_snapshot().visible);
}

#[test]
fn quick_paste_escape_keeps_later_chord_and_rejects_obsolete_window_commands() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.add_text(&json!({ "text": "Escape then new chord" })).unwrap();
    let key = |vk, down, now_ms| KeyEvent { vk, down, alt: true,
        ctrl: false, shift: false, win: false, now_ms };
    host.qp_handle_key_with_dispatch(key(0xc0, true, 1), true);
    host.poll_quick_paste();
    host.qp_web_action(&json!({ "action": "placementReady" }));
    host.qp_handle_key_with_dispatch(key(0xc0, false, 2), true);
    assert_eq!(host.qp_handle_key_with_dispatch(key(0x1b, true, 3), true)["command"], "Cancel");
    assert_eq!(host.qp_handle_key_with_dispatch(key(0xc0, true, 4), true)["command"], "BeginSession");
    host.poll_quick_paste();
    assert_eq!(host.qp_state_detail()["visible"], true);
    assert!(!host.prepare_chrome_command(ChromeCommand::HideQuickPaste), "old hide closed the fresh session");
    host.qp_debug_end();
    assert!(!host.prepare_chrome_command(ChromeCommand::ShowQuickPaste), "old show reopened a canceled session");
    assert_eq!(host.qp_state_detail()["visible"], false);
}

#[test]
fn quick_paste_native_cancel_retires_queued_begin_but_next_chord_still_opens() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.window_hide();
    host.add_text(&json!({ "text": "cancel queued opening" })).unwrap();
    while host.take_chrome_command().is_some() {}
    let trigger = |now_ms| KeyEvent { vk: 0xc0, down: true, alt: true,
        ctrl: false, shift: false, win: false, now_ms };
    assert_eq!(host.qp_handle_key_with_dispatch(trigger(1), true)["command"], "BeginSession");
    host.dismiss_native_window("quick-paste");
    host.poll_quick_paste();
    assert_eq!(host.qp_state_detail()["visible"], false, "old queued opening survived native close");
    assert_eq!(host.qp_handle_key(KeyEvent { vk: 0x12, down: false, alt: false,
        ctrl: false, shift: false, win: false, now_ms: 2 })["command"], "None");
    // A fresh chord requires releasing the trigger from the canceled press.
    assert_eq!(host.qp_handle_key_with_dispatch(KeyEvent { down: false, ..trigger(2) }, true)["command"], "None");
    assert_eq!(host.qp_handle_key_with_dispatch(trigger(3), true)["command"], "BeginSession");
    host.poll_quick_paste();
    assert_eq!(host.qp_state_detail()["visible"], true, "a fresh chord was incorrectly canceled");
    host.qp_debug_end();
}

#[test]
fn native_close_quick_paste_cancels_reveal_and_releases_keyboard() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.window_hide();
    host.add_text(&json!({ "text": "close without pasting" })).unwrap();
    assert_eq!(host.qp_debug_begin()["sessionActive"], true);
    while host.take_chrome_command().is_some() {}
    host.dismiss_native_window("quick-paste");
    assert_eq!(host.qp_debug_status()["sessionActive"], false);
    assert_eq!(host.qp_state_detail()["visible"], false);
    assert_eq!(host.qp_handle_key(key(0x28))["verdict"], "Pass");
    assert_eq!(host.qp_handle_key(KeyEvent { vk: 0x12, down: false, ..key(0x12) })["command"], "None");
    {
        let inner = host.inner.lock();
        assert!(!inner.qp.awaiting_reveal);
        assert!(inner.qp.reveal_deadline.is_none());
        assert_eq!(inner.qp.push_burst_left, 0);
        assert!(inner.qp.armed.is_none());
        assert!(!inner.qp_wheel_hit.visible);
    }
    assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::HideQuickPaste)));
    assert!(host.take_chrome_command().is_none());
    assert_eq!(host.qp_debug_begin()["sessionActive"], true);
}

#[test]
fn native_close_super_menu_retires_session_and_releases_escape() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.window_hide();
    let opened = host.menu_create_session("favor");
    let session = opened["sessionId"].as_str().unwrap();
    assert_eq!(host.super_menu_state_detail()["visible"], true);
    host.dismiss_native_window("super-menu");
    assert_eq!(host.super_menu_state_detail()["visible"], false);
    assert!(host.menu_sessions()["sessions"].as_array().unwrap().is_empty());
    assert!(host.menu_nodes(session, "root").is_err());
    assert!(!host.dispatch_low_level_key(key(0x1B)));
    host.menu_create_session("favor");
    assert_eq!(host.super_menu_state_detail()["visible"], true);
}

#[test]
fn native_close_toast_clears_hover_and_stops_hidden_resize() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.toast_show();
    host.toast_web_action(&json!({ "action": "hover" }));
    host.dismiss_native_window("copy-toast");
    while host.take_chrome_command().is_some() {}
    host.toast_web_action(&json!({ "action": "contentBounds", "height": 500, "devicePixelRatio": 1 }));
    assert!(host.take_chrome_command().is_none(), "late content bounds reopened a closed toast");
    let inner = host.inner.lock();
    assert!(!inner.toast.visible);
    assert!(!inner.toast.hovered);
    assert!(inner.toast.idle_deadline.is_none());
    assert!(inner.toast.overlay_place.is_none());
}

#[test]
fn native_navigation_restores_hidden_or_folded_clipboard_without_stealing_focus() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.inner.lock().config.enable_no_focus_mode = true;
    host.inner.lock().last_non_host_fg = 123;

    for folded in [false, true] {
        for route in ["/clipboard", "/favorites", "/quick-edit?id=owned-fixture"] {
            if folded {
                host.request_show();
                host.apply_window_action("setCollapsed", true);
                assert!(host.hide_snapshot().is_collapsed);
            } else {
                host.window_hide();
                assert!(!host.hide_snapshot().visible);
                assert!(!host.clipboard_no_activate());
            }
            while host.take_chrome_command().is_some() {}

            host.show_web_route(route);

            let snapshot = host.hide_snapshot();
            assert!(snapshot.visible, "{route}, folded={folded}");
            assert!(!snapshot.is_collapsed);
            assert!(snapshot.has_fake_focus);
            assert!(host.clipboard_no_activate());
            assert!(!host.should_activate_on_show());
            assert_eq!(host.inner.lock().last_non_host_fg, 123);
            assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::Show)));
            assert_eq!(host.take_pending_nav("clipboard").as_deref(), Some(route));
            assert!(host.take_pending_nav("main").is_none());
        }
    }

    host.inner.lock().config.enable_no_focus_mode = false;
    host.window_hide();
    host.show_web_route("/clipboard");
    assert!(host.should_activate_on_show());
    assert!(!host.clipboard_no_activate());
}

#[test]
fn native_main_navigation_keeps_clipboard_hidden() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.window_hide();
    while host.take_chrome_command().is_some() {}
    for route in ["/main/home", "/main/settings", "/main/docs", "/settings/clip", "/docs?topic=keys"] {
        host.mark_main_hidden();
        host.show_web_route(route);
        assert!(host.inner.lock().ui.main_visible);
        assert!(!host.hide_snapshot().visible);
        assert!(matches!(host.take_chrome_command(), Some(ChromeCommand::ShowMain)));
        assert_eq!(host.take_pending_nav("main").as_deref(), Some(route));
        assert!(host.take_pending_nav("clipboard").is_none());
    }
}

#[test]
fn pending_navigation_survives_other_windows_opening_before_commands_are_drained() {
    crate::clipboard::force_headless(true);
    let dir = tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.inner.lock().config.enable_no_focus_mode = true;
    let folder = host.favor_folder_create(&json!({ "title": "navigation fixture" })).unwrap();
    let folder_id = folder["id"].as_str().unwrap();

    host.execute_verb("clip show").unwrap();
    host.execute_verb("setting clip").unwrap();
    host.panel_show(&json!({ "folderId": folder_id })).unwrap();
    // The latest route for one host may replace its own earlier request only.
    host.show_web_route("/favorites");
    host.chrome_web_action(&json!({
        "kind": "appHostCommand", "command": "open", "route": "/main/home"
    }));

    assert_eq!(host.take_pending_nav("main").as_deref(), Some("/main/home"));
    assert!(host.take_pending_nav("main").is_none());
    assert_eq!(host.take_pending_nav("clipboard").as_deref(), Some("/favorites"));
    assert_eq!(host.take_pending_nav("panel"), Some(panel_spa_route(Some(folder_id))));
    assert!(host.take_pending_nav("clipboard").is_none());
    assert!(host.take_pending_nav("panel").is_none());
}
