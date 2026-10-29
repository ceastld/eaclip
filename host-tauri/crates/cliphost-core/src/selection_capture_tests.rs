use super::*;
use std::sync::atomic::AtomicUsize;

fn fixture() -> (tempfile::TempDir, HostState, String) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    let parent = host
        .favor_folder_create(&json!({"name": "Selection"}))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    (dir, host, parent)
}

#[test]
fn selection_capture_entrypoints_reject_activation_and_copy_failures_without_using_old_data() {
    let (dir, host, parent) = fixture();
    let file = dir.path().join("old-clipboard-file.txt");
    let destination = dir.path().join("destination");
    std::fs::write(&file, "must remain here").unwrap();
    std::fs::create_dir(&destination).unwrap();
    let _snapshot = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("stale clipboard".into()),
        files: vec![file.to_string_lossy().into_owned()],
        ..Default::default()
    });
    for fail_on in [1, 2] {
        for entry in ["draft", "favorite", "move-files"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let inner = Arc::downgrade(&host.inner);
            host.set_input_runner(move |job| {
                assert!(
                    inner.upgrade().unwrap().try_lock().is_some(),
                    "input must not occupy host state"
                );
                if seen.fetch_add(1, Ordering::SeqCst) + 1 == fail_on {
                    return Err("selection input rejected".into());
                }
                job()
            });
            let error = match entry {
                "draft" => host.execute_verb("collect clip").unwrap_err(),
                "favorite" => host
                    .favor_collect_from_clipboard_menu(&json!({"parentId": parent}), 0)
                    .unwrap_err(),
                _ => host
                    .file_menu_move_selected_into(destination.to_str().unwrap(), 0)
                    .unwrap_err(),
            };
            assert_eq!(error, "selection input rejected", "{entry}");
            assert_eq!(calls.load(Ordering::SeqCst), fail_on);
            assert!(host.collect_drafts.lock().is_empty());
            assert_eq!(
                host.inner.lock().store.lock().list_favors().unwrap().len(),
                1
            );
            assert!(file.exists());
            assert!(!destination.join(file.file_name().unwrap()).exists());
            assert!(host.clipboard_operation.try_lock().is_some());
        }
    }
}

#[test]
fn selection_capture_draft_uses_copy_result_and_explicit_clipboard_add_still_reads_current_data() {
    let (_dir, host, parent) = fixture();
    let _old = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("old clipboard".into()),
        ..Default::default()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let selected_guard = Arc::new(Mutex::new(None));
    let retain = selected_guard.clone();
    host.set_input_runner(move |job| {
        job()?;
        if seen.fetch_add(1, Ordering::SeqCst) == 1 {
            *retain.lock() = Some(crate::clipboard::install_test_snapshot(ClipboardSnapshot {
                text: Some("fresh selection".into()),
                ..Default::default()
            }));
        }
        Ok(())
    });
    let collected = host.execute_verb("collect clip --title Selected").unwrap();
    let draft = host
        .collect_get(&json!({"draftId": collected["draftId"]}))
        .unwrap();
    assert_eq!(draft["detail"]["bodyText"], "fresh selection");
    assert_eq!(draft["detail"]["title"], "Selected");
    assert!(host
        .inner
        .lock()
        .store
        .lock()
        .list_clips()
        .unwrap()
        .is_empty());

    let captured = ClipboardSnapshot {
        text: Some("captured favorite".into()),
        ..Default::default()
    };
    let _replacement = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        text: Some("current clipboard".into()),
        ..Default::default()
    });
    host.favor_from_snapshot(&json!({"parentId": parent}), captured)
        .unwrap();
    host.favor_from_clipboard(&json!({"parentId": parent}))
        .unwrap();
    let items = host.inner.lock().store.lock().list_favors().unwrap();
    assert!(items
        .iter()
        .any(|item| item.body_text.as_deref() == Some("captured favorite")));
    assert!(items
        .iter()
        .any(|item| item.body_text.as_deref() == Some("current clipboard")));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "explicit clipboard additions do not send Ctrl+C"
    );
}
