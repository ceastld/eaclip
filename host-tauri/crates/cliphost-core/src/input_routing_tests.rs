//! These fixtures enter the same guarded router as the native callbacks. No OS
//! hook, desktop input, foreground activation or system clipboard is used.
use super::*;
use std::sync::atomic::AtomicUsize;

fn fixture() -> (tempfile::TempDir, HostState, Arc<AtomicUsize>) {
    crate::clipboard::force_headless(true);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("input-routing-")
        .tempdir_in(root)
        .unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(directory.path().to_path_buf()),
        28965,
    )
    .unwrap();
    host.set_window_runtime_ready(true);
    host.add_text(&json!({ "text": "owned input fixture" }))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    host.set_input_runner(move |_| {
        counted.fetch_add(1, Ordering::AcqRel);
        Err("fixture must not inject input".into())
    });
    while host.take_chrome_command().is_some() {}
    (directory, host, calls)
}

fn key(vk: u32, down: bool, alt: bool) -> KeyEvent {
    KeyEvent {
        vk,
        down,
        alt,
        ctrl: false,
        shift: false,
        win: false,
        now_ms: 1,
    }
}

#[test]
fn input_busy_side_release_retires_ownership_before_next_plain_click() {
    for button_data in [1 << 16, 2 << 16] {
        let (_directory, host, calls) = fixture();
        host.settings_form_update(&json!({
            "configId": "quickPaste",
            "values": { "MouseWheelTrigger": if button_data == 1 << 16 { "x1" } else { "x2" } }
        })).unwrap();
        let wheel_data = ((-120_i16 as u16) as u32) << 16;
        assert!(!host.try_dispatch_input_mouse(0x020B, -20_000, -20_000, button_data));
        assert!(host.try_dispatch_input_mouse(0x020A, -20_000, -20_000, wheel_data));
        let inner = host.inner.lock();
        assert!(!host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, button_data),
            "a busy callback must return immediately without taking host state");
        drop(inner);
        assert!(host.input_gap_pending());

        assert!(!host.try_dispatch_input_mouse(0x020B, -20_000, -20_000, button_data));
        assert!(!host.try_dispatch_input_mouse(0x020C, -20_000, -20_000, button_data),
            "the already passed release must not consume the next ordinary click");
        host.poll_quick_paste_with_modifiers(|| false);
        assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::Idle);
        assert!(!host.inner.lock().qp.mouse.is_held());
        assert_eq!(calls.load(Ordering::Acquire), 0, "the missed release must not become a delayed paste");
    }
}

#[cfg(windows)]
#[test]
fn input_busy_chrome_registry_passes_release_and_cancels_before_backup_poll() {
    let _input_globals = crate::hooks::input_globals_test::exclusive();
    let (_directory, host, calls) = fixture();
    assert!(host.try_dispatch_input_key_for_foreground(key(0xC0, true, true), 0));
    let (entered, ready) = mpsc::channel();
    let holding = thread::spawn(move || {
        crate::window_chrome::with_registry_held_for_test(|| {
            entered.send(()).unwrap();
            thread::sleep(Duration::from_millis(400));
        });
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    assert!(!host.try_dispatch_input_key_for_foreground(key(0x12, false, false), 0));
    assert!(!host.try_dispatch_input_mouse(0x020A, 0, 0, 120 << 16));
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(host.input_gap_pending());
    holding.join().unwrap();
    host.poll_quick_paste_with_modifiers(|| false);
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::Idle);
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(host.inner.lock().qp.pending_commands.is_empty());
}

#[cfg(windows)]
#[test]
fn input_busy_chrome_snapshot_does_not_relock_after_admission() {
    let _input_globals = crate::hooks::input_globals_test::exclusive();
    let _snapshot = crate::window_chrome::try_input_scope([0, 0, 0]).unwrap();
    let (entered, ready) = mpsc::channel();
    let holding = thread::spawn(move || {
        crate::window_chrome::with_registry_held_for_test(|| {
            entered.send(()).unwrap();
            thread::sleep(Duration::from_millis(400));
        });
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    assert!(!crate::window_chrome::is_in_move_size(0));
    assert_eq!(crate::window_chrome::is_active(0), None);
    assert!(started.elapsed() < Duration::from_millis(200));
    holding.join().unwrap();
}

#[test]
fn input_busy_retires_dequeued_focus_even_after_gap_reconciliation() {
    let (_directory, host, _calls) = fixture();
    host.request_show();
    host.set_clipboard_web_view_ready(true);
    while host.take_chrome_task().is_some() {}
    host.enqueue_chrome(ChromeCommand::ClipboardKeyboardFocus);
    let accepted = host.take_chrome_task().unwrap();
    assert!(host.prepare_chrome_task(accepted));
    let owner = host.inner.lock();
    assert!(!host.try_dispatch_input_key_for_foreground(key(0x46, false, false), 0));
    drop(owner);
    host.poll_keyboard_work();
    assert!(!host.input_gap_pending());
    assert!(
        !host.prepare_chrome_task(accepted),
        "old Ctrl+F focus revived after clearing the gap"
    );
}

#[test]
fn input_busy_host_for_eleven_seconds_passes_events_and_retires_missed_releases() {
    let (_directory, host, calls) = fixture();
    assert!(host.try_dispatch_input_key_for_foreground(key(0xC0, true, true), 0));
    assert_eq!(
        host.inner.lock().qp.machine.phase,
        QuickPastePhase::SessionStarting
    );
    let blocked = host.clone();
    let (entered, ready) = mpsc::channel();
    let holding = thread::spawn(move || {
        let mut inner = blocked.inner.lock();
        inner.native_context_menu_escape_down = true;
        inner.qp.mouse_cancel_right_down = true;
        inner.caps_guide_until_keyup = true;
        entered.send(()).unwrap();
        thread::sleep(Duration::from_secs(11));
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    for _ in 0..100 {
        for event in [
            key(0xC0, false, true),
            key(0x12, false, false),
            key(0x14, false, false),
            key(0x1B, false, false),
        ] {
            assert!(!host.try_dispatch_input_key_for_foreground(event, 0));
        }
        for message in [0x0201, 0x0202, 0x0205, 0x020C, 0x020A] {
            assert!(!host.try_dispatch_input_mouse(message, 0, 0, 120 << 16));
        }
        host.try_notify_input_foreground(123);
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "busy callbacks waited for the 11s owner"
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(host.input_gap_pending());
    holding.join().unwrap();
    host.poll_keyboard_work();
    {
        let inner = host.inner.lock();
        assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
        assert!(inner.qp.pending_commands.is_empty());
        assert!(!inner.native_context_menu_escape_down);
        assert!(!inner.qp.mouse_cancel_right_down);
        assert!(!inner.caps_guide_until_keyup);
        assert!(!inner.qp.mouse.is_held());
        assert!(inner.keyboard_keys_down.iter().all(|down| !down));
    }
    assert_eq!(
        calls.load(Ordering::Acquire),
        0,
        "passed-through releases became delayed paste/menu input"
    );
    assert!(
        host.try_dispatch_input_key_for_foreground(key(0xC0, true, true), 0),
        "next chord did not recover"
    );
}

#[test]
fn input_busy_rejects_an_already_drained_ui_task_after_waiting_for_the_host() {
    let (_directory, host, _calls) = fixture();
    host.request_show();
    host.set_clipboard_web_view_ready(true);
    let task = {
        let inner = host.inner.lock();
        ChromeTask {
            command: ChromeCommand::ClipboardNavKey {
                key: "Enter",
                shift: false,
            },
            presentation_epoch: None,
            input_epoch: Some(inner.chrome_input_epochs[0]),
            escape_session: inner.clipboard_escape_session,
        }
    };
    let owner = host.inner.lock();
    let checking = host.clone();
    let (started, ready) = mpsc::channel();
    let validator = thread::spawn(move || {
        started.send(()).unwrap();
        checking.prepare_chrome_task(task)
    });
    ready.recv().unwrap();
    thread::sleep(Duration::from_millis(50));
    assert!(!host.try_dispatch_input_key_for_foreground(key(0x0D, false, false), 0));
    drop(owner);
    assert!(
        !validator.join().unwrap(),
        "an input guard missed the gap raised while it waited for state"
    );
}

#[test]
fn input_busy_menu_registry_retires_visible_state_then_cleans_registry_off_callback() {
    let (_directory, host, _calls) = fixture();
    let created = host
        .menus
        .create_fixture_session(&crate::menu_api::MenuBuildContext::default(), Vec::new());
    let session = created["sessionId"].as_str().unwrap().to_owned();
    {
        let mut inner = host.inner.lock();
        inner.super_menu.visible = true;
        inner.super_menu.session_id = Some(session.clone());
    }
    let menus = host.menus.clone();
    let (entered, ready) = mpsc::channel();
    let holding = thread::spawn(move || {
        menus.with_sessions_held_for_test(|| {
            entered.send(()).unwrap();
            thread::sleep(Duration::from_millis(400));
        })
    });
    ready.recv().unwrap();
    let started = Instant::now();
    assert!(host.try_dispatch_input_key_for_foreground(key(0x1B, true, false), 0));
    assert!(started.elapsed() < Duration::from_millis(200));
    {
        let inner = host.inner.lock();
        assert!(!inner.super_menu.visible);
        assert_eq!(inner.input_menu_cleanup, vec![session]);
    }
    holding.join().unwrap();
    host.poll_input_cleanup();
    assert!(host.menus.list()["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn input_busy_capture_event_queue_does_not_delay_accepted_navigation() {
    let (_directory, host, _calls) = fixture();
    host.request_show();
    host.set_clipboard_web_view_ready(true);
    let capture = {
        let mut inner = host.inner.lock();
        inner.has_host_fake_focus = true;
        inner.no_focus_session_active = true;
        inner.capture.clone()
    };
    while host.take_chrome_command().is_some() {}
    let (entered, ready) = mpsc::channel();
    let holding = thread::spawn(move || {
        capture.with_events_held_for_test(|| {
            entered.send(()).unwrap();
            thread::sleep(Duration::from_millis(400));
        })
    });
    ready.recv().unwrap();
    let started = Instant::now();
    assert!(host.try_dispatch_input_key_for_foreground(key(0x31, true, false), 0));
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(matches!(
        host.take_chrome_command(),
        Some(ChromeCommand::RouteDigit {
            digit: 1,
            repeat: false
        })
    ));
    holding.join().unwrap();
}

#[cfg(windows)]
#[test]
fn input_busy_registry_release_is_retired_without_replaying_the_key() {
    let _input_globals = crate::hooks::input_globals_test::exclusive();
    let (_directory, host, calls) = fixture();
    assert!(host.try_dispatch_input_key_for_foreground(key(0xC0, true, true), 0));
    assert!(!crate::hooks::deliver_registry_busy_key_for_test(
        &host,
        key(0x12, false, false)
    ));
    assert!(host.input_gap_pending());
    host.poll_keyboard_work();
    assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::Idle);
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(host.try_dispatch_input_key_for_foreground(key(0xC0, true, true), 0));
}
