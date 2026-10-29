use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture() -> (tempfile::TempDir, HostState, Arc<AtomicUsize>) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.add_text(&json!({"text":"one"})).unwrap();
    host.add_text(&json!({"text":"two"})).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let inner = host.inner.clone();
    host.set_input_runner(move |job| {
        assert!(inner.try_lock().is_some(), "input injection must not hold host state");
        count.fetch_add(1, Ordering::SeqCst);
        job()
    });
    (root, host, calls)
}

fn trigger(host: &HostState, down: bool, now_ms: u64) {
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0xC0, down, alt: true, ctrl: false, shift: false, win: false, now_ms,
    }));
}

#[test]
fn quick_paste_enabled_idle_keeps_ordinary_e_and_win_d_external() {
    for enabled in [false, true] {
        let (_root, host, _) = fixture();
        host.window_hide();
        host.inner.lock().qp_config.enable_paste_enter_shortcut = enabled;
        for (vk, win) in [(0x45, false), (0x44, true)] {
            for down in [true, false] {
                assert!(!host.try_dispatch_input_key_for_foreground(KeyEvent {
                    vk, down, alt: false, ctrl: false, shift: false, win, now_ms: 1,
                }, 0));
            }
        }
    }
}

#[test]
fn quick_paste_passed_keys_never_route_to_background_list_or_panel() {
    for phase in [QuickPastePhase::SessionStarting, QuickPastePhase::SessionActive, QuickPastePhase::Idle] {
        for panel in [false, true] {
            let (_root, host, _) = fixture();
            host.set_clipboard_web_view_ready(true);
            {
                let mut inner = host.inner.lock();
                inner.config.enable_no_focus_mode = true;
                inner.no_focus_session_active = true;
                inner.clipboard_keyboard_mode = ClipboardKeyboardMode::Full;
                inner.window_visible = true;
                inner.has_host_fake_focus = true;
                inner.ui.clipboard_route = "/clipboard".into();
                inner.ui.panel_visible = panel;
                inner.ui.panel_fake_focus = panel;
                inner.ui.panel_no_focus_session = panel;
                inner.panel_web_view_ready = panel;
                inner.qp.machine.phase = phase;
                inner.qp.overlay_visible = phase != QuickPastePhase::SessionStarting;
            }
            while host.take_chrome_command().is_some() {}
            for (vk, ctrl, win) in [(0x45, false, false), (0x33, false, false),
                (0x25, false, false), (0x46, true, false), (0x44, false, true)]
            {
                for down in [true, false] {
                    assert!(!host.try_dispatch_input_key_for_foreground(KeyEvent {
                        vk, down, alt: false, ctrl, shift: false, win, now_ms: 1,
                    }, 0), "QP {phase:?}, panel={panel}, key={vk:#x}");
                }
            }
            assert!(host.take_chrome_command().is_none(), "passed keys must not search/navigate the background");
            {
                let mut inner = host.inner.lock();
                inner.qp.machine.phase = QuickPastePhase::Idle;
                inner.qp.overlay_visible = false;
            }
            assert!(host.try_dispatch_input_key_for_foreground(KeyEvent {
                vk: if panel { 0x25 } else { 0x45 }, down: true,
                alt: false, ctrl: false, shift: false, win: false, now_ms: 2,
            }, 0), "ending QP restores the existing background keyboard rules");
        }
    }
}

#[test]
fn disabled_e_passes_through_actual_hook_and_setting_changes() {
    let (_root, host, _) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.qp.machine = QuickPasteMachine::new(0xC0, false);
        inner.qp.machine.phase = QuickPastePhase::SessionStarting;
        inner.qp_config.enable_paste_enter_shortcut = false;
    }
    let event = |down| KeyEvent {
        vk: 0x45, down, alt: false, ctrl: false, shift: false, win: false, now_ms: 1,
    };
    assert!(!host.try_dispatch_input_key_for_foreground(event(true), 0));
    host.inner.lock().qp_config.enable_paste_enter_shortcut = true;
    assert!(!host.try_dispatch_input_key_for_foreground(event(true), 0));
    assert!(!host.try_dispatch_input_key_for_foreground(event(false), 0));
    assert!(host.try_dispatch_input_key_for_foreground(event(true), 0));
    assert!(host.inner.lock().qp.machine.paste_enter_armed);
}

#[tokio::test]
async fn queued_shortcut_wakes_worker_and_preserves_cancel_before_dispatch() {
    let (_root, host, calls) = fixture();
    let wakeup = host.keyboard_work_notifier();
    trigger(&host, true, 1);
    tokio::time::timeout(Duration::from_millis(100), wakeup.notified()).await
        .expect("shortcut must wake the worker without waiting for its backup timer");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "waking must not run input in the hook");
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x1B, down: true, alt: true, ctrl: false, shift: false, win: false, now_ms: 2,
    }));
    tokio::time::timeout(Duration::from_millis(100), wakeup.notified()).await
        .expect("cancel queued while worker is busy must retain a wake permit");
    host.poll_quick_paste_with_modifiers(|| false);
    assert!(!host.inner.lock().qp.overlay_visible);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn hook_never_injects_alt_menu_input_and_worker_runs_it_once() {
    let (_root, host, calls) = fixture();
    trigger(&host, true, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "SendInput ran inside the low-level hook");
    trigger(&host, true, 2); // Key repeat is not another chord press.
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.poll_quick_paste_with_modifiers(|| true);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "worker disarms the Alt menu once for the accepted press");
    trigger(&host, false, 3);
    trigger(&host, true, 4);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    host.poll_quick_paste_with_modifiers(|| true);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(host.inner.lock().qp.active_index, 1);
}

#[test]
fn released_modifiers_skip_queued_alt_menu_input() {
    let (_root, host, calls) = fixture();
    trigger(&host, true, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "late Ctrl pulse must not reach the foreground after Alt release");
    assert!(host.inner.lock().qp.overlay_visible);
}

#[test]
fn canceled_chord_discards_queued_alt_menu_input() {
    let (_root, host, calls) = fixture();
    trigger(&host, true, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.qp_debug_end();
    host.poll_quick_paste_with_modifiers(|| true);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!host.inner.lock().qp.overlay_visible);
}

#[test]
fn starting_escape_cancels_before_candidate_loading_and_modifier_release() {
    let (_root, host, calls) = fixture();
    host.set_clipboard_ui_runner(|_| panic!("canceled opening must not write clipboard"));
    trigger(&host, true, 1);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x1B, down: true, alt: true, ctrl: false, shift: false, win: false, now_ms: 2,
    }));
    assert!(!host.dispatch_low_level_key(KeyEvent {
        vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 3,
    }));
    host.poll_quick_paste_with_modifiers(|| false);
    let inner = host.inner.lock();
    assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
    assert!(!inner.qp.machine.commit_when_session_opens);
    assert!(!inner.qp.overlay_visible);
    assert!(!inner.chrome.iter().any(|command| *command == ChromeCommand::ShowQuickPaste));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn hotkey_refresh_same_binding_preserves_release_commit() {
    for load_first in [false, true] {
        let (_root, host, _calls) = fixture();
        trigger(&host, true, 1);
        if load_first { host.poll_quick_paste_with_modifiers(|| false); }
        trigger(&host, false, 2);
        let cancel_generation = host.inner.lock().qp.cancel_generation;
        host.settings_form_update(&json!({
            "configId": "quickPaste", "values": { "ListLimit": 12 }
        })).unwrap();
        assert_eq!(host.inner.lock().qp.cancel_generation, cancel_generation,
            "saving unchanged input must not cancel its session");
        assert!(!host.dispatch_low_level_key(KeyEvent {
            vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 3,
        }));
        host.poll_quick_paste_with_modifiers(|| false);
        assert!(!host.inner.lock().qp.overlay_visible);
        let total_usage: i32 = host.store().lock().list_clips().unwrap().iter().map(|item| item.usage_count).sum();
        assert_eq!(total_usage, 1, "the preserved chord must commit exactly once");
    }
}

#[test]
fn hotkey_refresh_changed_or_disabled_retires_old_work_and_accepts_new_binding() {
    for queued_commit in [false, true] {
        for chord in ["Ctrl+F24", ""] {
            let (_root, host, _calls) = fixture();
            host.set_clipboard_ui_runner(|_| panic!("changed binding must not paste old work"));
            host.set_input_runner(|_| panic!("changed binding must not send input"));
            trigger(&host, true, 1);
            if queued_commit {
                host.poll_quick_paste_with_modifiers(|| false);
                trigger(&host, false, 2);
                assert!(!host.dispatch_low_level_key(KeyEvent {
                    vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 3,
                }));
            }
            let old_generation = host.inner.lock().qp.cancel_generation;
            host.settings_form_update(&json!({
                "configId": "quickPaste", "values": { "HotkeyQuickPaste": chord }
            })).unwrap();
            assert!(host.inner.lock().qp.cancel_generation != old_generation);
            host.poll_quick_paste_with_modifiers(|| false);
            {
                let inner = host.inner.lock();
                assert!(!inner.qp.overlay_visible);
                assert!(inner.qp.items.is_empty());
                assert!(inner.qp.pending_commands.is_empty());
            }
            while let Some(command) = host.take_chrome_command() {
                assert_ne!(command, ChromeCommand::ShowQuickPaste, "old show survived binding replacement");
            }
            assert!(host.store().lock().list_clips().unwrap().iter().all(|item| item.usage_count == 0));
            if chord.is_empty() {
                assert!(!host.dispatch_low_level_key(KeyEvent {
                    vk: 0xC0, down: true, alt: true, ctrl: false, shift: false, win: false, now_ms: 4,
                }));
                host.settings_form_update(&json!({
                    "configId": "quickPaste", "values": { "HotkeyQuickPaste": "Ctrl+F24" }
                })).unwrap();
            }
            assert!(host.dispatch_low_level_key(KeyEvent {
                vk: 0x87, down: true, alt: false, ctrl: true, shift: false, win: false, now_ms: 5,
            }));
            host.poll_quick_paste_with_modifiers(|| false);
            assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionActive);
            assert!(host.inner.lock().qp.overlay_visible, "new binding did not open a fresh session");
            host.qp_debug_end();
        }
    }
}

#[test]
fn ctrl_alt_chord_never_injects_a_ctrl_release() {
    let (_root, host, calls) = fixture();
    host.inner.lock().qp.machine = QuickPasteMachine::with_required(0x87, true, true, false, false);
    for (down, now_ms) in [(true, 1), (false, 2), (true, 3)] {
        assert!(host.dispatch_low_level_key(KeyEvent {
            vk: 0x87, down, alt: true, ctrl: true, shift: false, win: false, now_ms,
        }));
        host.poll_quick_paste_with_modifiers(|| true);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "a Ctrl pulse would release this chord's required modifier");
        assert!(host.inner.lock().qp.overlay_visible);
    }
    assert_eq!(host.inner.lock().qp.active_index, 1);
    host.qp_debug_end();
}


fn queue_ready_commit(host: &HostState) -> String {
    trigger(host, true, 1);
    host.poll_quick_paste_with_modifiers(|| false);
    host.qp_web_action(&json!({"action": "placementReady"}));
    let id = host.inner.lock().qp.items[0].id.clone();
    trigger(host, false, 2);
    assert!(!host.dispatch_low_level_key(KeyEvent {
        vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 3,
    }));
    id
}

#[test]
fn quick_paste_input_gap_after_release_cancels_pending_commit_and_overlay() {
    let (_root, host, calls) = fixture();
    let id = queue_ready_commit(&host);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::Idle);
    {
        let _held = host.inner.lock();
        assert!(!host.try_dispatch_input_key(KeyEvent {
            vk: 0x57, down: true, alt: false, ctrl: false,
            shift: false, win: false, now_ms: 4,
        }));
    }
    host.poll_quick_paste_with_modifiers(|| false);
    let inner = host.inner.lock();
    assert!(!inner.qp.overlay_visible, "a canceled release must not leave an inert overlay");
    assert!(inner.qp.pending_commands.is_empty());
    assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
    assert_eq!(inner.store.lock().get_clip(&id).unwrap().usage_count, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn hide_poll_never_dispatches_queued_keyboard_input() {
    let (_root, host, calls) = fixture();
    let id = queue_ready_commit(&host);
    host.set_input_runner({
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("test receiver unavailable".into())
        }
    });
    host.poll_hide();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "hide timers must not enter a blocking input operation");
    assert!(host.inner.lock().qp.overlay_visible, "the keyboard worker still owns the pending commit");
    assert!(!host.inner.lock().qp.pending_commands.is_empty());
    assert_eq!(host.inner.lock().store.lock().get_clip(&id).unwrap().usage_count, 0);
}


#[test]
fn hide_poll_expires_toast_and_hides_window_during_pending_quick_paste() {
    use std::sync::mpsc;
    let (_root, host, _calls) = fixture();
    let id = queue_ready_commit(&host);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Arc::new(Mutex::new(release_rx));
    let inputs = Arc::new(AtomicUsize::new(0));
    host.set_input_runner(move |job| {
        if inputs.fetch_add(1, Ordering::SeqCst) == 0 {
            entered_tx.send(()).map_err(|err| err.to_string())?;
            release_rx.lock().recv_timeout(Duration::from_secs(5)).map_err(|err| err.to_string())?;
        }
        job()
    });
    let keyboard = host.clone();
    let keyboard_worker = std::thread::spawn(move || keyboard.poll_keyboard_work());
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    // Keep cleanup ahead of assertions so a failed check cannot strand a worker.
    let waiting = host.inner.lock().qp.commit_in_flight;
    let usage_before = host.inner.lock().store.lock().get_clip(&id).unwrap().usage_count;
    host.show_host_toast("expired while pasting", "info", None, Some(2500), None);
    {
        let mut inner = host.inner.lock();
        inner.toast.idle_deadline = Some(Instant::now() - Duration::from_secs(1));
        inner.window_visible = true;
        inner.config.dismiss_behavior = "Hide".into();
        inner.window_pinned = false;
        inner.pointer_in_host = false;
        inner.has_host_fake_focus = false;
        inner.is_layout_busy = false;
        inner.expand_grace_until_ms = 0;
        inner.spa_modal_open = false;
        inner.spa_overlay_open = false;
    }
    let policy = host.clone();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let policy_worker = std::thread::spawn(move || {
        let decision = policy.poll_hide();
        let inner = policy.inner.lock();
        let _ = done_tx.send((decision.window_action, inner.window_visible, inner.toast.visible));
    });
    let policy_result = done_rx.recv_timeout(Duration::from_secs(1));
    let _ = release_tx.send(());
    let keyboard_result = keyboard_worker.join();
    let policy_join = policy_worker.join();
    assert!(entered.is_ok(), "the actual queued commit never reached its input worker");
    assert!(waiting);
    assert_eq!(usage_before, 0, "waiting must not mark the item as pasted");
    assert_eq!(policy_result.unwrap(), (crate::hide_policy::HostDismissAction::Hide, false, false));
    keyboard_result.unwrap();
    policy_join.unwrap();
    assert!(!host.inner.lock().qp.commit_in_flight);
    assert_eq!(host.inner.lock().store.lock().get_clip(&id).unwrap().usage_count, 1);
}


#[test]
fn win_chord_queues_menu_disarm_without_input_inside_hook() {
    let (_root, host, calls) = fixture();
    host.inner.lock().qp.machine = QuickPasteMachine::with_required(0x87, false, false, false, true);
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x87, down: true, alt: false, ctrl: false, shift: false, win: true, now_ms: 1,
    }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.poll_quick_paste_with_modifiers(|| true);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the worker must disarm a swallowed Win chord's menu effect");
}

#[test]
fn ctrl_win_chord_does_not_queue_a_ctrl_release() {
    let (_root, host, calls) = fixture();
    host.inner.lock().qp.machine = QuickPasteMachine::with_required(0x87, true, false, false, true);
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x87, down: true, alt: false, ctrl: true, shift: false, win: true, now_ms: 1,
    }));
    host.poll_quick_paste_with_modifiers(|| true);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the chord's held Ctrl must not be released");
}


#[test]
fn early_release_commits_once_without_placement_ready() {
    for fifo in [false, true] {
        let (_root, host, calls) = fixture();
        let ids: Vec<_> = host.inner.lock().store.lock().list_clips().unwrap()
            .iter().map(|item| item.id.clone()).collect();
        if fifo { host.arm_fifo_queue(ids.clone()); }
        while host.take_chrome_command().is_some() {}
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        let inner = host.inner.clone();
        host.set_clipboard_ui_runner(move |job| {
            assert!(inner.try_lock().is_some(), "early commit must release the host lock");
            count.fetch_add(1, Ordering::SeqCst);
            job()
        });
        trigger(&host, true, 1);
        trigger(&host, false, 2);
        assert!(!host.dispatch_low_level_key(KeyEvent {
            vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 3,
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no input inside the hook");
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1, "candidate loading must complete a fast tap without a frontend ack");
        assert!(!host.inner.lock().qp.overlay_visible);
        assert!(!host.inner.lock().chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::ShowQuickPaste)),
            "an already-released chord must not flash the overlay");
        assert_eq!(host.inner.lock().store.lock().get_clip(&ids[0]).unwrap().usage_count, 1);
        if fifo { assert_eq!(host.fifo_status()["remainingCount"], ids.len() - 1); }
        assert_eq!(host.qp_web_action(&json!({"action":"placementReady"}))["reason"], "session-inactive");
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1, "stale ack and polling cannot paste twice");
        assert_eq!(host.inner.lock().store.lock().get_clip(&ids[0]).unwrap().usage_count, 1);
    }
}

#[test]
fn loaded_session_accepts_navigation_and_release_without_placement_ready() {
    let (_root, host, _calls) = fixture();
    trigger(&host, true, 1);
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionActive,
        "candidate readiness must be independent of frontend placement");
    let ids: Vec<_> = host.inner.lock().qp.items.iter().map(|item| item.id.clone()).collect();
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x28, down: true, alt: true, ctrl: false, shift: false, win: false, now_ms: 2,
    }));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().qp.active_index, 1);
    trigger(&host, false, 3);
    assert!(!host.dispatch_low_level_key(KeyEvent {
        vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 4,
    }));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().store.lock().get_clip(&ids[1]).unwrap().usage_count, 1);
    assert_eq!(host.inner.lock().store.lock().get_clip(&ids[0]).unwrap().usage_count, 0);
    assert!(!host.inner.lock().qp.overlay_visible);
}


#[test]
fn early_release_keeps_fast_repeated_trigger_selection() {
    let (_root, host, _calls) = fixture();
    let ids: Vec<_> = host.inner.lock().store.lock().list_clips().unwrap()
        .iter().map(|item| item.id.clone()).collect();
    trigger(&host, true, 1);
    trigger(&host, false, 2);
    trigger(&host, true, 3);
    trigger(&host, false, 4);
    assert!(!host.dispatch_low_level_key(KeyEvent {
        vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 5,
    }));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().store.lock().get_clip(&ids[1]).unwrap().usage_count, 1,
        "a fast second trigger must advance selection before the immediate commit");
    assert_eq!(host.inner.lock().store.lock().get_clip(&ids[0]).unwrap().usage_count, 0);
    assert!(!host.inner.lock().qp.overlay_visible);
}

fn configure_mouse_trigger(host: &HostState, value: &str) {
    host.settings_form_update(&json!({
        "configId": "quickPaste",
        "values": { "MouseWheelTrigger": value, "RememberSelection": false }
    })).unwrap();
    while host.take_chrome_command().is_some() {}
}

fn count_mouse_clipboard_writes(host: &HostState) -> Arc<AtomicUsize> {
    let writes = Arc::new(AtomicUsize::new(0));
    let count = writes.clone();
    let inner = host.inner.clone();
    host.set_clipboard_ui_runner(move |job| {
        assert!(inner.try_lock().is_some(), "mouse commit must release the host lock");
        count.fetch_add(1, Ordering::SeqCst);
        job()
    });
    writes
}

#[test]
fn mouse_quick_paste_native_mouse_router_consumes_only_engaged_release() {
    for (config, button_data, other_data) in [("x1", 1 << 16, 2 << 16), ("x2", 2 << 16, 1 << 16)] {
        let (_root, host, calls) = fixture();
        configure_mouse_trigger(&host, config);
        let writes = count_mouse_clipboard_writes(&host);
        let wheel_data = ((-120_i16 as u16) as u32) << 16;
        assert!(!host.try_dispatch_input_mouse(0x020B, -20_000, -20_000, button_data));
        assert!(!host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, button_data),
            "a plain side click must keep its ordinary release");

        assert!(!host.try_dispatch_input_mouse(0x020B, -20_000, -20_000, button_data));
        assert!(host.try_dispatch_input_mouse(0x020A, -20_000, -20_000, wheel_data));
        assert!(!host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, other_data),
            "the other side button cannot release this gesture");
        assert!(host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, button_data),
            "the native router must suppress the gesture release in other applications");
        assert!(!host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, button_data),
            "release ownership must be consumed exactly once");
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the native callback only queues commit");
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1);

        let committed_inputs = calls.load(Ordering::SeqCst);
        assert!(!host.try_dispatch_input_mouse(0x020B, -20_000, -20_000, button_data));
        assert!(host.try_dispatch_input_mouse(0x020A, -20_000, -20_000, wheel_data));
        assert!(host.try_dispatch_input_mouse(0x0204, -20_000, -20_000, 0));
        assert!(host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, button_data),
            "right cancellation must retain the owned side release");
        assert!(host.try_dispatch_input_mouse(0x0205, -20_000, -20_000, 0));
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), committed_inputs,
            "the canceled gesture must not commit or replay a click");
    }
}

#[test]
fn mouse_quick_paste_fast_wheels_release_commit_correct_item_without_flash() {
    for (config, button) in [("x1", QuickPasteMouseButton::X1), ("x2", QuickPasteMouseButton::X2)] {
        let (_root, host, calls) = fixture();
        host.add_text(&json!({"text":"three"})).unwrap();
        configure_mouse_trigger(&host, config);
        let writes = count_mouse_clipboard_writes(&host);
        let ids: Vec<_> = host.store().lock().list_clips().unwrap()
            .iter().map(|item| item.id.clone()).collect();
        assert!(!host.dispatch_low_level_side_button(button, true));
        // There is no placed overlay or hit rectangle at these screen points.
        assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
        assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
        assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
        assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
        assert!(host.dispatch_low_level_wheel(120, -20_000, -20_000));
        assert!(host.dispatch_low_level_side_button(button, false));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "mouse hooks must not inject input");
        assert_eq!(writes.load(Ordering::SeqCst), 0, "mouse hooks must not load or paste candidates");
        assert!(host.inner.lock().qp.items.is_empty());
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(calls.load(Ordering::SeqCst) > 0, "the worker must execute the queued paste");
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(host.store().lock().get_clip(id).unwrap().usage_count, i32::from(index == 1),
                "signed wheel moves must precede the early-release commit");
        }
        {
            let inner = host.inner.lock();
            assert!(!inner.qp.overlay_visible);
            assert!(!inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::ShowQuickPaste)),
                "release before candidate loading must not flash the overlay");
        }
        let input_calls = calls.load(Ordering::SeqCst);
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), input_calls, "a release can paste only once");
    }
}

#[test]
fn mouse_quick_paste_escape_retires_opening_and_moves_until_fresh_press() {
    let (_root, host, calls) = fixture();
    configure_mouse_trigger(&host, "x1");
    let writes = count_mouse_clipboard_writes(&host);
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x1B, down: true, alt: false, ctrl: false, shift: false, win: false, now_ms: 1,
    }));
    assert!(host.inner.lock().qp.mouse.is_draining());
    assert!(!host.dispatch_low_level_wheel(120, -20_000, -20_000), "canceled wheel intent must pass through");
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0x1B, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 2,
    }), "accepted Escape owns its matching release");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 0, "Escape must retire both loading and queued moves");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "canceled release must neither paste nor replay a click");
    {
        let inner = host.inner.lock();
        assert!(!inner.qp.mouse.is_held());
        assert!(!inner.qp.overlay_visible);
        assert!(!inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::ShowQuickPaste)));
    }
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a fresh gesture is still deferred to the worker");
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 1, "the drained canceled press must permit a fresh session");
    assert_eq!(host.store().lock().list_clips().unwrap().iter().map(|item| item.usage_count).sum::<i32>(), 1);
}

#[test]
fn mouse_quick_paste_only_matching_button_release_commits() {
    let (_root, host, calls) = fixture();
    configure_mouse_trigger(&host, "x2");
    let writes = count_mouse_clipboard_writes(&host);
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionActive);
    assert!(host.inner.lock().qp.overlay_visible);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a mouse session must not pulse keyboard modifiers");
    for (vk, down, alt, now_ms) in [
        (0x12, false, false, 1),
        (0xC0, true, true, 2),
        (0xC0, false, true, 3),
        (0xC0, true, false, 4),
        (0xC0, false, false, 5),
    ] {
        assert!(!host.dispatch_low_level_key(KeyEvent {
            vk, down, alt, ctrl: false, shift: false, win: false, now_ms,
        }), "mouse ownership must not borrow the Alt/backtick trigger");
    }
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(host.inner.lock().qp.overlay_visible, "only the owned side button may end this session");
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, false));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "release must queue rather than inject paste");
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(!host.inner.lock().qp.overlay_visible);
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}

#[test]
fn mouse_quick_paste_side_button_alone_passes_to_other_tools() {
    let (_root, host, calls) = fixture();
    configure_mouse_trigger(&host, "x1");
    let writes = count_mouse_clipboard_writes(&host);
    // Click and press-and-hold (e.g. a Quicker wheel menu) reach other hooks untouched.
    for _ in 0..2 {
        assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
        assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    }
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing is injected for a plain side-button press");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::Idle);
    assert!(!host.dispatch_low_level_wheel(-120, -20_000, -20_000), "wheel without the held button is ordinary input");
    // Once the wheel takes the chord, its release must not reach other tools.
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 1, "releasing the chord still commits");
}

#[test]
fn mouse_quick_paste_plain_click_passes_and_config_change_drains() {
    let (_root, host, calls) = fixture();
    let writes = count_mouse_clipboard_writes(&host);
    for button in [QuickPasteMouseButton::X1, QuickPasteMouseButton::X2] {
        assert!(!host.dispatch_low_level_side_button(button, true), "disabled trigger must pass ordinary input");
        assert!(!host.dispatch_low_level_side_button(button, false));
    }
    assert!(!host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    configure_mouse_trigger(&host, "x1");
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, true));
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, false));
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a passed plain click is never replayed");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    configure_mouse_trigger(&host, "x2");
    assert!(host.inner.lock().qp.mouse.is_draining());
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, true), "old press must drain before a new button can arm");
    assert!(!host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 0, "changing the trigger must cancel queued paste work");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the old release must not inject anything");
    assert!(!host.inner.lock().qp.overlay_visible);
    assert!(!host.inner.lock().chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::ShowQuickPaste)));
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, true));
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the new configured button's plain click also just passes");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}
