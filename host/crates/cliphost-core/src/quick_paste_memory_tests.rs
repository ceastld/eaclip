use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    for text in ["memory first", "memory second", "memory third"] {
        host.add_text(&json!({ "text": text })).unwrap();
    }
    {
        let mut inner = host.inner.lock();
        inner.qp_config.remember_selection = true;
        inner.qp_config.selection_memory_seconds = 10;
    }
    host.set_input_runner(|job| job());
    host.set_clipboard_ui_runner(|job| job());
    (root, host)
}

fn open(host: &HostState) -> usize {
    host.qp_debug_begin();
    let inner = host.inner.lock();
    assert!(inner.qp.overlay_visible);
    assert_eq!(inner.qp.items.len(), 3);
    inner.qp.active_index
}

fn usage(host: &HostState) -> i32 {
    host.store().lock().list_clips().unwrap().iter().map(|item| item.usage_count).sum()
}

fn mouse_open(host: &HostState) -> usize {
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    host.poll_quick_paste_with_modifiers(|| false);
    let inner = host.inner.lock();
    assert_eq!(inner.qp.machine.phase, QuickPastePhase::SessionActive);
    assert!(inner.qp.overlay_visible);
    inner.qp.active_index
}

fn mouse_cancel_and_release(host: &HostState) {
    assert!(host.dispatch_low_level_quick_paste_right_button(true));
    assert!(host.dispatch_low_level_quick_paste_right_button(false));
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert!(!host.inner.lock().qp.overlay_visible);
}

#[test]
fn quick_paste_memory_success_and_escape_cancel_resume_expected_item() {
    let (_root, host) = fixture();
    assert_eq!(open(&host), 0);
    host.qp_commit(false);
    assert_eq!(usage(&host), 1);
    assert_eq!(open(&host), 1);
    assert_eq!(host.qp_handle_key(KeyEvent {
        vk: crate::quick_paste::VK_ESCAPE, down: true,
        alt: false, ctrl: false, shift: false, win: false, now_ms: 1,
    })["verdict"], "Swallow");
    assert_eq!(usage(&host), 1, "Escape must not count the selected second item as pasted");
    assert_eq!(open(&host), 1, "canceling the second item must reopen that same item");
    host.qp_commit(false);
    assert_eq!(usage(&host), 2);
    assert_eq!(open(&host), 2, "only the successful second paste advances to the third item");
    host.qp_debug_end();
}

#[test]
fn quick_paste_memory_side_button_right_cancel_retries_same_item() {
    let (_root, host) = fixture();
    host.settings_form_update(&json!({
        "configId": "quickPaste", "values": { "MouseWheelTrigger": "x1" }
    })).unwrap();
    let inputs = Arc::new(AtomicUsize::new(0));
    let observed = inputs.clone();
    host.set_input_runner(move |job| { observed.fetch_add(1, Ordering::SeqCst); job() });
    assert_eq!(mouse_open(&host), 0);
    mouse_cancel_and_release(&host);
    assert_eq!(usage(&host), 0, "right cancellation and side release must not paste or replay");
    assert_eq!(inputs.load(Ordering::SeqCst), 0);
    assert_eq!(mouse_open(&host), 0, "canceling the first item must reopen the first item");
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(usage(&host), 1);
    let committed_inputs = inputs.load(Ordering::SeqCst);
    assert_eq!(mouse_open(&host), 1);
    mouse_cancel_and_release(&host);
    assert_eq!(usage(&host), 1, "canceling the second item must leave usage unchanged");
    assert_eq!(inputs.load(Ordering::SeqCst), committed_inputs, "canceled side release must not inject another click or paste");
    assert_eq!(mouse_open(&host), 1);
    mouse_cancel_and_release(&host);
}

#[test]
fn quick_paste_memory_failed_output_retries_but_completed_paste_advances_before_enter() {
    for failure in ["copy", "input", "enter"] {
        let (_root, host) = fixture();
        assert_eq!(open(&host), 0);
        host.qp_move(1);
        assert_eq!(host.inner.lock().qp.active_index, 1);
        if failure == "copy" {
            host.set_clipboard_ui_runner(|_| Err("test clipboard unavailable".into()));
        } else {
            let calls = Arc::new(AtomicUsize::new(0));
            let fail_at = if failure == "input" { 1 } else { 2 };
            host.set_input_runner(move |job| {
                if calls.fetch_add(1, Ordering::SeqCst) == fail_at {
                    Err("test input unavailable".into())
                } else { job() }
            });
        }
        host.qp_commit(true);
        assert_eq!(usage(&host), if failure == "enter" { 1 } else { 0 }, "{failure}");
        host.set_input_runner(|job| job());
        host.set_clipboard_ui_runner(|job| job());
        assert_eq!(open(&host), if failure == "enter" { 2 } else { 1 },
            "copy/input failure retries the current item; failed Enter cannot undo a completed paste: {failure}");
        host.qp_debug_end();
    }
}

#[test]
fn quick_paste_memory_loading_cancel_preserves_memory_and_expiry_resets_selection() {
    let (_root, host) = fixture();
    assert_eq!(open(&host), 0);
    host.qp_commit(false);
    let before = host.inner.lock().qp.remembered.as_ref()
        .map(|memory| (memory.next_index, memory.at, memory.session_revision));
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0xC0, down: true, alt: true, ctrl: false, shift: false, win: false, now_ms: 10,
    }));
    assert!(host.inner.lock().qp.items.is_empty(), "the new candidate load has not run yet");
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: crate::quick_paste::VK_ESCAPE, down: true,
        alt: true, ctrl: false, shift: false, win: false, now_ms: 11,
    }));
    host.poll_quick_paste_with_modifiers(|| false);
    let after = host.inner.lock().qp.remembered.as_ref()
        .map(|memory| (memory.next_index, memory.at, memory.session_revision));
    assert_eq!(after, before, "a canceled candidate load must neither erase nor refresh prior memory");
    host.dispatch_low_level_key(KeyEvent {
        vk: 0xC0, down: false, alt: true, ctrl: false, shift: false, win: false, now_ms: 12,
    });
    host.dispatch_low_level_key(KeyEvent {
        vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 13,
    });
    assert_eq!(open(&host), 1);
    host.qp_debug_end();
    host.inner.lock().qp.remembered.as_mut().unwrap().at = Instant::now() - Duration::from_secs(11);
    assert_eq!(open(&host), 0, "expired ten-second memory must restart at the first item");
    host.qp_debug_end();
}

#[test]
fn quick_paste_memory_late_commit_cannot_replace_a_newer_session_retry() {
    let (_root, host) = fixture();
    assert_eq!(open(&host), 0);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let newer = host.clone();
    host.set_input_runner(move |job| {
        let call = seen.fetch_add(1, Ordering::SeqCst);
        let result = job();
        if call == 1 && result.is_ok() {
            // The paste was sent, but its success bookkeeping has not returned.
            // A later canceled selection must own subsequent session memory.
            newer.qp_debug_begin();
            newer.qp_move(2);
            newer.qp_debug_end();
        }
        result
    });
    host.qp_commit(false);
    // Release the runner's HostState capture before the fixture is destroyed.
    host.set_input_runner(|job| job());
    assert_eq!(calls.load(Ordering::SeqCst), 2, "activation and one paste must finish");
    assert_eq!(usage(&host), 1, "the completed old paste still counts as a successful paste");
    assert_eq!(open(&host), 2, "the old completion must not advance over the newer canceled third item");
    host.qp_debug_end();
}
