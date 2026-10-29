use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture(trigger: &str) -> (tempfile::TempDir, HostState, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.add_text(&json!({ "text": "one" })).unwrap();
    host.add_text(&json!({ "text": "two" })).unwrap();
    host.settings_form_update(&json!({
        "configId": "quickPaste",
        "values": { "MouseWheelTrigger": trigger, "RememberSelection": false }
    })).unwrap();
    while host.take_chrome_command().is_some() {}
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let inner = host.inner.clone();
    host.set_input_runner(move |job| {
        assert!(inner.try_lock().is_some(), "input must run outside the host lock");
        count.fetch_add(1, Ordering::SeqCst);
        job()
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let count = writes.clone();
    let inner = host.inner.clone();
    host.set_clipboard_ui_runner(move |job| {
        assert!(inner.try_lock().is_some(), "clipboard write must run outside the host lock");
        count.fetch_add(1, Ordering::SeqCst);
        job()
    });
    (root, host, calls, writes)
}

fn start_mouse(host: &HostState, button: QuickPasteMouseButton) {
    assert!(!host.dispatch_low_level_side_button(button, true));
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
}

fn assert_no_paste_or_replay(host: &HostState, calls: &AtomicUsize, writes: &AtomicUsize) {
    assert_eq!(calls.load(Ordering::SeqCst), 0, "right cancellation must not inject paste or replay clicks");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(host.store().lock().list_clips().unwrap().iter().all(|item| item.usage_count == 0));
    let inner = host.inner.lock();
    assert!(!inner.qp.overlay_visible);
    assert!(!inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::ShowQuickPaste)));
}

#[test]
fn mouse_cancel_before_load_retires_moves_and_side_release_then_right_release() {
    for (trigger, button) in [("x1", QuickPasteMouseButton::X1), ("x2", QuickPasteMouseButton::X2)] {
        let (_root, host, calls, writes) = fixture(trigger);
        start_mouse(&host, button);
        assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
        assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
        assert!(host.dispatch_low_level_quick_paste_right_button(true));
        assert!(host.inner.lock().qp.mouse.is_draining());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the right-button hook must not perform input");
        host.poll_quick_paste_with_modifiers(|| false);
        assert_no_paste_or_replay(&host, &calls, &writes);
        assert!(!host.dispatch_low_level_wheel(-120, -20_000, -20_000));
        assert!(host.dispatch_low_level_side_button(button, false));
        assert!(host.dispatch_low_level_quick_paste_right_button(false), "owned right Up must stay swallowed after side Up");
        host.poll_quick_paste_with_modifiers(|| false);
        assert_no_paste_or_replay(&host, &calls, &writes);
        assert!(!host.inner.lock().qp.mouse.is_held());
        assert!(!host.dispatch_low_level_quick_paste_right_button(false), "duplicate Up belongs to ordinary input");

        // Side Up can enqueue Commit before the worker has loaded candidates.
        // A subsequent right press must still retire that pending commit.
        start_mouse(&host, button);
        assert!(host.dispatch_low_level_side_button(button, false));
        assert!(host.dispatch_low_level_quick_paste_right_button(true));
        assert!(host.dispatch_low_level_quick_paste_right_button(false));
        host.poll_quick_paste_with_modifiers(|| false);
        assert_no_paste_or_replay(&host, &calls, &writes);

        start_mouse(&host, button);
        assert!(host.dispatch_low_level_side_button(button, false));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the fresh gesture must also wait for its worker");
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(writes.load(Ordering::SeqCst), 1, "releasing the canceled buttons must permit a fresh session");
        assert_eq!(host.store().lock().list_clips().unwrap().iter().map(|item| item.usage_count).sum::<i32>(), 1);
    }
}

#[test]
fn mouse_cancel_visible_session_drains_right_release_then_side_release() {
    let (_root, host, calls, writes) = fixture("x2");
    start_mouse(&host, QuickPasteMouseButton::X2);
    host.poll_quick_paste_with_modifiers(|| false);
    host.qp_web_action(&json!({ "action": "placementReady" }));
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionActive);
    assert!(host.inner.lock().qp.overlay_visible);
    while host.take_chrome_command().is_some() {}
    assert!(host.dispatch_low_level_wheel(-120, -20_000, -20_000));
    assert!(host.dispatch_low_level_quick_paste_right_button(true));
    assert!(host.dispatch_low_level_quick_paste_right_button(false));
    assert!(host.inner.lock().qp.mouse.is_draining(), "right Up must not release the owned side-button drain");
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    assert!(host.dispatch_low_level_side_button(QuickPasteMouseButton::X2, false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_no_paste_or_replay(&host, &calls, &writes);
    assert!(!host.inner.lock().qp.mouse.is_held());
    host.poll_quick_paste_with_modifiers(|| false);
    assert_no_paste_or_replay(&host, &calls, &writes);
}

#[test]
fn mouse_cancel_plain_right_click_and_unengaged_side_click_pass_through() {
    let (_root, host, calls, writes) = fixture("none");
    assert!(!host.dispatch_low_level_quick_paste_right_button(true));
    assert!(!host.dispatch_low_level_quick_paste_right_button(false));
    host.settings_form_update(&json!({
        "configId": "quickPaste", "values": { "MouseWheelTrigger": "x1" }
    })).unwrap();
    assert!(!host.dispatch_low_level_quick_paste_right_button(true));
    assert!(!host.dispatch_low_level_quick_paste_right_button(false));
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, true));
    assert!(!host.dispatch_low_level_quick_paste_right_button(true), "a held side button without wheel is not a quick-paste session");
    assert!(!host.dispatch_low_level_quick_paste_right_button(false));
    assert!(!host.dispatch_low_level_side_button(QuickPasteMouseButton::X1, false));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the ordinary side click already passed; nothing is replayed");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[test]
fn mouse_cancel_right_button_does_not_cancel_keyboard_opening_or_active_session() {
    let (_root, host, calls, writes) = fixture("x1");
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0xC0, down: true, alt: true, ctrl: false, shift: false, win: false, now_ms: 1,
    }));
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
    let cancel_generation = host.inner.lock().qp.cancel_generation;
    assert!(!host.dispatch_low_level_quick_paste_right_button(true));
    assert!(!host.dispatch_low_level_quick_paste_right_button(false));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionActive);
    assert!(!host.dispatch_low_level_quick_paste_right_button(true));
    assert!(!host.dispatch_low_level_quick_paste_right_button(false));
    assert_eq!(host.inner.lock().qp.cancel_generation, cancel_generation);
    assert!(host.inner.lock().qp.overlay_visible);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(host.dispatch_low_level_key(KeyEvent {
        vk: 0xC0, down: false, alt: true, ctrl: false, shift: false, win: false, now_ms: 2,
    }));
    assert!(!host.dispatch_low_level_key(KeyEvent {
        vk: 0x12, down: false, alt: false, ctrl: false, shift: false, win: false, now_ms: 3,
    }));
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 1, "the original keyboard release must still commit once");
    assert!(!host.inner.lock().qp.overlay_visible);
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
