use super::*;
use std::sync::{atomic::AtomicUsize, mpsc};

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    host.inner.lock().config.enable_vv_tab_text_instruction = true;
    host.window_hide();
    (dir, host)
}

fn key(vk: u32) -> KeyEvent {
    KeyEvent {
        vk,
        down: true,
        alt: false,
        ctrl: false,
        shift: false,
        win: false,
        now_ms: 0,
    }
}

fn trigger(host: &HostState, last: u32) {
    assert!(!host.dispatch_low_level_key(key(0x56)));
    assert!(!host.dispatch_low_level_key(key(0x56)));
    assert!(host.dispatch_low_level_key(key(last)));
    assert_eq!(
        host.inner
            .lock()
            .vv_tab_pending
            .as_ref()
            .unwrap()
            .backspaces,
        2
    );
}

#[test]
fn vv_tab_worker_failure_keeps_window_hidden_and_can_retry() {
    let (_dir, host) = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    host.set_input_runner(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Err("receiver changed".into())
    });
    trigger(&host, 0x56);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "hook performed input");
    host.poll_text_instruction();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!host.inner.lock().window_visible);
    assert!(host
        .drain_push_events()
        .iter()
        .any(|(name, value)| name == "toast" && value["message"] == "receiver changed"));
    assert!(host.clipboard_operation.try_lock().is_some());
    host.set_input_runner(|job| job());
    trigger(&host, 0x09);
    host.poll_text_instruction();
    assert!(host.inner.lock().window_visible);
}

#[test]
fn vv_tab_pending_request_is_cancelled_by_new_input_expiry_or_disabled_config() {
    let (_dir, host) = fixture();
    host.set_input_runner(|_| panic!("cancelled instruction sent input"));
    for cancellation in 0..4 {
        host.inner.lock().config.enable_vv_tab_text_instruction = true;
        trigger(&host, 0x09);
        match cancellation {
            0 => {
                assert!(!host.dispatch_low_level_key(key(0x41)));
            }
            1 => host.notify_outside_click_at(0, 0),
            2 => {
                host.inner.lock().vv_tab_pending.as_mut().unwrap().queued_at =
                    Instant::now() - Duration::from_secs(1)
            }
            _ => host
                .apply_clip_config(
                    &json!({"enableVvTabTextInstruction": false}),
                    ClipConfigApply::Settings,
                )
                .unwrap(),
        }
        host.poll_text_instruction();
        assert!(!host.inner.lock().window_visible);
        assert!(host.inner.lock().vv_tab_pending.is_none());
    }
    // Disabling and re-enabling must also discard a half-typed sequence.
    host.apply_clip_config(
        &json!({"enableVvTabTextInstruction": true}),
        ClipConfigApply::Settings,
    )
    .unwrap();
    assert!(!host.dispatch_low_level_key(key(0x56)));
    assert!(!host.dispatch_low_level_key(key(0x56)));
    host.apply_clip_config(
        &json!({"enableVvTabTextInstruction": false}),
        ClipConfigApply::Settings,
    )
    .unwrap();
    host.apply_clip_config(
        &json!({"enableVvTabTextInstruction": true}),
        ClipConfigApply::Settings,
    )
    .unwrap();
    assert!(!host.dispatch_low_level_key(key(0x09)));
}

#[test]
fn vv_tab_slow_input_does_not_block_hook_or_host_state() {
    let (_dir, host) = fixture();
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let wait = Mutex::new(wait);
    host.set_input_runner(move |job| {
        entered.send(()).unwrap();
        wait.lock().recv_timeout(Duration::from_secs(5)).unwrap();
        job()
    });
    trigger(&host, 0x56);
    let worker = host.clone();
    let worker = std::thread::spawn(move || worker.poll_text_instruction());
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let responding = host.clone();
    let (done, response) = mpsc::channel();
    let events = std::thread::spawn(move || {
        let swallowed = responding.dispatch_low_level_key(key(0x41));
        done.send((swallowed, responding.health())).unwrap();
    });
    let responsive = response.recv_timeout(Duration::from_secs(3));
    release.send(()).unwrap();
    worker.join().unwrap();
    events.join().unwrap();
    let (swallowed, health) = responsive.expect("input worker blocked hook/state access");
    assert!(!swallowed);
    assert_eq!(health["ok"], true);
    assert!(
        !host.inner.lock().window_visible,
        "typing during a queued input must cancel it"
    );
}


#[test]
fn vv_tab_regression_search_letters_never_queue_external_backspaces() {
    let (_dir, host) = fixture();
    host.set_clipboard_web_view_ready(true);
    {
        let mut inner = host.inner.lock();
        inner.window_visible = true;
    }
    // fixture() hides the window and releases keyboard ownership. Reopening
    // starts the same complete session as the production show path.
    host.begin_no_focus_session_if_enabled();
    assert_eq!(host.clipboard_keyboard_mode(), ClipboardKeyboardMode::Full);
    while host.take_chrome_command().is_some() {}
    for _ in 0..3 {
        assert!(host.dispatch_low_level_key_for_foreground(key(0x56), 101));
    }
    assert!(host.inner.lock().vv_tab_pending.is_none(), "host search queued external backspaces");
    for _ in 0..3 {
        assert_eq!(host.take_chrome_command(), Some(ChromeCommand::ClipboardAppendSearch('v')));
    }
    assert!(host.take_chrome_command().is_none());
}

#[test]
fn vv_tab_regression_prefix_stays_in_its_foreground_window() {
    let (_dir, host) = fixture();
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x56), 101));
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x56), 101));
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x56), 202), "prefix crossed foreground windows");
    assert!(host.inner.lock().vv_tab_pending.is_none());
    // The first V in the new window still starts a fresh, valid sequence.
    assert!(!host.dispatch_low_level_key_for_foreground(key(0x56), 202));
    assert!(host.dispatch_low_level_key_for_foreground(key(0x09), 202));
    assert_eq!(host.inner.lock().vv_tab_pending.as_ref().unwrap().hwnd, 202);
}

#[test]
fn vv_tab_regression_click_discards_an_unfinished_prefix() {
    let (_dir, host) = fixture();
    assert!(!host.dispatch_low_level_key(key(0x56)));
    assert!(!host.dispatch_low_level_key(key(0x56)));
    host.notify_outside_click_at(0, 0);
    assert!(!host.dispatch_low_level_key(key(0x09)), "click retained a stale text prefix");
    assert!(host.inner.lock().vv_tab_pending.is_none());
}


#[test]
fn vv_tab_right_click_and_suspended_input_discard_a_prefix() {
    for reason in 0..3 {
        let (_dir, host) = fixture();
        assert!(!host.dispatch_low_level_key(key(0x56)));
        assert!(!host.dispatch_low_level_key(key(0x56)));
        match reason {
            0 => host.notify_panel_outside_click_at(0, 0),
            1 => {
                host.set_window_runtime_ready(false);
                assert!(!host.dispatch_low_level_key(key(0x56)));
                host.set_window_runtime_ready(true);
            }
            _ => {
                host.inner.lock().ole_drag_active = true;
                assert!(!host.dispatch_low_level_key(key(0x56)));
                host.inner.lock().ole_drag_active = false;
            }
        }
        assert!(!host.dispatch_low_level_key(key(0x09)));
        assert!(host.inner.lock().vv_tab_pending.is_none());
    }
}
