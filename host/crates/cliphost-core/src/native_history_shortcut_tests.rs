//! The actual nonblocking callback entry point, with isolated host data and
//! synthetic KeyEvents. These checks do not claim a Windows hook/desktop test.
use super::*;

const SHORTCUTS: [(u32, &str); 3] = [(0x41, "SelectAll"), (0x43, "Copy"), (0x50, "Pin")];

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new().prefix("history-shortcuts-").tempdir_in(root).unwrap();
    let host = HostState::open(AppPaths::from_data_root(directory.path().into()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    host.set_native_history_shortcuts_enabled(true);
    {
        let mut inner = host.inner.lock();
        inner.config.enable_no_focus_mode = true;
        inner.no_focus_session_active = true;
        inner.clipboard_keyboard_mode = ClipboardKeyboardMode::Full;
        inner.window_visible = true;
        inner.has_host_fake_focus = true;
        inner.ui.clipboard_route = "/clipboard".into();
    }
    drain(&host);
    (directory, host)
}

fn drain(host: &HostState) {
    while host.take_chrome_task().is_some() {}
}

fn key(vk: u32, down: bool, modifiers: u8) -> KeyEvent {
    KeyEvent { vk, down, ctrl: modifiers & 1 != 0, alt: modifiers & 2 != 0,
        win: modifiers & 4 != 0, shift: modifiers & 8 != 0, now_ms: 1 }
}

fn dispatch(host: &HostState, vk: u32, down: bool, modifiers: u8) -> bool {
    host.try_dispatch_input_key_for_foreground(key(vk, down, modifiers), 0)
}

fn take_shortcut(host: &HostState, command: &'static str) -> ChromeTask {
    let task = host.take_chrome_task().expect("history command");
    assert_eq!(task.command(), ChromeCommand::ClipboardNavKey { key: command, shift: false });
    assert!(host.chrome_task_input_allowed(task));
    assert!(host.take_chrome_task().is_none());
    task
}

#[test]
fn native_history_shortcuts_own_one_command_and_the_complete_main_key_press() {
    let (_directory, host) = fixture();
    for (vk, command) in SHORTCUTS {
        for _ in 0..2 {
            assert!(dispatch(&host, vk, true, 1));
            take_shortcut(&host, command);
            assert!(dispatch(&host, vk, true, 1));
            assert!(!dispatch(&host, 0x11, false, 0), "Ctrl itself belongs to the foreground app");
            assert!(dispatch(&host, vk, true, 0), "repeat after Ctrl release leaked to the app");
            assert!(dispatch(&host, vk, false, 0));
            assert!(!dispatch(&host, vk, false, 0));
            assert!(host.take_chrome_task().is_none(), "held chord executed twice");
        }
    }
    let inner = host.inner.lock();
    assert!(inner.no_focus_session_active && inner.has_host_fake_focus);
    assert_eq!(inner.ui.keyboard_focus_host, None, "list shortcuts must not activate the window");
}

#[test]
fn native_history_shortcuts_preserve_other_modifiers_and_external_digits_only_scope() {
    let (_directory, host) = fixture();
    for (vk, _) in SHORTCUTS {
        for modifiers in (2..=15).filter(|modifiers| *modifiers != 8) {
            assert!(!dispatch(&host, vk, true, modifiers), "stole modifiers={modifiers}");
            assert!(!dispatch(&host, vk, false, modifiers));
            assert!(host.take_chrome_task().is_none());
        }
        assert!(dispatch(&host, vk, true, 8), "plain Shift keeps the existing uppercase search");
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardAppendSearch(char::from_u32(vk).unwrap())));
        assert!(!dispatch(&host, vk, false, 8));
    }
    for mode in [ClipboardKeyboardMode::DigitsOnly, ClipboardKeyboardMode::Released] {
        host.inner.lock().clipboard_keyboard_mode = mode;
        for (vk, _) in SHORTCUTS {
            assert!(!dispatch(&host, vk, true, 1));
            assert!(!dispatch(&host, vk, false, 1));
            assert!(host.take_chrome_task().is_none());
            assert_eq!(host.clipboard_keyboard_mode(), mode,
                "selection/copy/pin must not change the existing external input rules");
        }
    }
}

#[test]
fn native_history_shortcuts_require_a_ready_history_consumer_and_unprotected_input() {
    for unavailable in ["capability", "favorites", "editing", "modal", "overlay", "hidden",
        "runtime", "page", "session", "engagement", "reveal", "ole", "quick-paste", "menu", "panel"]
    {
        let (_directory, host) = fixture();
        {
            let mut inner = host.inner.lock();
            match unavailable {
                "capability" => inner.native_history_shortcuts_enabled = false,
                "favorites" => inner.ui.clipboard_route = "/favorites".into(),
                "editing" => inner.ui.clipboard_editing = true,
                "modal" => inner.spa_modal_open = true,
                "overlay" => inner.spa_overlay_open = true,
                "hidden" => inner.window_visible = false,
                "runtime" => inner.window_runtime_ready = false,
                "page" => inner.clipboard_web_view_ready = false,
                "session" => inner.no_focus_session_active = false,
                "engagement" => inner.has_host_fake_focus = false,
                "reveal" => inner.native_reveal_in_flight = 1,
                "ole" => inner.ole_drag_active = true,
                "quick-paste" => inner.qp.overlay_visible = true,
                "menu" => inner.super_menu.loading = true,
                "panel" => {
                    inner.ui.panel_visible = true;
                    inner.ui.panel_fake_focus = true;
                    inner.ui.panel_no_focus_session = true;
                }
                _ => unreachable!(),
            }
        }
        for (vk, _) in SHORTCUTS {
            assert!(!dispatch(&host, vk, true, 1), "intercepted {unavailable}");
            assert!(!dispatch(&host, vk, false, 1));
            assert!(host.take_chrome_task().is_none(), "queued in {unavailable}");
        }
    }
}

#[test]
fn native_history_shortcuts_keep_filtered_lists_but_yield_to_actual_search_focus() {
    let (_directory, host) = fixture();
    // A nonempty query filters the list. It is not evidence that an input is
    // focused; SelectAll must continue to reach that filtered list.
    host.set_clipboard_search_active(true);
    assert!(dispatch(&host, 0x41, true, 1));
    take_shortcut(&host, "SelectAll");
    assert!(dispatch(&host, 0x41, false, 1));
    host.request_host_keyboard_focus("clipboard");
    drain(&host);
    assert!(!host.inner.lock().no_focus_session_active);
    for (vk, _) in SHORTCUTS {
        assert!(!dispatch(&host, vk, true, 1), "text input lost its Ctrl chord");
        assert!(!dispatch(&host, vk, false, 1));
        assert!(host.take_chrome_task().is_none());
    }
}

#[test]
fn native_history_shortcuts_drain_after_hide_handoff_or_busy_locks() {
    let (_directory, host) = fixture();
    for (vk, command) in SHORTCUTS {
        host.request_show();
        host.set_native_history_shortcuts_enabled(true);
        drain(&host);
        assert!(dispatch(&host, vk, true, 1));
        take_shortcut(&host, command);
        host.window_hide();
        host.set_native_history_shortcuts_enabled(false);
        drain(&host);
        // Holding the real host lock proves that repeat/up drainage does not
        // reenter the ordinary try-lock path or block on UI/database activity.
        let inner = host.inner.lock();
        assert!(dispatch(&host, vk, true, 0));
        assert!(dispatch(&host, vk, false, 0));
        assert!(!host.input_gap_pending());
        drop(inner);
        assert!(!dispatch(&host, vk, false, 0));
        assert!(host.take_chrome_task().is_none());
    }
}

#[test]
fn native_history_shortcuts_do_not_adopt_a_key_that_was_passed_before_reengagement() {
    let (_directory, host) = fixture();
    for (vk, command) in SHORTCUTS {
        // The first Down can be lost to lock contention or belong to a modal.
        // Clearing the input gap resets ordinary key tracking; the dedicated
        // press ownership must still preserve this external held key.
        let inner = host.inner.lock();
        assert!(!dispatch(&host, vk, true, 1));
        drop(inner);
        assert!(host.input_gap_pending());
        assert!(!dispatch(&host, vk, true, 1));
        assert!(!host.input_gap_pending());
        assert!(host.take_chrome_task().is_none());
        assert!(!dispatch(&host, vk, false, 1));
        assert!(dispatch(&host, vk, true, 1));
        take_shortcut(&host, command);
        assert!(dispatch(&host, vk, false, 1));
    }
    host.apply_window_action("setSpaModalOpen", true);
    assert!(!dispatch(&host, 0x41, true, 1));
    host.apply_window_action("setSpaModalOpen", false);
    drain(&host);
    assert!(!dispatch(&host, 0x41, true, 1));
    assert!(!dispatch(&host, 0x41, false, 1));
    assert!(host.take_chrome_task().is_none());
}

#[test]
fn native_history_shortcuts_revalidate_dequeued_work_after_layer_or_capability_changes() {
    for retire in ["capability", "route", "modal", "focus", "digits"] {
        let (_directory, host) = fixture();
        assert!(dispatch(&host, 0x41, true, 1));
        let task = take_shortcut(&host, "SelectAll");
        match retire {
            "capability" => {
                host.set_native_history_shortcuts_enabled(false);
                host.set_native_history_shortcuts_enabled(true);
            }
            "route" => {
                for route in ["/favorites", "/clipboard"] {
                    host.chrome_web_action(&json!({ "kind": "clipboardHostChrome", "route": route }));
                }
            }
            "modal" => {
                host.apply_window_action("setSpaModalOpen", true);
                host.apply_window_action("setSpaModalOpen", false);
            }
            "focus" => {
                host.request_host_keyboard_focus("clipboard");
                host.begin_no_focus_session_if_enabled();
            }
            "digits" => {
                let mut inner = host.inner.lock();
                set_clipboard_keyboard_mode_locked(&mut inner, ClipboardKeyboardMode::DigitsOnly);
                set_clipboard_keyboard_mode_locked(&mut inner, ClipboardKeyboardMode::Full);
            }
            _ => unreachable!(),
        }
        assert!(!host.chrome_task_input_allowed(task), "stale work revived after {retire}");
        assert!(dispatch(&host, 0x41, true, 1));
        assert!(dispatch(&host, 0x41, false, 0));
        drain(&host);
    }
}

#[cfg(windows)]
#[test]
fn native_history_shortcuts_registry_busy_down_is_observed_without_adopting_its_repeat() {
    let _globals = crate::hooks::input_globals_test::exclusive();
    let (_directory, host) = fixture();
    for (vk, command) in SHORTCUTS {
        assert!(!crate::hooks::deliver_registry_busy_key_for_test(&host, key(vk, true, 1)));
        assert!(host.input_gap_pending());
        assert!(!crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, true, 1), false, 0));
        assert!(!host.input_gap_pending());
        assert!(!crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, false, 1), false, 0));
        assert!(host.take_chrome_task().is_none());
        assert!(crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, true, 1), false, 0));
        take_shortcut(&host, command);
        assert!(crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, false, 1), false, 0));
    }
}

#[cfg(windows)]
#[test]
fn native_history_shortcuts_registry_busy_repeat_and_up_drain_and_allow_the_next_press() {
    let _globals = crate::hooks::input_globals_test::exclusive();
    let (_directory, host) = fixture();
    for (vk, command) in SHORTCUTS {
        assert!(crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, true, 1), false, 0));
        take_shortcut(&host, command);
        assert!(crate::hooks::deliver_registry_busy_key_for_test(&host, key(vk, true, 0)));
        assert!(crate::hooks::deliver_registry_busy_key_for_test(&host, key(vk, false, 0)));
        assert!(!host.input_gap_pending(), "a drained event did not miss any routing work");
        assert!(host.take_chrome_task().is_none());
        assert!(crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, true, 1), false, 0));
        take_shortcut(&host, command);
        assert!(crate::hooks::deliver_keyboard_event_for_test(&host, key(vk, false, 1), false, 0));
    }
}

#[cfg(windows)]
#[test]
fn native_history_shortcuts_self_injected_events_never_change_physical_press_ownership() {
    let (_directory, host) = fixture();
    let deliver = |event, own| crate::hooks::deliver_keyboard_event_for_test(
        &host, event, own, if own { crate::paste::HOST_KEYBOARD_INPUT_TAG } else { 0 });
    for (vk, command) in SHORTCUTS {
        assert!(!deliver(key(vk, true, 1), true));
        assert!(deliver(key(vk, true, 1), false));
        take_shortcut(&host, command);
        assert!(!deliver(key(vk, false, 0), true));
        assert!(deliver(key(vk, true, 0), false));
        assert!(host.take_chrome_task().is_none(), "injected Up ended a physical held chord");
        assert!(deliver(key(vk, false, 0), false));
        assert!(!deliver(key(vk, false, 0), false));
    }
}
