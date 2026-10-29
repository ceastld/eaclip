use super::*;

const FOREIGN: InputWindowIdentity = InputWindowIdentity {
    hwnd: 0x112233,
    pid: 424242,
    thread: 424243,
};

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("input-companion-")
        .tempdir_in(root)
        .unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(directory.path().to_path_buf()),
        28965,
    )
    .unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    host.set_panel_web_view_ready(true);
    {
        let mut inner = host.inner.lock();
        inner.host_hwnd = 100;
        inner.panel_hwnd = 200;
        inner.window_visible = true;
        inner.no_focus_session_active = true;
        inner.has_host_fake_focus = true;
        inner.config.dismiss_behavior = "Hide".into();
        inner.ui.panel_visible = true;
        inner.ui.panel_no_focus_session = true;
        inner.ui.panel_fake_focus = true;
    }
    while host.take_chrome_task().is_some() {}
    host.set_input_runner(|_| panic!("companion classification must never inject input"));
    (directory, host)
}

fn publish(host: &HostState, companion: bool) {
    host.refresh_input_companions_with([FOREIGN.hwnd, FOREIGN.hwnd], |_| {
        assert!(
            host.inner.try_lock().is_some(),
            "classification retained HostInner"
        );
        companion
    });
}

fn click(host: &HostState) {
    assert!(!host.try_dispatch_input_mouse(0x0201, -10000, -10000, 0));
}

fn assert_preserved(host: &HostState) {
    let inner = host.inner.lock();
    assert!(inner.window_visible);
    assert!(inner.has_host_fake_focus);
    assert!(inner.ui.panel_visible);
    assert!(inner.ui.panel_fake_focus);
    assert!(inner.qp.pending_commands.is_empty());
}

#[test]
fn input_companion_quicker_no_owner_windows_keep_dismiss_and_fake_focus_protection() {
    for (title, no_activate) in [
        ("SupperMenuWindow", false),
        ("FloatButtonWindow", false),
        ("CircleMenuWindow", false),
        ("ordinary Quicker tool", true),
    ] {
        let (_directory, host) = fixture();
        with_test_window(FOREIGN, || {
            let companion = crate::host_hit_test::is_dismiss_companion(
                false,
                crate::window_menu::is_quicker_ignore_window("Quicker", title),
                no_activate,
            );
            assert!(companion);
            publish(&host, companion);
            click(&host);
            assert_preserved(&host);
            assert!(!std::iter::from_fn(|| host.take_chrome_command())
                .any(|command| matches!(command, ChromeCommand::Hide | ChromeCommand::HidePanel)));
        });
    }
}

#[test]
fn input_companion_known_regular_outside_still_dismisses_clipboard_and_panel() {
    let (_directory, host) = fixture();
    with_test_window(FOREIGN, || {
        publish(&host, false);
        click(&host);
        let inner = host.inner.lock();
        assert!(!inner.window_visible);
        assert!(!inner.ui.panel_visible);
    });
}

#[test]
fn input_companion_unknown_or_slow_worker_does_not_replay_an_outside_click() {
    let (_directory, host) = fixture();
    let worker = host.clone();
    let (entered, ready) = mpsc::channel();
    let classifying = thread::spawn(move || {
        with_test_window(FOREIGN, || {
            worker.refresh_input_companions_with([FOREIGN.hwnd, 0], |_| {
                assert!(worker.inner.try_lock().is_some());
                entered.send(()).unwrap();
                thread::sleep(Duration::from_millis(400));
                false
            });
        })
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    with_test_window(FOREIGN, || {
        let started = Instant::now();
        click(&host);
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_preserved(&host);
        classifying.join().unwrap();
        assert_preserved(&host);
        // Publishing the classification never replays the original click.
        click(&host);
        assert!(!host.inner.lock().window_visible);
    });
}

#[test]
fn input_companion_cache_identity_epoch_and_age_must_all_be_current() {
    for mode in ["pid", "thread", "epoch", "expired"] {
        let (_directory, host) = fixture();
        with_test_window(FOREIGN, || {
            publish(&host, false);
            let mut inner = host.inner.lock();
            let entry = &mut inner.input_companions.entries[0];
            match mode {
                "pid" => entry.identity.pid += 1,
                "thread" => entry.identity.thread += 1,
                "epoch" => entry.epoch = entry.epoch.wrapping_sub(1),
                _ => entry.sampled_at -= Duration::from_secs(1),
            }
            // The pointer/foreground duplicate is independently validated too.
            inner.input_companions.entries.truncate(1);
            drop(inner);
            click(&host);
            assert_preserved(&host);
        });
    }
}

#[test]
fn input_companion_cache_publish_contention_passes_and_does_not_queue_hide() {
    let (_directory, host) = fixture();
    with_test_window(FOREIGN, || {
        let publisher = host.inner.lock();
        click(&host);
        assert!(host.input_gap_pending());
        drop(publisher);
        host.poll_keyboard_work();
        publish(&host, false);
        assert_preserved(&host);
    });
}

#[test]
fn input_companion_fifo_escape_uses_the_same_ready_classification() {
    for classification in [None, Some(true), Some(false)] {
        let (_directory, host) = fixture();
        with_test_window(FOREIGN, || {
            {
                let mut inner = host.inner.lock();
                inner.window_visible = false;
                inner.ui.panel_visible = false;
                inner.fifo = Some(vec!["owned-fixture".into()]);
            }
            if let Some(companion) = classification {
                publish(&host, companion);
            }
            let accepted = host.try_dispatch_input_key_for_foreground(
                KeyEvent {
                    vk: 0x1B,
                    down: true,
                    alt: false,
                    ctrl: false,
                    shift: false,
                    win: false,
                    now_ms: 1,
                },
                FOREIGN.hwnd,
            );
            assert_eq!(accepted, classification == Some(false));
            assert_eq!(
                host.inner.lock().qp.pending_commands.len(),
                usize::from(accepted)
            );
        });
    }
}
