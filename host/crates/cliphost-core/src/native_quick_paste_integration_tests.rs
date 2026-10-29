use super::*;
use std::sync::atomic::AtomicUsize;

struct Fixture {
    _root: tempfile::TempDir,
    host: HostState,
    id: String,
    inputs: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
}

fn fixture() -> Fixture {
    crate::clipboard::force_headless(true);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&directory).unwrap();
    let root = tempfile::Builder::new()
        .prefix("native-qp-integration-")
        .tempdir_in(directory)
        .unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
    let id = host
        .add_text(&json!({"text":"first accepted QP text"}))
        .unwrap()["createdIds"][0]
        .as_str()
        .unwrap()
        .to_string();
    let second = host.add_text(&json!({"text":"second QP text"})).unwrap()["createdIds"][0]
        .as_str()
        .unwrap()
        .to_string();
    let items = {
        let store = host.store();
        let store = store.lock();
        vec![
            store.get_clip(&id).unwrap(),
            store.get_clip(&second).unwrap(),
        ]
    };
    {
        let mut inner = host.inner.lock();
        inner.qp.items = items;
        inner.qp.overlay_visible = true;
        inner.qp.awaiting_reveal = true;
        inner.qp_hwnd = 100;
        inner.qp.captured_fg = 42;
        inner.qp.captured_target = Some(InputTarget::capture(42).unwrap());
        inner.qp.session_generation = 2;
        inner.qp.hook_generation = 2;
        inner.qp.session_revision = 4;
        inner.qp.state_revision = 8;
        inner.qp.session_started = Some(Instant::now());
        inner.qp_config.shortcut1_action = "simulate".into();
        inner.qp_config.shortcut2_action = "none".into();
        inner.qp_config.remember_selection = true;
        inner.qp_config.selection_memory_seconds = 10;
    }
    let inputs = Arc::new(AtomicUsize::new(0));
    let counted = inputs.clone();
    host.set_input_runner(move |job| {
        counted.fetch_add(1, Ordering::SeqCst);
        job()
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let counted = writes.clone();
    host.set_clipboard_ui_runner(move |job| {
        job()?;
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    Fixture {
        _root: root,
        host,
        id,
        inputs,
        writes,
    }
}

#[test]
fn quick_paste_command_router_arms_slots_and_arrows_retire_armed_actions() {
    let f = fixture();
    f.host
        .qp_apply_command(QuickPasteCommand::ArmRowAction { shortcut: 1 });
    assert_eq!(f.host.inner.lock().qp.active_index, 0);
    assert_eq!(
        f.host.inner.lock().qp.armed.as_ref().unwrap().kind,
        "simulate"
    );
    f.host
        .qp_apply_command(QuickPasteCommand::ArmRowAction { shortcut: 2 });
    assert!(f.host.inner.lock().qp.armed.is_none());
    f.host
        .qp_apply_command(QuickPasteCommand::ArmRowAction { shortcut: 1 });
    f.host
        .qp_apply_command(QuickPasteCommand::MoveSelection { delta: 1 });
    assert_eq!(f.host.inner.lock().qp.active_index, 1);
    assert!(f.host.inner.lock().qp.armed.is_none());
    assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
}

#[test]
fn quick_paste_native_row_click_routes_without_hiding_or_advancing_memory() {
    let f = fixture();
    assert_eq!(
        f.host
            .qp_web_action(&json!({"action":"nativeRowAction","itemId":f.id,
        "shortcut":1,"sourceHwnd":100,"sessionRevision":4,"stateRevision":8}))["ok"],
        true
    );
    assert!(f.host.inner.lock().qp.overlay_visible);
    assert!(f.host.inner.lock().qp.remembered.is_none());
    assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.host.store().lock().get_clip(&f.id).unwrap().usage_count,
        1
    );
}

#[test]
fn quick_paste_native_row_click_is_retired_by_selection_change_before_input() {
    let f = fixture();
    let host = f.host.clone();
    f.host.set_input_runner(move |job| {
        host.qp_move(1);
        job()
    });
    assert_eq!(
        f.host
            .qp_web_action(&json!({"action":"nativeRowAction","itemId":f.id,
        "shortcut":1,"sourceHwnd":100,"sessionRevision":4,"stateRevision":8}))["ok"],
        false
    );
    assert_eq!(f.host.inner.lock().qp.active_index, 1);
    assert!(f.host.inner.lock().qp.overlay_visible);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.host.store().lock().get_clip(&f.id).unwrap().usage_count,
        0
    );
}

#[test]
fn quick_paste_armed_release_uses_accepted_session_and_advances_only_success() {
    for cancel in [false, true] {
        let f = fixture();
        f.host
            .qp_apply_command(QuickPasteCommand::ArmRowAction { shortcut: 1 });
        if cancel {
            let host = f.host.clone();
            f.host.set_input_runner(move |job| {
                host.qp_cancel_session();
                job()
            });
        }
        f.host.qp_commit(false);
        assert_eq!(
            f.host.store().lock().get_clip(&f.id).unwrap().usage_count,
            i32::from(!cancel)
        );
        assert_eq!(
            f.host
                .inner
                .lock()
                .qp
                .remembered
                .as_ref()
                .unwrap()
                .next_index,
            i32::from(!cancel)
        );
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn quick_paste_plain_commit_rechecks_prepared_and_queued_work_before_input() {
    for cancellation in ["cancel", "hook", "session", "hwnd", "input"] {
        let f = fixture();
        let host = f.host.clone();
        if cancellation == "input" {
            let calls = Arc::new(AtomicUsize::new(0));
            f.host.set_input_runner(move |job| {
                if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    host.qp_cancel_session();
                }
                job()
            });
        } else {
            f.host.set_clipboard_ui_runner(move |job| {
                match cancellation {
                    "cancel" => host.qp_cancel_session(),
                    "hook" => host.inner.lock().qp.hook_generation += 1,
                    "session" => host.inner.lock().qp.session_revision += 1,
                    _ => host.inner.lock().qp_hwnd = 101,
                }
                job()
            });
        }
        f.host.qp_commit(false);
        assert_eq!(
            f.host.store().lock().get_clip(&f.id).unwrap().usage_count,
            0,
            "{cancellation}"
        );
        if cancellation != "input" {
            assert_eq!(f.writes.load(Ordering::SeqCst), 0, "{cancellation}");
        }
        if cancellation != "session" {
            assert_eq!(
                f.host
                    .inner
                    .lock()
                    .qp
                    .remembered
                    .as_ref()
                    .unwrap()
                    .next_index,
                0,
                "{cancellation}"
            );
        }
    }
}

#[test]
fn quick_paste_plain_commit_keeps_cached_target_and_original_usage_pipeline() {
    let f = fixture();
    f.host.inner.lock().last_non_host_fg = 43;
    f.host.qp_commit(false);
    assert_eq!(f.host.inner.lock().qp.captured_target.unwrap().hwnd(), 42);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.host.store().lock().get_clip(&f.id).unwrap().usage_count,
        1
    );
    assert_eq!(
        f.host
            .inner
            .lock()
            .qp
            .remembered
            .as_ref()
            .unwrap()
            .next_index,
        1
    );
}

fn fifo_keyboard_fixture() -> Fixture {
    let f = fixture();
    f.host.qp_cancel_session();
    f.host.set_window_runtime_ready(true);
    f.host.inner.lock().qp_config.shortcut2_action = "simulate".into();
    f
}

fn fifo_keyboard_event(host: &HostState, vk: u32, down: bool, alt: bool, now_ms: u64) -> bool {
    host.dispatch_low_level_key_for_foreground(
        KeyEvent { vk, down, alt, ctrl: false, shift: false, win: false, now_ms },
        42,
    )
}

fn fifo_keyboard_begin(f: &Fixture, now_ms: u64) {
    assert!(fifo_keyboard_event(&f.host, 0xC0, true, true, now_ms));
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(f.host.inner.lock().qp.machine.phase, QuickPastePhase::SessionActive);
    assert!(fifo_keyboard_event(&f.host, 0xC0, false, true, now_ms + 1));
}

fn fifo_keyboard_press(f: &Fixture, vk: u32, now_ms: u64) {
    assert!(fifo_keyboard_event(&f.host, vk, true, true, now_ms));
    fifo_keyboard_event(&f.host, vk, false, true, now_ms + 1);
    f.host.poll_quick_paste_with_modifiers(|| false);
}

fn fifo_keyboard_release(f: &Fixture, now_ms: u64) {
    assert!(!fifo_keyboard_event(&f.host, 0x12, false, false, now_ms));
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert!(!f.host.inner.lock().qp.overlay_visible);
}

fn fifo_keyboard_arm(f: &Fixture) -> Vec<String> {
    fifo_keyboard_begin(f, 10);
    fifo_keyboard_press(f, 0x46, 20);
    let inner = f.host.inner.lock();
    assert!(inner.qp.fifo_arm_only_this_session);
    let ids = inner.fifo.clone().unwrap();
    assert_eq!(ids.len(), 2);
    ids
}

#[test]
fn quick_paste_q_wraps_back_and_backtick_moves_forward_without_arming_fifo() {
    let f = fifo_keyboard_fixture();
    let ids = starting_item_ids(&f);
    fifo_keyboard_begin(&f, 10);
    assert_eq!(f.host.inner.lock().qp.active_index, 0);
    fifo_keyboard_press(&f, 0x51, 20);
    assert_eq!(f.host.inner.lock().qp.active_index, ids.len() - 1);
    fifo_keyboard_press(&f, 0xC0, 30);
    assert_eq!(f.host.inner.lock().qp.active_index, 0);
    fifo_keyboard_press(&f, 0x51, 40);
    assert!(!fifo_keyboard_event(&f.host, 0x57, true, true, 50));
    assert!(!fifo_keyboard_event(&f.host, 0x57, false, true, 51));
    f.host.poll_quick_paste_with_modifiers(|| false);
    {
        let inner = f.host.inner.lock();
        assert_eq!(inner.qp.active_index, ids.len() - 1, "W must not change the selection");
        assert!(inner.fifo.is_none(), "Q navigates without preparing a paste queue");
        assert!(!inner.qp.fifo_arm_only_this_session);
    }
    fifo_keyboard_release(&f, 60);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 0);
    assert_eq!(f.host.store().lock().get_clip(&ids[1]).unwrap().usage_count, 1);
    assert_eq!(f.host.fifo_status()["armed"], false);
}

#[test]
fn quick_paste_fifo_f_only_and_disabled_e_prepare_without_input() {
    for disabled_e in [false, true] {
        let f = fifo_keyboard_fixture();
        let ids = fifo_keyboard_arm(&f);
        if disabled_e {
            f.host.inner.lock().qp_config.enable_paste_enter_shortcut = false;
            assert!(!fifo_keyboard_event(&f.host, 0x45, true, true, 30));
            assert!(!fifo_keyboard_event(&f.host, 0x45, false, true, 31));
            f.host.poll_quick_paste_with_modifiers(|| false);
        }
        fifo_keyboard_release(&f, 40);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.host.inner.lock().fifo.as_ref(), Some(&ids));
        for id in ids {
            assert_eq!(f.host.store().lock().get_clip(&id).unwrap().usage_count, 0);
        }
    }
}

#[test]
fn quick_paste_fifo_f_then_row_shortcut_executes_explicit_action() {
    // Simulation exercises the same accepted armed-action branch as pinning,
    // without invoking an external Snipaste/PixPin process.
    for vk in [0x31, 0x32] {
        let f = fifo_keyboard_fixture();
        let ids = fifo_keyboard_arm(&f);
        fifo_keyboard_press(&f, vk, 30);
        assert_eq!(f.host.inner.lock().qp.armed.as_ref().unwrap().kind, "simulate");
        fifo_keyboard_release(&f, 40);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 1);
        // Row operations preserve the prepared queue, as on later FIFO rounds.
        assert_eq!(f.host.inner.lock().fifo.as_ref(), Some(&ids));
        f.host.qp_commit(false);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 2, "a stale release cannot repeat the action");
    }
}

#[test]
fn quick_paste_fifo_f_then_e_consumes_once_and_next_round_enter_finishes() {
    let f = fifo_keyboard_fixture();
    let ids = fifo_keyboard_arm(&f);
    let generation = f.host.inner.lock().fifo_generation;
    fifo_keyboard_press(&f, 0x45, 30);
    fifo_keyboard_release(&f, 40);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 3, "activate, paste, then Enter");
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.host.fifo_status()["remainingIds"], json!([ids[1]]));
    assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 1);
    f.host.qp_commit(true);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 3);

    fifo_keyboard_begin(&f, 50);
    fifo_keyboard_press(&f, 0x45, 60);
    fifo_keyboard_release(&f, 70);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 6, "the final FIFO item also appends Enter");
    assert_eq!(f.writes.load(Ordering::SeqCst), 2);
    assert_eq!(f.host.fifo_status()["armed"], false);
    assert_eq!(f.host.inner.lock().fifo_generation, generation);
    assert_eq!(f.host.store().lock().get_clip(&ids[1]).unwrap().usage_count, 1);
    assert_eq!(f.host.take_toast_push().unwrap()["kind"], "success");
}

#[test]
fn quick_paste_fifo_f_then_e_failure_consumes_only_completed_paste() {
    for fail_on_input in [2, 3] {
        let f = fifo_keyboard_fixture();
        let ids = fifo_keyboard_arm(&f);
        let calls = f.inputs.clone();
        f.host.set_input_runner(move |job| {
            if calls.fetch_add(1, Ordering::SeqCst) + 1 == fail_on_input {
                Err("synthetic FIFO input failure".into())
            } else {
                job()
            }
        });
        fifo_keyboard_press(&f, 0x45, 30);
        fifo_keyboard_release(&f, 40);
        let pasted = fail_on_input == 3;
        assert_eq!(f.inputs.load(Ordering::SeqCst), fail_on_input);
        assert_eq!(f.writes.load(Ordering::SeqCst), 1);
        assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, i32::from(pasted));
        assert_eq!(f.host.fifo_status()["remainingCount"], if pasted { 1 } else { 2 });
        assert_eq!(f.host.take_toast_push().unwrap()["kind"], "error");
        f.host.qp_commit(true);
        assert_eq!(f.inputs.load(Ordering::SeqCst), fail_on_input);
        assert_eq!(f.host.fifo_status()["remainingCount"], if pasted { 1 } else { 2 });
    }
}

fn queue_starting_keyboard_commands(f: &Fixture, commands: &[u32], now_ms: u64) {
    assert!(fifo_keyboard_event(&f.host, 0xC0, true, true, now_ms));
    assert!(fifo_keyboard_event(&f.host, 0xC0, false, true, now_ms + 1));
    for (index, &vk) in commands.iter().enumerate() {
        let at = now_ms + 2 + index as u64 * 2;
        assert!(fifo_keyboard_event(&f.host, vk, true, true, at), "early key {vk:#x}");
        fifo_keyboard_event(&f.host, vk, false, true, at + 1);
    }
    assert_eq!(f.host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
    assert!(!fifo_keyboard_event(&f.host, 0x12, false, false, now_ms + 100));
    assert!(f.host.inner.lock().qp.machine.commit_when_session_opens);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 0, "the hook must only queue work");
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
}

fn starting_item_ids(f: &Fixture) -> Vec<String> {
    f.host.qp_session_items().unwrap().0.into_iter().map(|item| item.id).collect()
}

fn assert_early_session_finished(f: &Fixture) {
    let inner = f.host.inner.lock();
    assert!(!inner.qp.overlay_visible);
    assert!(inner.qp.pending_commands.is_empty());
    assert!(!inner.chrome.iter().any(|command| *command == ChromeCommand::ShowQuickPaste),
        "an early release must not flash a late overlay");
}

#[test]
fn quick_paste_starting_commands_execute_before_early_release_commit() {
    for vk in [0x31, 0x32, 0x45, 0x44, 0x46] {
        let f = fifo_keyboard_fixture();
        let ids = starting_item_ids(&f);
        queue_starting_keyboard_commands(&f, &[vk], 10);
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_early_session_finished(&f);
        match vk {
            0x31 | 0x32 => {
                assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
                assert_eq!(f.writes.load(Ordering::SeqCst), 0, "row simulation must replace default paste");
                assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 1);
            }
            0x45 => {
                assert_eq!(f.inputs.load(Ordering::SeqCst), 3, "early E must append Enter");
                assert_eq!(f.writes.load(Ordering::SeqCst), 1);
                assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 1);
            }
            0x44 => {
                assert!(matches!(f.host.store().lock().get_clip(&ids[0]), Err(StoreError::NotFound(_))));
                assert_eq!(f.host.store().lock().get_clip(&ids[1]).unwrap().usage_count, 1);
                assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
                assert_eq!(f.writes.load(Ordering::SeqCst), 1);
            }
            _ => {
                assert_eq!(f.inputs.load(Ordering::SeqCst), 0, "early F still only prepares its queue");
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
                assert_eq!(f.host.fifo_status()["remainingIds"], json!([ids[1], ids[0]]));
            }
        }
    }
}

#[test]
fn quick_paste_starting_moves_and_explicit_actions_keep_physical_order() {
    for (keys, simulate, fifo) in [
        (vec![0x28, 0x31], true, false),
        (vec![0xC0, 0x31], true, false),
        (vec![0x31, 0x28], false, false),
        (vec![0x51, 0x31], true, false),
        (vec![0x31, 0x51], false, false),
        (vec![0x46, 0x31], true, true),
        (vec![0x46, 0x45], false, true),
    ] {
        let f = fifo_keyboard_fixture();
        let ids = starting_item_ids(&f);
        queue_starting_keyboard_commands(&f, &keys, 10);
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_early_session_finished(&f);
        assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 0, "{keys:?}");
        assert_eq!(f.host.store().lock().get_clip(&ids[1]).unwrap().usage_count, 1, "{keys:?}");
        assert_eq!(f.inputs.load(Ordering::SeqCst), if fifo && !simulate { 3 } else { 2 }, "{keys:?}");
        assert_eq!(f.writes.load(Ordering::SeqCst), usize::from(!simulate), "{keys:?}");
        if fifo {
            assert_eq!(f.host.fifo_status()["remainingCount"], if simulate { 2 } else { 1 });
        }
    }
}

#[test]
fn quick_paste_starting_fifo_tail_e_commits_and_enters_once() {
    let f = fifo_keyboard_fixture();
    f.host.arm_fifo_queue(vec![f.id.clone()]);
    let generation = f.host.inner.lock().fifo_generation;
    queue_starting_keyboard_commands(&f, &[0x45], 10);
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_early_session_finished(&f);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 3);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.host.store().lock().get_clip(&f.id).unwrap().usage_count, 1);
    assert_eq!(f.host.fifo_status()["armed"], false);
    assert_eq!(f.host.inner.lock().fifo_generation, generation);
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 3);
}

#[test]
fn quick_paste_starting_cancel_retires_early_actions_and_allows_fresh_chord() {
    let f = fifo_keyboard_fixture();
    queue_starting_keyboard_commands(&f, &[0x44, 0x46, 0x45], 10);
    assert!(fifo_keyboard_event(&f.host, 0x1B, true, false, 120));
    fifo_keyboard_event(&f.host, 0x1B, false, false, 121);
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_early_session_finished(&f);
    assert_eq!(f.host.store().lock().list_clips().unwrap().len(), 2);
    assert_eq!(f.host.fifo_status()["armed"], false);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    queue_starting_keyboard_commands(&f, &[0x45], 200);
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_early_session_finished(&f);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 3);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
}

#[test]
fn quick_paste_starting_mouse_row_action_precedes_side_release_commit() {
    let f = fifo_keyboard_fixture();
    let ids = starting_item_ids(&f);
    f.host.inner.lock().qp.mouse.configure(Some(QuickPasteMouseButton::X1));
    assert!(!f.host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(f.host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert_eq!(f.host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
    assert!(fifo_keyboard_event(&f.host, 0x31, true, false, 10));
    fifo_keyboard_event(&f.host, 0x31, false, false, 11);
    assert!(f.host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_early_session_finished(&f);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    assert_eq!(f.host.store().lock().get_clip(&ids[0]).unwrap().usage_count, 1);
}

#[test]
fn quick_paste_starting_delete_cannot_reveal_before_queued_release() {
    let f = fifo_keyboard_fixture();
    let ids = starting_item_ids(&f);
    queue_starting_keyboard_commands(&f, &[0x44], 10);
    // Step the same hook-created queue to observe the state before Commit
    // closes it. A retained GPUI window polls geometry without a Show command.
    let begin = f.host.inner.lock().qp.pending_commands.pop_front().unwrap();
    assert_eq!(begin.command, QuickPasteCommand::BeginSession);
    assert!(f.host.qp_begin_session_from_press(Some(begin)));
    assert!(f.host.inner.lock().qp.overlay_visible);
    assert!(!f.host.inner.lock().qp.awaiting_reveal);
    assert!(f.host.qp_overlay_place().is_none());
    let delete = f.host.inner.lock().qp.pending_commands.pop_front().unwrap();
    assert_eq!(delete.command, QuickPasteCommand::DeleteActive);
    f.host.qp_apply_command(delete.command);
    assert!(matches!(f.host.store().lock().get_clip(&ids[0]), Err(StoreError::NotFound(_))));
    assert!(f.host.qp_overlay_place().is_none(), "reloading after D must not reveal a released session");
    assert_eq!(f.host.qp_web_action(&json!({"action":"placementReady"}))["reason"], "session-inactive",
        "a retained presenter's late placement must not reveal it either");
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_early_session_finished(&f);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.host.store().lock().get_clip(&ids[1]).unwrap().usage_count, 1);
}

#[test]
fn quick_paste_escape_retires_pending_commit_before_input_and_preserves_main_window() {
    for fifo in [false, true] {
        let f = fifo_keyboard_fixture();
        if fifo { f.host.arm_fifo_queue(vec![f.id.clone()]); }
        fifo_keyboard_begin(&f, 10);
        assert!(!fifo_keyboard_event(&f.host, 0x12, false, false, 20));
        {
            let mut inner = f.host.inner.lock();
            assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
            assert!(inner.qp.overlay_visible);
            assert!(inner.qp.pending_commands.iter().any(|work|
                matches!(work.command, QuickPasteCommand::Commit { .. })));
            inner.window_visible = true;
            inner.has_host_fake_focus = true;
            inner.chrome.clear();
        }
        assert!(fifo_keyboard_event(&f.host, 0x1B, true, false, 21));
        assert!(!f.host.inner.lock().qp.overlay_visible, "cancel is immediate, before the worker");
        assert!(fifo_keyboard_event(&f.host, 0x1B, true, false, 22));
        assert!(fifo_keyboard_event(&f.host, 0x1B, false, false, 23));
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert!(f.host.inner.lock().window_visible, "held Escape must not close the layer below");
        assert_eq!(f.host.fifo_status()["armed"], false);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.host.store().lock().get_clip(&f.id).unwrap().usage_count, 0);
    }
}

#[test]
fn quick_paste_fifo_escape_cancels_opening_and_active_queue_only_once() {
    for load in [false, true] {
        let f = fifo_keyboard_fixture();
        f.host.arm_fifo_queue(vec![f.id.clone()]);
        let generation = f.host.inner.lock().fifo_generation;
        assert!(fifo_keyboard_event(&f.host, 0xC0, true, true, 10));
        if load { f.host.poll_quick_paste_with_modifiers(|| false); }
        assert!(fifo_keyboard_event(&f.host, 0x1B, true, true, 11));
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(f.host.fifo_status()["armed"], false);
        assert_eq!(f.host.inner.lock().fifo_generation, generation + 1);
        f.host.arm_fifo_queue(vec![f.id.clone()]);
        let new_generation = f.host.inner.lock().fifo_generation;
        assert!(fifo_keyboard_event(&f.host, 0x1B, true, false, 12));
        assert!(fifo_keyboard_event(&f.host, 0x1B, false, false, 13));
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(f.host.inner.lock().fifo_generation, new_generation);
        assert_eq!(f.host.fifo_status()["remainingIds"], json!([f.id]));
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn quick_paste_fifo_escape_cannot_cancel_queue_replaced_before_worker() {
    for load in [false, true] {
        let f = fifo_keyboard_fixture();
        f.host.arm_fifo_queue(vec![f.id.clone()]);
        assert!(fifo_keyboard_event(&f.host, 0xC0, true, true, 10));
        if load { f.host.poll_quick_paste_with_modifiers(|| false); }
        assert!(fifo_keyboard_event(&f.host, 0x1B, true, true, 11));
        f.host.arm_fifo_queue(vec![f.id.clone()]);
        let generation = f.host.inner.lock().fifo_generation;
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(f.host.inner.lock().fifo_generation, generation);
        assert_eq!(f.host.fifo_status()["remainingIds"], json!([f.id]));
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn quick_paste_fifo_escape_survives_input_gap_without_canceling_new_queue() {
    for replace in [false, true] {
        let f = fifo_keyboard_fixture();
        f.host.arm_fifo_queue(vec![f.id.clone()]);
        fifo_keyboard_begin(&f, 10);
        assert!(fifo_keyboard_event(&f.host, 0x1B, true, true, 20));
        if replace { f.host.arm_fifo_queue(vec![f.id.clone()]); }
        let generation = f.host.inner.lock().fifo_generation;
        f.host.input_was_busy.store(true, Ordering::Release);
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(f.host.fifo_status()["armed"], replace);
        assert_eq!(f.host.inner.lock().fifo_generation, generation + u64::from(!replace));
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn quick_paste_debug_escape_matches_pending_commit_and_fifo_cancellation() {
    let f = fifo_keyboard_fixture();
    f.host.arm_fifo_queue(vec![f.id.clone()]);
    fifo_keyboard_begin(&f, 10);
    assert!(!fifo_keyboard_event(&f.host, 0x12, false, false, 20));
    let escape = starting_escape_key(0x1B, true);
    assert_eq!(f.host.qp_handle_key(escape)["command"], "Cancel");
    assert_eq!(f.host.fifo_status()["armed"], false);
    assert_eq!(f.host.qp_handle_key(escape)["command"], "None");
    assert_eq!(f.host.qp_handle_key(starting_escape_key(0x1B, false))["verdict"], "Swallow");
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn quick_paste_pending_commit_does_not_take_over_previously_passed_escape() {
    for debug in [false, true] {
        let f = fifo_keyboard_fixture();
        fifo_keyboard_begin(&f, 10);
        let mut passed = starting_escape_key(0x1B, true);
        passed.ctrl = true;
        assert!(!f.host.dispatch_low_level_key_for_foreground(passed, 42));
        assert!(!fifo_keyboard_event(&f.host, 0x12, false, false, 20));
        let escape = starting_escape_key(0x1B, true);
        if debug {
            assert_eq!(f.host.qp_handle_key(escape)["verdict"], "Pass");
            assert_eq!(f.host.qp_handle_key(starting_escape_key(0x1B, false))["verdict"], "Pass");
            assert_eq!(f.host.qp_handle_key(escape)["command"], "Cancel");
        } else {
            assert!(!f.host.dispatch_low_level_key_for_foreground(escape, 42));
            assert!(!fifo_keyboard_event(&f.host, 0x1B, false, false, 21));
            assert!(fifo_keyboard_event(&f.host, 0x1B, true, false, 22));
        }
        f.host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn quick_paste_non_escape_cancellation_keeps_fifo_prepared() {
    let f = fifo_keyboard_fixture();
    f.host.arm_fifo_queue(vec![f.id.clone()]);
    let generation = f.host.inner.lock().fifo_generation;
    fifo_keyboard_begin(&f, 10);
    f.host.qp_cancel_session();
    f.host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(f.host.inner.lock().fifo_generation, generation);
    assert_eq!(f.host.fifo_status()["remainingIds"], json!([f.id]));
    assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn quick_paste_preview_side_uses_real_placement_and_negative_monitor_origin() {
    let f = fixture();
    for (cursor, left, expected) in [(100, 0.0, false), (1800, 0.0, true), (-120, -1920.0, true)] {
        let work = crate::edge_hide::Rect {
            left,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        f.host.inner.lock().qp.overlay_place =
            Some(crate::window_place::compute_quick_paste_overlay(
                2,
                crate::window_place::ShowWindowLocation::MouseRightBottom,
                Some((cursor, 300)),
                work,
                1.0,
                1.0,
            ));
        let detail = f.host.qp_state_detail();
        assert_eq!(detail["previewOnLeft"], expected);
        assert_eq!(detail["hoverCancel"], false);
    }
}

fn starting_escape_fixture(scope: Option<NativeSuperMenuKeyScope>) -> Fixture {
    let f = fixture();
    f.host.set_window_runtime_ready(true);
    {
        let mut inner = f.host.inner.lock();
        inner.qp.machine.reset_session();
        inner.qp.overlay_visible = false;
        inner.qp.hook_source = QuickPasteInputSource::Keyboard;
        inner.qp.pending_commands.clear();
        inner.super_menu_hwnd = 200;
        inner.window_visible = false;
        inner.has_host_fake_focus = false;
        inner.last_fg = 42;
        inner.last_non_host_fg = 42;
    }
    let menu = f.host.menu_create_session_ex("text", "", 200, 42);
    if let Some(scope) = scope {
        let generation = f.host.super_menu_state_detail()["generation"]
            .as_i64()
            .unwrap() as i32;
        let request = f
            .host
            .capture_super_menu_paste(menu["sessionId"].as_str().unwrap(), generation, 200)
            .unwrap();
        f.host
            .bind_native_super_menu_keyboard(&request, scope)
            .unwrap();
    }
    while f.host.take_chrome_task().is_some() {}
    f
}

fn starting_escape_key(vk: u32, down: bool) -> KeyEvent {
    KeyEvent {
        vk,
        down,
        alt: false,
        ctrl: false,
        shift: false,
        win: false,
        now_ms: 1,
    }
}

fn start_mouse_before_candidates(f: &Fixture) {
    f.host
        .inner
        .lock()
        .qp
        .mouse
        .configure(Some(QuickPasteMouseButton::X1));
    assert!(!f
        .host
        .dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(f.host.dispatch_low_level_wheel(120, 20, 20));
    assert_eq!(
        f.host.inner.lock().qp.machine.phase,
        QuickPastePhase::SessionStarting
    );
    assert!(!f.host.inner.lock().qp.overlay_visible);
}

fn start_keyboard_before_candidates(f: &Fixture) {
    let mut trigger = starting_escape_key(f.host.inner.lock().qp.machine.trigger_vk(), true);
    trigger.alt = true;
    assert!(f.host.dispatch_low_level_key_for_foreground(trigger, 42));
    assert_eq!(
        f.host.inner.lock().qp.machine.phase,
        QuickPastePhase::SessionStarting
    );
    assert!(!f.host.inner.lock().qp.overlay_visible);
}

fn assert_starting_escape_cancelled(f: &Fixture) {
    let inner = f.host.inner.lock();
    assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
    assert_eq!(inner.qp.cancel_generation, 1);
    assert!(inner.qp.pending_commands.is_empty(), "Escape retires work at the key event");
    assert!(!inner.qp.overlay_visible);
    assert!(
        inner.super_menu.visible,
        "QP Escape must not close the background menu"
    );
    drop(inner);
    assert!(f.host.take_native_super_menu_key().is_none());
    assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn quick_paste_starting_plain_escape_cancels_mouse_before_native_or_tauri_menu() {
    for scope in [
        Some(NativeSuperMenuKeyScope::WindowHub),
        Some(NativeSuperMenuKeyScope::ClipboardHub),
        None,
    ] {
        let f = starting_escape_fixture(scope);
        start_mouse_before_candidates(&f);
        // Model a serial worker that already drained BeginSession, but has not
        // opened it yet. Cancellation must retire this exact delayed command.
        let begin = f.host.inner.lock().qp.pending_commands.pop_front().unwrap();
        assert_eq!(begin.command, QuickPasteCommand::BeginSession);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, true), 42));
        assert_starting_escape_cancelled(&f);
        assert!(!f.host.qp_begin_session_from_press(Some(begin)));
        assert!(f
            .host
            .dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
        assert_starting_escape_cancelled(&f);
    }
}

#[test]
fn quick_paste_starting_plain_escape_cancels_keyboard_after_alt_release() {
    for scope in [
        Some(NativeSuperMenuKeyScope::WindowHub),
        Some(NativeSuperMenuKeyScope::ClipboardHub),
        None,
    ] {
        let f = starting_escape_fixture(scope);
        start_keyboard_before_candidates(&f);
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x12, false), 42));
        assert_eq!(
            f.host.inner.lock().qp.machine.phase,
            QuickPastePhase::SessionStarting
        );
        assert!(f.host.inner.lock().qp.machine.commit_when_session_opens);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, true), 42));
        assert_starting_escape_cancelled(&f);
    }
}

#[test]
fn quick_paste_starting_plain_escape_keeps_alt_held_cancel_route() {
    for scope in [
        Some(NativeSuperMenuKeyScope::WindowHub),
        Some(NativeSuperMenuKeyScope::ClipboardHub),
        None,
    ] {
        let f = starting_escape_fixture(scope);
        start_keyboard_before_candidates(&f);
        let mut escape = starting_escape_key(0x1B, true);
        escape.alt = true;
        assert!(f.host.dispatch_low_level_key_for_foreground(escape, 42));
        assert_starting_escape_cancelled(&f);
    }
}

#[test]
fn quick_paste_starting_plain_escape_drains_old_native_press_before_new_cancel() {
    for scope in [
        NativeSuperMenuKeyScope::WindowHub,
        NativeSuperMenuKeyScope::ClipboardHub,
    ] {
        let f = starting_escape_fixture(Some(scope));
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, true), 42));
        let down = f.host.take_native_super_menu_key().unwrap();
        assert_eq!(down.key(), "Escape");
        assert!(down.down());
        start_mouse_before_candidates(&f);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, true), 42));
        assert!(
            f.host.take_native_super_menu_key().is_none(),
            "native Escape executes once per press"
        );
        assert_eq!(
            f.host.inner.lock().qp.machine.phase,
            QuickPastePhase::SessionStarting
        );
        assert_eq!(f.host.inner.lock().qp.cancel_generation, 0);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, false), 42));
        let up = f.host.take_native_super_menu_key().unwrap();
        assert_eq!(up.key(), "Escape");
        assert!(!up.down());
        assert_eq!(
            f.host.inner.lock().qp.machine.phase,
            QuickPastePhase::SessionStarting
        );
        assert_eq!(f.host.inner.lock().qp.cancel_generation, 0);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, true), 42));
        assert_starting_escape_cancelled(&f);
    }
}

#[test]
fn quick_paste_starting_plain_escape_preserves_idle_menu_routes() {
    for scope in [
        Some(NativeSuperMenuKeyScope::WindowHub),
        Some(NativeSuperMenuKeyScope::ClipboardHub),
        None,
    ] {
        let native = scope.is_some();
        let f = starting_escape_fixture(scope);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, true), 42));
        if native {
            let down = f.host.take_native_super_menu_key().unwrap();
            assert_eq!(down.key(), "Escape");
            assert!(down.down());
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, false), 42));
            assert!(!f.host.take_native_super_menu_key().unwrap().down());
            assert!(f.host.inner.lock().super_menu.visible);
        } else {
            assert!(f.host.take_native_super_menu_key().is_none());
            assert!(!f.host.inner.lock().super_menu.visible);
            assert!(!f
                .host
                .dispatch_low_level_key_for_foreground(starting_escape_key(0x1B, false), 42));
        }
        assert_eq!(f.host.inner.lock().qp.machine.phase, QuickPastePhase::Idle);
        assert_eq!(f.host.inner.lock().qp.cancel_generation, 0);
        assert!(f.host.inner.lock().qp.pending_commands.is_empty());
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}
