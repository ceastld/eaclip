use super::*;
use std::sync::atomic::AtomicUsize;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    (dir, host)
}

#[test]
fn text_action_workflow_failed_save_preserves_list_and_delete_can_retry() {
    let (_dir, host) = fixture();
    host.text_action_upsert(&json!({"type":"Send","title":"original","data":{"Text":"hello"}}))
        .unwrap();
    let path = host.inner.lock().paths.configs.join("TextActionData.json");
    let bytes = std::fs::read(&path).unwrap();
    let before = host.text_action_list();
    let menu = host.menu_create_session("text");
    let session = menu["sessionId"].as_str().unwrap();
    let node = menu["root"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["title"] == "original")
        .unwrap();
    let context = host
        .menu_nodes_ex(session, node["id"].as_str().unwrap(), "context-menu", None)
        .unwrap();
    let delete = context["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["role"] == "text-delete")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let backup = path.with_extension("fixture-backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    for params in [
        json!({"id":"0","type":"Send","title":"changed"}),
        json!({"type":"Send","title":"new"}),
    ] {
        assert!(host.text_action_upsert(&params).is_err());
        assert_eq!(host.text_action_list(), before);
    }
    assert!(host.text_action_delete(&json!({"id":"0"})).is_err());
    assert_eq!(host.text_action_list(), before);
    assert!(host.menu_invoke(session, delete, &json!({})).is_err());
    assert_eq!(host.text_action_list(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), bytes);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    assert_eq!(
        host.menu_invoke(session, delete, &json!({})).unwrap()["refresh"],
        "Root"
    );
    let paths = host.inner.lock().paths.clone();
    drop(host);
    assert_eq!(
        HostState::open(paths, 28965).unwrap().text_action_list()["actions"],
        json!([])
    );
}

#[test]
fn text_action_workflow_activation_and_copy_failure_stop_before_output() {
    let (_dir, host) = fixture();
    host.inner.lock().text_actions =
        vec![json!({"Type":"Internal","Data":{"Type":"RemoveEmptyLine"}})];
    let _old = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("old clipboard".into()),
        ..Default::default()
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let seen = writes.clone();
    host.set_clipboard_ui_runner(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Err("unexpected output".into())
    });
    for fail_on in [1, 2] {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        host.set_input_runner(move |job| {
            if seen.fetch_add(1, Ordering::SeqCst) + 1 == fail_on {
                Err("selection rejected".into())
            } else {
                job()
            }
        });
        let result = host.execute_text_action(&json!({"actionId":"0"}), 0);
        if fail_on == 1 {
            assert_eq!(result.unwrap_err(), "selection rejected");
        } else {
            assert_eq!(result.unwrap()["closeMenu"], true);
            assert_eq!(
                host.take_toast_push().unwrap()["message"],
                "selection rejected"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), fail_on);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(host.clipboard_operation.try_lock().is_some());
    }
}

#[test]
fn text_action_workflow_reads_copy_result_before_transform_and_does_not_use_history() {
    let (_dir, host) = fixture();
    host.inner.lock().text_actions = vec![json!({"Type":"Internal","Data":{"Type":"ZipJson"}})];
    host.add_text(&json!({"text":"unrelated history"})).unwrap();
    let _old = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("old clipboard is not JSON".into()),
        ..Default::default()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let selected = Arc::new(Mutex::new(None));
    let retain = selected.clone();
    let inner = Arc::downgrade(&host.inner);
    host.set_input_runner(move |job| {
        assert!(inner.upgrade().unwrap().try_lock().is_some());
        job()?;
        if seen.fetch_add(1, Ordering::SeqCst) == 1 {
            *retain.lock() = Some(crate::clipboard::install_test_snapshot(ClipboardSnapshot {
                text: Some("{\"fresh\": true}".into()),
                ..Default::default()
            }));
        }
        Ok(())
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let seen = writes.clone();
    let stages = calls.clone();
    host.set_clipboard_ui_runner(move |job| {
        assert_eq!(
            stages.load(Ordering::SeqCst),
            2,
            "activate and copy before output"
        );
        seen.fetch_add(1, Ordering::SeqCst);
        job()
    });
    assert_eq!(
        host.execute_text_action(&json!({"actionId":"0"}), 0)
            .unwrap()["closeMenu"],
        true
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    drop(selected.lock().take());
    host.set_input_runner(|job| job());
    host.set_clipboard_ui_runner(|_| panic!("history must not replace missing selection"));
    host.execute_text_action(&json!({"actionId":"0"}), 0)
        .unwrap();
    assert_eq!(
        host.take_toast_push().unwrap()["message"],
        "获取选中文本失败"
    );
    for text in ["  \r\n\t", ""] {
        let _selection = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
            text: Some(text.into()),
            ..Default::default()
        });
        assert_eq!(
            host.capture_selected_text(InputTarget::capture(0).unwrap())
                .unwrap(),
            text
        );
    }
}

#[test]
fn text_action_workflow_empty_result_does_not_clear_clipboard_or_replace_selection() {
    let (_dir, host) = fixture();
    let _selection = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("\r\n\r\n".into()),
        ..Default::default()
    });
    host.set_clipboard_ui_runner(|_| panic!("empty result must not clear clipboard"));
    for simulate in [false, true] {
        host.inner.lock().text_actions =
            vec![json!({"Type":"Internal","Simulate":simulate,"Data":{"Type":"RemoveEmptyLine"}})];
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        host.set_input_runner(move |job| {
            assert!(
                seen.fetch_add(1, Ordering::SeqCst) < 2,
                "empty result must not replace selection"
            );
            job()
        });
        host.execute_text_action(&json!({"actionId":"0"}), 0)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "activate and copy only");
    }
}

#[test]
fn text_action_workflow_rechecks_version_at_the_output_runner_and_can_retry() {
    let (_dir, host) = fixture();
    let sequence = Arc::new(std::sync::atomic::AtomicU32::new(41));
    let _sequence = crate::selection_capture::install_test_sequence(sequence.clone());
    let _selection = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("{\"fresh\": true}".into()), ..Default::default()
    });
    for simulate in [false, true] {
        sequence.store(41, Ordering::SeqCst);
        host.inner.lock().text_actions = vec![json!({"Type":"Internal", "Simulate":simulate, "Data":{"Type":"ZipJson"}})];
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let updated = sequence.clone();
        host.set_input_runner(move |job| {
            let call = seen.fetch_add(1, Ordering::SeqCst);
            if simulate && call == 2 { updated.store(42, Ordering::SeqCst); }
            job()
        });
        let updated = sequence.clone();
        host.set_clipboard_ui_runner(move |job| {
            updated.store(42, Ordering::SeqCst);
            job()
        });
        let error = host.execute_text_action(&json!({"actionId":"0"}), 0).unwrap_err();
        assert!(error.contains("剪贴板已变化"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), if simulate { 3 } else { 2 });
        assert!(host.clipboard_operation.try_lock().is_some());
        host.set_input_runner(|job| job());
        host.set_clipboard_ui_runner(|job| job());
        assert!(host.execute_text_action(&json!({"actionId":"0"}), 0).is_ok(), "a fresh selection can be processed again");
    }
}

#[test]
fn text_action_workflow_unsupported_regex_stops_before_replacing_selection() {
    let (_dir, host) = fixture();
    for (pattern, options, text, expected_error) in [
        ("a+", 2048, "aaa".to_string(), "0x800"),
        ("a+", 256 | 64, "aaa".to_string(), "ECMAScript 不能"),
        (r"^(a|aa)+\1$", 256, format!("{}!", "a".repeat(32)), "正则匹配超过"),
        (r"[a-z-[b]c]", 0, "abc".to_string(), "字符类无效"),
        (r"[a-z-[x]-[y]]", 0, "abc".to_string(), "字符类无效"),
        (r"[\甲]", 0, "甲".to_string(), "字符类无效"),
        (r"[\p{Greek}]", 0, "α".to_string(), "字符类无效"),
        (r"(a)\1", 1024, "aa".to_string(), "非回溯模式不支持"),
        ("a+", 1024 | 64, "aaa".to_string(), "非回溯模式不能"),
        (r"^(a|aa)+\1$", 0, format!("{}!", "a".repeat(32)), "正则匹配失败"),
        (r"(?=(a))^(a|aa)+b$", 0, format!("{}!", "a".repeat(32)), "正则匹配超过"),
        (r"^(?:(?<n>a)|(?<n>aa))+b$", 0, format!("{}!", "a".repeat(32)), "正则匹配超过"),
        (r"^(?<n>a)?(?(n)(a|aa)+b|c)$", 0, format!("{}!", "a".repeat(32)), "正则匹配超过"),
        (r"(?(7)a|b)", 0, "a".to_string(), "条件组引用的分组不存在"),
        (r"(?<-missing>a)", 0, "a".to_string(), "平衡组引用的分组不存在"),
        (r"^(?<n>a)(?<-n>)(a|aa)+b$", 0, format!("{}!", "a".repeat(32)), "正则匹配超过"),
    ] {
    let _selection = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some(text),
        ..Default::default()
    });
    host.set_clipboard_ui_runner(|_| panic!("rejected regex must not overwrite clipboard"));
    for simulate in [false, true] {
        host.inner.lock().text_actions = vec![json!({
            "Type":"SingleReplace", "Simulate":simulate,
            "Data":{"Pattern":pattern, "ReplaceMent":"[$&]", "UseRegex":true, "Options":options}
        })];
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        host.set_input_runner(move |job| {
            assert!(seen.fetch_add(1, Ordering::SeqCst) < 2, "rejected regex must not send output");
            job()
        });
        let error = host.execute_text_action(&json!({"actionId":"0"}), 0).unwrap_err();
        assert!(error.contains(expected_error), "{pattern}: {error}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "activate and copy only");
        assert!(host.clipboard_operation.try_lock().is_some());
    }
    }
}
