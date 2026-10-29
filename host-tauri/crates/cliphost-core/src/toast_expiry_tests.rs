use super::*;

fn fixture() -> (tempfile::TempDir, HostState, i32) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().join("data")), 28965).unwrap();
    host.show_host_toast("first", "info", None, Some(2500), None);
    while host.take_chrome_command().is_some() {}
    let generation = {
        let mut inner = host.inner.lock();
        inner.toast.idle_deadline = Some(Instant::now() - Duration::from_secs(1));
        inner.toast.generation
    };
    (root, host, generation)
}

#[test]
fn toast_expiry_new_message_survives_previous_deadline() {
    let (_root, host, old_generation) = fixture();
    host.show_host_toast("second", "info", None, Some(2500), None);
    assert!(!host.toast_collapse_expired(old_generation));
    let inner = host.inner.lock();
    assert!(inner.toast.visible);
    assert_eq!(inner.toast.pending_push.as_ref().unwrap()["message"], "second");
    assert!(!inner.chrome.contains(&ChromeCommand::HideToast));
}

#[test]
fn toast_expiry_hover_after_due_check_cancels_collapse() {
    let (_root, host, generation) = fixture();
    host.toast_web_action(&json!({"action": "hover"}));
    assert!(!host.toast_collapse_expired(generation));
    let inner = host.inner.lock();
    assert!(inner.toast.visible && inner.toast.hovered);
    assert!(inner.toast.idle_deadline.is_none());
    assert!(inner.chrome.is_empty());
}

#[test]
fn toast_expiry_resumed_timer_survives_previous_deadline() {
    let (_root, host, generation) = fixture();
    host.toast_web_action(&json!({"action": "hover"}));
    host.toast_web_action(&json!({"action": "unhover"}));
    assert_eq!(host.inner.lock().toast.generation, generation);
    assert!(!host.toast_collapse_expired(generation));
    let inner = host.inner.lock();
    assert!(inner.toast.visible && !inner.toast.hovered);
    assert!(inner.toast.idle_deadline.unwrap() > Instant::now());
    assert!(inner.chrome.is_empty());
}

#[test]
fn toast_expiry_already_hidden_does_not_queue_another_hide() {
    let (_root, host, generation) = fixture();
    host.dismiss_native_window("copy-toast");
    while host.take_chrome_command().is_some() {}
    assert!(!host.toast_collapse_expired(generation));
    assert!(host.take_chrome_command().is_none());
}

#[test]
fn toast_expiry_current_deadline_hides_once() {
    let (_root, host, generation) = fixture();
    assert!(host.toast_collapse_expired(generation));
    let inner = host.inner.lock();
    assert!(!inner.toast.visible && !inner.toast.hovered);
    assert!(inner.toast.idle_deadline.is_none());
    assert_eq!(inner.toast.last_action.as_deref(), Some("idle-fallback"));
    assert_eq!(inner.chrome.iter().copied().collect::<Vec<_>>(), vec![ChromeCommand::HideToast]);
}

fn assert_repeated_show_preserves_hover(with_message: bool) {
    let (_root, host, old_generation) = fixture();
    host.toast_web_action(&json!({"action": "hover"}));
    if with_message {
        host.show_host_toast("second", "info", None, Some(2500), None);
    } else {
        host.toast_show_with_duration(Some(2500));
    }
    {
        let inner = host.inner.lock();
        assert!(inner.toast.visible && inner.toast.hovered);
        assert!(inner.toast.idle_deadline.is_none(), "hover must keep fallback paused");
        assert_ne!(inner.toast.generation, old_generation);
    }
    let deferred = host.toast_web_action(&json!({"action": "idle"}));
    assert_eq!(deferred["deferred"], true);
    host.toast_web_action(&json!({"action": "unhover"}));
    {
        let inner = host.inner.lock();
        assert!(inner.toast.visible && !inner.toast.hovered);
        assert!(inner.toast.idle_deadline.unwrap() > Instant::now());
    }
    host.dismiss_native_window("copy-toast");
    host.toast_web_action(&json!({"action": "hover"}));
    host.show_host_toast("fresh session", "info", None, Some(2500), None);
    let inner = host.inner.lock();
    assert!(!inner.toast.hovered, "a hidden session cannot lend its old hover to a new one");
    assert!(inner.toast.idle_deadline.is_some());
}

#[test]
fn toast_expiry_new_message_preserves_existing_hover_pause() {
    assert_repeated_show_preserves_hover(true);
}

#[test]
fn toast_expiry_repeat_show_preserves_existing_hover_pause() {
    assert_repeated_show_preserves_hover(false);
}
