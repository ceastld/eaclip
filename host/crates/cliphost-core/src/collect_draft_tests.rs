use super::*;

fn fixture() -> (tempfile::TempDir, HostState, String, String, String) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    let folder = host
        .favor_folder_create(&json!({"name": "Collected"}))
        .unwrap();
    let parent = folder["id"].as_str().unwrap().to_string();
    let draft = host.execute_verb("collect text --data original").unwrap();
    let draft_id = draft["draftId"].as_str().unwrap().to_string();
    let detail = host.collect_get(&json!({"draftId": draft_id})).unwrap();
    let item_id = detail["detail"]["id"].as_str().unwrap().to_string();
    (dir, host, parent, draft_id, item_id)
}

#[test]
fn collect_commit_failure_keeps_draft_and_rolls_back_for_retry() {
    let (_dir, host, parent, draft_id, item_id) = fixture();
    let store = host.inner.lock().store.clone();
    let original = host.collect_get(&json!({"draftId": draft_id})).unwrap()["detail"].clone();
    let mut params = json!({"draftId": draft_id, "parentId": "deleted-folder", "title": "Edited", "content": "saved"});
    assert_eq!(
        host.collect_commit(&params).unwrap_err(),
        "Invalid parent folder."
    );
    assert_eq!(
        host.collect_get(&json!({"draftId": draft_id})).unwrap()["detail"],
        original
    );

    // Fail after inserting the item, when its folder's children are being saved.
    let db = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    db.execute_batch("CREATE TRIGGER reject_collect_parent BEFORE UPDATE ON favor_item WHEN OLD.is_folder=1 BEGIN SELECT RAISE(ABORT, 'collect fixture'); END;").unwrap();
    params["parentId"] = json!(parent.to_uppercase());
    assert!(host
        .collect_commit(&params)
        .unwrap_err()
        .contains("collect fixture"));
    assert!(
        store.lock().get_favor(&item_id).is_err(),
        "failed transaction leaves no orphan"
    );
    assert!(store.lock().get_favor(&parent).unwrap().children.is_empty());
    assert_eq!(
        host.collect_get(&json!({"draftId": draft_id})).unwrap()["detail"],
        original
    );

    db.execute_batch("DROP TRIGGER reject_collect_parent;")
        .unwrap();
    assert_eq!(host.collect_commit(&params).unwrap()["ok"], true);
    assert!(host.collect_get(&json!({"draftId": draft_id})).is_err());
    let saved = store.lock().get_favor(&item_id).unwrap();
    assert_eq!(saved.parent_id.as_deref(), Some(parent.as_str()));
    assert_eq!(saved.name, "Edited");
    assert_eq!(saved.body_text.as_deref(), Some("saved"));
    assert_eq!(store.lock().get_favor(&parent).unwrap().children, [item_id]);
}

#[test]
fn collect_commit_concurrent_requests_insert_only_once() {
    let (_dir, host, parent, draft_id, item_id) = fixture();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = (0..2).map(|_| {
        let testing = host.clone();
        let barrier = barrier.clone();
        let params = json!({"draftId": draft_id, "parentId": parent, "title": "Saved", "content": "once"});
        thread::spawn(move || {
            barrier.wait();
            testing.collect_commit(&params)
        })
    }).collect::<Vec<_>>();
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results.into_iter().find_map(Result::err).unwrap(),
        "Draft not found or expired."
    );
    let store = host.inner.lock().store.clone();
    assert_eq!(store.lock().get_favor(&parent).unwrap().children, [item_id]);
    assert_eq!(store.lock().list_favors().unwrap().len(), 2);
}
