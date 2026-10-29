use super::*;
use std::sync::atomic::AtomicUsize;

struct Fixture {
    _root: tempfile::TempDir,
    host: HostState,
    id: String,
    writes: Arc<AtomicUsize>,
    inputs: Arc<AtomicUsize>,
}

fn fixture() -> Fixture {
    crate::clipboard::force_headless(true);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&directory).unwrap();
    let root = tempfile::tempdir_in(directory).unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    let id = host
        .add_text(&json!({"text":"deferred native paste"}))
        .unwrap()["createdIds"][0]
        .as_str()
        .unwrap()
        .to_string();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    {
        let mut inner = host.inner.lock();
        inner.host_hwnd = isize::MAX;
        inner.last_non_host_fg = 42;
        inner.keyboard_target_hwnd = 42;
        inner.window_visible = true;
        inner.config.dismiss_behavior = "Hide".into();
        inner.config.enable_no_focus_mode = true;
        inner.no_focus_session_active = true;
        inner.has_host_fake_focus = true;
    }
    while host.take_chrome_task().is_some() {}
    let writes = Arc::new(AtomicUsize::new(0));
    let seen = writes.clone();
    host.set_clipboard_ui_runner(move |job| {
        seen.fetch_add(1, Ordering::SeqCst);
        job()
    });
    let inputs = Arc::new(AtomicUsize::new(0));
    let seen = inputs.clone();
    host.set_input_runner(move |job| {
        seen.fetch_add(1, Ordering::SeqCst);
        job()
    });
    Fixture {
        _root: root,
        host,
        id,
        writes,
        inputs,
    }
}

fn assert_rejected(fixture: &Fixture, request: &NativePasteRequest) {
    assert!(fixture
        .host
        .paste_native(&json!({"id":fixture.id}), request)
        .is_err());
    assert_eq!(
        fixture.writes.load(Ordering::SeqCst),
        0,
        "expired input must not write the clipboard"
    );
    assert_eq!(
        fixture.inputs.load(Ordering::SeqCst),
        0,
        "expired input must not activate or inject"
    );
    assert_eq!(
        fixture
            .host
            .store()
            .lock()
            .get_clip(&fixture.id)
            .unwrap()
            .usage_count,
        0
    );
}

#[test]
fn native_paste_uses_captured_target_and_allows_its_own_dismissal() {
    let fixture = fixture();
    let request = fixture.host.capture_native_paste(None).unwrap();
    assert_eq!(request.target.hwnd(), 42);
    // A later history update must not retarget an already captured request.
    fixture.host.inner.lock().last_non_host_fg = 43;
    assert_eq!(
        fixture
            .host
            .paste_native_for_foreground(
                &json!({"id":fixture.id,"sourceHandle":123}),
                &request,
                || 42
            )
            .unwrap()["ok"],
        true
    );
    assert!(
        !fixture.host.inner.lock().window_visible,
        "the original paste dismissal still runs"
    );
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.inputs.load(Ordering::SeqCst),
        2,
        "activation and paste use the existing pipeline"
    );
    assert_eq!(
        fixture
            .host
            .store()
            .lock()
            .get_clip(&fixture.id)
            .unwrap()
            .usage_count,
        1
    );
}

#[test]
fn native_paste_collapsed_fold_digit_uses_the_existing_paste_pipeline() {
    let fixture = fixture();
    {
        let mut inner = fixture.host.inner.lock();
        inner.config.dismiss_behavior = "Fold".into();
        inner.is_collapsed = true;
    }
    fixture.host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 1, repeat: false });
    let task = fixture.host.take_chrome_task().unwrap();
    let request = fixture.host.capture_native_paste(Some(task)).unwrap();
    assert_eq!(fixture.host.paste_native(&json!({"id":fixture.id}), &request).unwrap()["ok"], true);
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.inputs.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.host.store().lock().get_clip(&fixture.id).unwrap().usage_count, 1);
    let snapshot = fixture.host.hide_snapshot();
    assert!(snapshot.visible && snapshot.is_collapsed);
}

#[test]
fn native_paste_collapsed_fold_requires_a_current_digit_task() {
    for command in [None, Some(ChromeCommand::ClipboardNavKey { key: "Enter", shift: false })] {
        let fixture = fixture();
        {
            let mut inner = fixture.host.inner.lock();
            inner.config.dismiss_behavior = "Fold".into();
            inner.is_collapsed = true;
        }
        let task = command.map(|command| {
            fixture.host.enqueue_chrome(command);
            fixture.host.take_chrome_task().unwrap()
        });
        assert!(fixture.host.capture_native_paste(task).is_err());
        assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.inputs.load(Ordering::SeqCst), 0);
    }
    for (behavior, edge_tucked, engaged) in [("Hide", false, true), ("Fold", true, true), ("Fold", false, false)] {
        let fixture = fixture();
        {
            let mut inner = fixture.host.inner.lock();
            inner.config.dismiss_behavior = behavior.into();
            inner.is_collapsed = true;
            inner.edge_tucked = edge_tucked;
            inner.has_host_fake_focus = engaged;
        }
        fixture.host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 1, repeat: false });
        let task = fixture.host.take_chrome_task().unwrap();
        assert!(fixture.host.capture_native_paste(Some(task)).is_err());
        assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.inputs.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn native_paste_edge_tucked_digits_keep_pasting_after_the_first_dismissal() {
    let fixture = fixture();
    {
        let mut inner = fixture.host.inner.lock();
        inner.config.dismiss_behavior = "EdgeHide".into();
        inner.config.enable_edge_hide = true;
    }
    for press in 1..=5 {
        while fixture.host.take_chrome_task().is_some() {}
        let down = KeyEvent {
            vk: 0x31, down: true, ctrl: false, alt: false, shift: false, win: false,
            now_ms: press * 2,
        };
        assert!(fixture.host.dispatch_low_level_key_for_foreground(down, 42));
        let task = fixture.host.take_chrome_task().unwrap();
        assert_eq!(task.command(), ChromeCommand::RouteDigit { digit: 1, repeat: false });
        assert!(!fixture.host.dispatch_low_level_key_for_foreground(KeyEvent { down: false, ..down }, 42));
        let request = fixture.host.capture_native_paste(Some(task))
            .unwrap_or_else(|error| panic!("digit press {press} was swallowed but cannot paste: {error}"));
        assert_eq!(fixture.host.paste_native(&json!({"id":fixture.id}), &request).unwrap()["ok"], true);
        let snapshot = fixture.host.hide_snapshot();
        assert!(snapshot.visible && snapshot.has_fake_focus);
        assert!(fixture.host.inner.lock().edge_tucked);
        assert_eq!(fixture.writes.load(Ordering::SeqCst), press as usize);
        assert_eq!(fixture.inputs.load(Ordering::SeqCst), press as usize * 2);
        assert_eq!(fixture.host.store().lock().get_clip(&fixture.id).unwrap().usage_count, press as i32);
    }
}

#[test]
fn native_paste_edge_tucked_still_rejects_click_and_navigation_tickets() {
    for command in [None, Some(ChromeCommand::ClipboardNavKey { key: "Enter", shift: false })] {
        let fixture = fixture();
        fixture.host.inner.lock().config.dismiss_behavior = "EdgeHide".into();
        let task = command.map(|command| {
            fixture.host.enqueue_chrome(command);
            fixture.host.take_chrome_task().unwrap()
        });
        let expanded_request = fixture.host.capture_native_paste(task).unwrap();
        apply_edge_tuck_locked(&mut fixture.host.inner.lock());
        assert_rejected(&fixture, &expanded_request);
        assert!(fixture.host.capture_native_paste(task).is_err());
    }
}

#[test]
fn native_paste_edge_tucked_digit_ticket_cannot_survive_retired_ownership() {
    for retirement in ["hide", "new-session", "new-list", "released"] {
        let fixture = fixture();
        fixture.host.inner.lock().config.dismiss_behavior = "EdgeHide".into();
        fixture.host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 1, repeat: false });
        let task = fixture.host.take_chrome_task().unwrap();
        let request = fixture.host.capture_native_paste(Some(task)).unwrap();
        apply_edge_tuck_locked(&mut fixture.host.inner.lock());
        match retirement {
            "new-list" => drop_queued_clipboard_list_input(&mut fixture.host.inner.lock()),
            "released" => set_clipboard_keyboard_mode_locked(
                &mut fixture.host.inner.lock(), ClipboardKeyboardMode::Released),
            _ => {
                fixture.host.window_hide();
                if retirement == "new-session" {
                    fixture.host.request_show();
                    apply_edge_tuck_locked(&mut fixture.host.inner.lock());
                }
            }
        }
        assert_rejected(&fixture, &request);
    }
}

#[test]
fn native_paste_collapsed_fold_digit_ticket_cannot_survive_hide_or_new_ownership() {
    for retirement in ["hide", "new-session", "new-list"] {
        let fixture = fixture();
        {
            let mut inner = fixture.host.inner.lock();
            inner.config.dismiss_behavior = "Fold".into();
            inner.is_collapsed = true;
        }
        fixture.host.enqueue_chrome(ChromeCommand::RouteDigit { digit: 1, repeat: false });
        let task = fixture.host.take_chrome_task().unwrap();
        let request = fixture.host.capture_native_paste(Some(task)).unwrap();
        if retirement == "new-list" {
            drop_queued_clipboard_list_input(&mut fixture.host.inner.lock());
        } else {
            fixture.host.window_hide();
            if retirement == "new-session" {
                fixture.host.request_show();
                fixture.host.inner.lock().is_collapsed = true;
            }
        }
        assert_rejected(&fixture, &request);
    }
}

#[test]
fn native_menu_paste_variants_keep_the_captured_target_and_usage_rules() {
    for plain_text in [false, true] {
        let fixture = fixture();
        let second = fixture
            .host
            .add_text(&json!({"text":"second native paste"}))
            .unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        let request = fixture.host.capture_native_paste(None).unwrap();
        fixture.host.inner.lock().last_non_host_fg = 43;
        let params = json!({"ids":[second, fixture.id], "mode":"descending"});
        assert_eq!(
            fixture
                .host
                .paste_native_items_for_foreground(&params, &request, plain_text, || 42)
                .unwrap()["ok"],
            true
        );
        assert!(!fixture.host.inner.lock().window_visible);
        assert_eq!(fixture.writes.load(Ordering::SeqCst), 1);
        for id in [&second, &fixture.id] {
            assert_eq!(
                fixture.host.store().lock().get_clip(id).unwrap().usage_count,
                1
            );
        }
    }
}

#[test]
fn native_menu_paste_variants_reject_a_retired_ticket_without_writes() {
    for plain_text in [false, true] {
        let fixture = fixture();
        let request = fixture.host.capture_native_paste(None).unwrap();
        fixture.host.window_hide();
        fixture.host.request_show();
        assert!(fixture
            .host
            .paste_native_items(&json!({"ids":[fixture.id]}), &request, plain_text)
            .is_err());
        assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.inputs.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn native_paste_hide_then_show_cannot_revive_a_ticket() {
    let fixture = fixture();
    let request = fixture.host.capture_native_paste(None).unwrap();
    fixture.host.window_hide();
    fixture.host.request_show();
    assert!(fixture.host.inner.lock().window_visible);
    assert_rejected(&fixture, &request);
}

#[test]
fn native_paste_document_retirement_cannot_revive_a_ticket() {
    let fixture = fixture();
    let request = fixture.host.capture_native_paste(None).unwrap();
    fixture.host.set_clipboard_web_view_ready(false);
    fixture.host.set_clipboard_web_view_ready(true);
    assert_rejected(&fixture, &request);
}

#[test]
fn native_paste_new_list_ownership_cancels_dequeued_digit_and_click_tickets() {
    for command in [
        None,
        Some(ChromeCommand::RouteDigit {
            digit: 1,
            repeat: false,
        }),
        Some(ChromeCommand::ClipboardNavKey {
            key: "Enter",
            shift: false,
        }),
    ] {
        let fixture = fixture();
        let task = command.map(|command| {
            fixture.host.enqueue_chrome(command);
            fixture.host.take_chrome_task().unwrap()
        });
        let request = fixture.host.capture_native_paste(task).unwrap();
        drop_queued_clipboard_list_input(&mut fixture.host.inner.lock());
        assert_rejected(&fixture, &request);
    }
}

#[test]
fn native_paste_replaced_root_cancels_before_clipboard_write() {
    let fixture = fixture();
    let request = fixture.host.capture_native_paste(None).unwrap();
    fixture.host.inner.lock().host_hwnd -= 1;
    assert_rejected(&fixture, &request);
}

#[test]
fn native_paste_new_foreground_or_rebound_target_still_pastes_once() {
    for rebound in [false, true] {
        let fixture = fixture();
        let request = fixture.host.capture_native_paste(None).unwrap();
        if rebound {
            fixture.host.inner.lock().keyboard_target_hwnd = 43;
        }
        let mut checks = 0;
        assert_eq!(fixture.host.paste_native_for_foreground(
            &json!({"id":fixture.id}), &request, || {
                checks += 1;
                if checks == 1 { 42 } else { 43 }
            }).unwrap()["ok"], true);
        assert_eq!(fixture.writes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.inputs.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.host.store().lock().get_clip(&fixture.id).unwrap().usage_count, 1);
    }
}

#[test]
fn native_menu_and_favorite_paste_allow_a_changed_foreground_after_read() {
    for kind in ["items", "plain", "favorite"] {
        let fixture = fixture();
        let request = fixture.host.capture_native_paste(None).unwrap();
        let mut checks = 0;
        let foreground = || {
            checks += 1;
            if checks == 1 { 42 } else { 43 }
        };
        let result = if kind == "favorite" {
            fixture.host.favorite_from_clip(&json!({"id":fixture.id})).unwrap();
            let id = fixture.host.store().lock().list_favors().unwrap()[0].id.clone();
            fixture.host.favor_paste_native_for_foreground(&json!({"id":id}), &request, foreground)
        } else {
            fixture.host.paste_native_items_for_foreground(
                &json!({"ids":[fixture.id]}), &request, kind == "plain", foreground)
        };
        assert_eq!(result.unwrap()["ok"], true, "{kind}");
        assert_eq!(fixture.writes.load(Ordering::SeqCst), 1, "{kind}");
        assert_eq!(fixture.inputs.load(Ordering::SeqCst), 2, "{kind}");
    }
}

#[test]
fn native_favorite_paste_keeps_target_and_rechecks_after_read() {
    for retire_after_read in [false, true] {
        let fixture = fixture();
        fixture.host.favorite_from_clip(&json!({"id":fixture.id})).unwrap();
        let id = fixture.host.store().lock().list_favors().unwrap()[0].id.clone();
        let request = fixture.host.capture_native_paste(None).unwrap();
        let mut checks = 0;
        let result = fixture.host.favor_paste_native_for_foreground(&json!({"id":id}), &request, || {
            checks += 1;
            if retire_after_read && checks == 2 { fixture.host.window_hide(); }
            42
        });
        if retire_after_read {
            assert!(result.is_err());
            assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.inputs.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(result.unwrap()["ok"], true);
            assert_eq!(fixture.writes.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.inputs.load(Ordering::SeqCst), 2);
        }
    }
}

#[test]
fn native_favorite_paste_cannot_revive_after_hide_and_show() {
    let fixture = fixture();
    fixture.host.favorite_from_clip(&json!({"id":fixture.id})).unwrap();
    let id = fixture.host.store().lock().list_favors().unwrap()[0].id.clone();
    let request = fixture.host.capture_native_paste(None).unwrap();
    fixture.host.window_hide();
    fixture.host.request_show();
    assert!(fixture.host.favor_paste_native(&json!({"id":id}), &request).is_err());
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.inputs.load(Ordering::SeqCst), 0);
}

#[test]
fn native_menu_category_switch_preserves_target_and_rejects_retired_sources() {
    let fixture = fixture();
    let first = fixture.host.menu_create_session_ex("text", "", isize::MAX as i64, 42);
    let session = first["sessionId"].as_str().unwrap();
    let generation = fixture.host.super_menu_state_detail()["generation"].as_i64().unwrap() as i32;
    let next = fixture.host.menu_create_session_for_current(session, generation, "text", isize::MAX as i64).unwrap();
    let next_id = next["sessionId"].as_str().unwrap();
    assert_ne!(session, next_id);
    assert_eq!(fixture.host.menu_session_target_handle(next_id), 42);
    assert_eq!(fixture.host.menu_session_target_handle(session), 0);
    assert!(fixture.host.menu_create_session_for_current(session, generation, "text", 0).is_err());
    let current_generation = fixture.host.super_menu_state_detail()["generation"].as_i64().unwrap() as i32;
    fixture.host.dismiss_native_window("super-menu");
    assert!(fixture.host.menu_create_session_for_current(next_id, current_generation, "text", 0).is_err());
    assert_eq!(fixture.host.super_menu_state_detail()["visible"], false);
}

#[test]
fn native_paste_revalidates_after_history_lookup_before_its_dismissal() {
    let fixture = fixture();
    let request = fixture.host.capture_native_paste(None).unwrap();
    let mut checks = 0;
    let result =
        fixture
            .host
            .paste_native_for_foreground(&json!({"id":fixture.id}), &request, || {
                checks += 1;
                if checks == 2 {
                    fixture.host.window_hide();
                    fixture.host.request_show();
                }
                42
            });
    assert!(result.is_err());
    assert_eq!(checks, 2);
    assert_eq!(fixture.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.inputs.load(Ordering::SeqCst), 0);
}

#[test]
fn native_paste_ticket_cannot_cross_host_instances() {
    let source = fixture();
    let destination = fixture();
    let request = source.host.capture_native_paste(None).unwrap();
    assert_rejected(&destination, &request);
}
