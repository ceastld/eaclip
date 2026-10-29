use super::*;
use std::collections::BTreeSet;

fn fixture() -> (tempfile::TempDir, HostState, rusqlite::Connection) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    let db = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    (dir, host, db)
}

fn add(host: &HostState, text: &str, age: i64, pinned: bool) -> String {
    let mut item = ClipRecord::new_text(text, "fixture");
    item.create_time = Utc::now() - chrono::Duration::seconds(age);
    item.is_top = pinned;
    host.inner
        .lock()
        .store
        .lock()
        .insert_clip(&item, true)
        .unwrap();
    item.id
}

fn ids(host: &HostState) -> BTreeSet<String> {
    host.inner
        .lock()
        .store
        .lock()
        .list_clips()
        .unwrap()
        .into_iter()
        .map(|item| item.id)
        .collect()
}

fn reject_second_delete(db: &rusqlite::Connection) {
    let count: usize = db
        .query_row("SELECT COUNT(*) FROM clip_item", [], |row| row.get(0))
        .unwrap();
    db.execute_batch(&format!("CREATE TRIGGER reject_delete BEFORE DELETE ON clip_item WHEN (SELECT COUNT(*) FROM clip_item) = {} BEGIN SELECT RAISE(ABORT, 'history fixture'); END;", count - 1)).unwrap();
}

fn expire_clear(host: &HostState) {
    host.inner.lock().pending_clear.as_mut().unwrap().expires_at = Instant::now();
}

fn schedule_cleanup(host: &HostState) {
    let mut inner = host.inner.lock();
    inner.cleanup_pending = true;
    inner.last_cleanup_at = Instant::now() - CLEANUP_SAMPLE;
}

#[test]
fn history_maintenance_distinct_failure_rolls_back_every_delete_and_can_retry() {
    let (_dir, host, db) = fixture();
    let kept = add(&host, "duplicate", 0, false);
    add(&host, "duplicate", 1, false);
    add(&host, "duplicate", 2, false);
    let before = ids(&host);
    reject_second_delete(&db);
    host.drain_push_events();
    assert!(host
        .distinct(&json!({"scope":"all"}))
        .unwrap_err()
        .contains("history fixture"));
    assert_eq!(ids(&host), before);
    assert!(host.drain_push_events().is_empty());
    db.execute_batch("DROP TRIGGER reject_delete").unwrap();
    assert_eq!(host.distinct(&json!({"scope":"all"})).unwrap()["count"], 2);
    assert_eq!(ids(&host), BTreeSet::from([kept]));
}

#[test]
fn history_maintenance_deferred_failure_restores_visibility_and_reports_once() {
    let (_dir, host, db) = fixture();
    let pinned = add(&host, "pinned", 3, true);
    add(&host, "old one", 2, false);
    add(&host, "old two", 1, false);
    let scheduled = host.clear_all(true).unwrap();
    let session = host.create_session();
    let request = json!({"webSessionId":session["sessionId"],"take":20});
    assert_eq!(host.search_items(&request).unwrap()["total"], 1);
    add(&host, "copied during undo", 0, false);
    let before = ids(&host);
    reject_second_delete(&db);
    expire_clear(&host);
    host.commit_pending_clear();
    assert_eq!(ids(&host), before);
    assert!(host.clear_pending().is_null());
    assert_eq!(host.search_items(&request).unwrap()["total"], 4);
    assert!(host.inner.lock().deferred_hidden.lock().is_empty());
    let toast = host.take_toast_push().unwrap();
    assert_eq!(toast["kind"], "error");
    assert!(toast["message"]
        .as_str()
        .unwrap()
        .contains("history fixture"));
    host.commit_pending_clear();
    assert!(host.take_toast_push().is_none());
    assert!(host
        .clear_cancel(&json!({"clearId":scheduled["clearId"]}))
        .is_err());
    db.execute_batch("DROP TRIGGER reject_delete").unwrap();
    host.clear_all(true).unwrap();
    let new = add(&host, "after retry snapshot", 0, false);
    expire_clear(&host);
    host.commit_pending_clear();
    assert_eq!(ids(&host), BTreeSet::from([pinned, new]));
}

#[test]
fn history_maintenance_retention_preserves_undo_and_rolls_back_failure_then_refreshes() {
    let (_dir, host, db) = fixture();
    let pinned = add(&host, "pinned", 3, true);
    add(&host, "old one", 2, false);
    add(&host, "old two", 1, false);
    let newest = add(&host, "newest", 0, false);
    host.inner.lock().config.max_item_size = 1;
    let before = ids(&host);
    host.clear_all(true).unwrap();
    schedule_cleanup(&host);
    host.poll_history_cleanup();
    assert_eq!(ids(&host), before, "undo snapshot is not retention garbage");
    assert!(host.inner.lock().cleanup_pending);
    host.clear_cancel(&json!({})).unwrap();
    reject_second_delete(&db);
    host.drain_push_events();
    host.poll_history_cleanup();
    assert_eq!(ids(&host), before);
    assert!(!host.inner.lock().cleanup_busy);
    assert_eq!(host.take_toast_push().unwrap()["kind"], "error");
    assert!(!host
        .drain_push_events()
        .iter()
        .any(|(name, _)| name == "clipChanged"));
    db.execute_batch("DROP TRIGGER reject_delete").unwrap();
    schedule_cleanup(&host);
    host.poll_history_cleanup();
    assert_eq!(ids(&host), BTreeSet::from([pinned, newest]));
    let events = host.drain_push_events();
    let delta = events
        .iter()
        .find(|(name, _)| name == "clipChanged")
        .unwrap();
    assert_eq!(delta.1["op"], "delta");
    assert_eq!(delta.1["ids"].as_array().unwrap().len(), 2);
}

#[test]
fn history_maintenance_empty_replacement_cancels_previous_clear() {
    let (_dir, host, _db) = fixture();
    let pinned = add(&host, "pinned", 0, true);
    host.clear_all(false).unwrap();
    assert_eq!(host.clear_all(true).unwrap()["itemCount"], 0);
    assert!(host.clear_pending().is_null());
    assert!(host.inner.lock().deferred_hidden.lock().is_empty());
    host.commit_pending_clear();
    assert_eq!(ids(&host), BTreeSet::from([pinned]));
}

#[test]
fn history_maintenance_database_wait_leaves_window_commands_available() {
    let (_dir, host, _db) = fixture();
    add(&host, "delete me", 0, false);
    host.clear_all(false).unwrap();
    expire_clear(&host);
    let store = host.inner.lock().store.clone();
    let held = store.lock();
    let testing = host.clone();
    let committing = thread::spawn(move || testing.commit_pending_clear());
    let deadline = Instant::now() + Duration::from_secs(3);
    while host.history_operation.try_lock().is_some() && Instant::now() < deadline {
        thread::yield_now();
    }
    let entered = host.history_operation.try_lock().is_none();
    let testing = host.clone();
    let (sent, received) = mpsc::channel();
    let command = thread::spawn(move || sent.send(testing.window_hide()).unwrap());
    let response = received.recv_timeout(Duration::from_secs(3));
    drop(held);
    committing.join().unwrap();
    command.join().unwrap();
    assert!(entered, "maintenance worker reached the database wait");
    assert_eq!(
        response.expect("window command was blocked by database I/O")["ok"],
        true
    );
    assert!(ids(&host).is_empty());
}

#[test]
fn history_maintenance_explicit_batches_roll_back_and_retry_without_partial_records() {
    for operation in ["add", "split_lines", "split_files"] {
        let (_dir, host, db) = fixture();
        let mut source = ClipRecord::new_text("same\nsame\nlast", "fixture");
        if operation == "split_files" {
            source.kind = ClipKind::FileClipItem;
            source.file_paths = vec!["C:\\first.txt".into(), "C:\\second.txt".into()];
            source.content_hash = hash_files(&source.file_paths);
        }
        host.inner
            .lock()
            .store
            .lock()
            .insert_clip(&source, true)
            .unwrap();
        let before = ids(&host);
        let request = json!({"id":source.id,"text":source.body_text,"lineByLine":true});
        let execute = || match operation {
            "add" => host.add_text(&request),
            "split_lines" => host.split_lines(&request),
            "split_files" => host.split_files(&request),
            _ => unreachable!(),
        };
        db.execute_batch(
            "CREATE TRIGGER reject_insert BEFORE INSERT ON clip_item
             WHEN (SELECT COUNT(*) FROM clip_item) = 2
             BEGIN SELECT RAISE(ABORT, 'batch fixture'); END;",
        )
        .unwrap();
        host.drain_push_events();
        assert!(
            execute().unwrap_err().contains("batch fixture"),
            "{operation}"
        );
        assert_eq!(ids(&host), before, "{operation} left a partial batch");
        assert!(
            host.drain_push_events().is_empty(),
            "{operation} published failed data"
        );

        db.execute_batch("DROP TRIGGER reject_insert").unwrap();
        let result = execute().unwrap();
        let expected = if operation == "split_files" { 2 } else { 3 };
        assert_eq!(result["count"], expected);
        let mut added: Vec<_> = host
            .inner
            .lock()
            .store
            .lock()
            .list_clips()
            .unwrap()
            .into_iter()
            .filter(|item| !before.contains(&item.id))
            .collect();
        added.sort_by_key(|item| item.sub_index);
        assert_eq!(added.len(), expected as usize);
        assert!(added
            .iter()
            .enumerate()
            .all(|(index, item)| item.sub_index == index as i32
                && item.create_time == added[0].create_time
                && item.update_time == item.create_time));
        if operation == "split_files" {
            assert_eq!(added[0].file_paths, vec!["C:\\first.txt"]);
            assert_eq!(added[1].file_paths, vec!["C:\\second.txt"]);
        } else {
            assert_eq!(
                added
                    .iter()
                    .map(|item| item.body_text.as_deref())
                    .collect::<Vec<_>>(),
                vec![Some("same"), Some("same"), Some("last")]
            );
        }
        assert_eq!(
            host.drain_push_events()
                .iter()
                .filter(|(name, _)| name == "clipChanged")
                .count(),
            1
        );
        // Read through another connection: the event represents committed data.
        let reopened = AppStore::open(&host.inner.lock().paths.db).unwrap();
        assert_eq!(
            reopened.list_clips().unwrap().len(),
            before.len() + expected as usize
        );
    }
}
