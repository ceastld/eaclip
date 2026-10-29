use super::*;

fn folder(host: &HostState, name: &str, parent: Option<&str>) -> String {
    host.favor_folder_create(&json!({"name": name, "parentId": parent}))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn add(host: &HostState, parent: &str, text: &str) -> String {
    host.favor_add_text_items(&json!({"parentId": parent, "text": text}))
        .unwrap()["createdIds"][0]
        .as_str()
        .unwrap()
        .to_string()
}

fn fixture() -> (tempfile::TempDir, HostState, rusqlite::Connection, String) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    let protected = folder(&host, "Home", None);
    add(&host, &protected, "keep home");
    let db = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    (dir, host, db, protected)
}

fn snapshot(host: &HostState) -> std::collections::BTreeMap<String, Value> {
    host.inner
        .lock()
        .store
        .lock()
        .list_favors()
        .unwrap()
        .into_iter()
        .map(|item| (item.id.clone(), serde_json::to_value(item).unwrap()))
        .collect()
}

fn no_favorite_events(host: &HostState) {
    assert!(!host
        .drain_push_events()
        .iter()
        .any(|(name, _)| name == "favoritesChanged"));
}

#[test]
fn favorite_persistence_batch_add_rolls_back_without_notifications() {
    let (_dir, host, db, protected) = fixture();
    db.execute_batch("CREATE TRIGGER reject_second BEFORE INSERT ON favor_item WHEN NEW.body_text='second' BEGIN SELECT RAISE(ABORT, 'batch fixture'); END;").unwrap();
    let before = snapshot(&host);
    host.drain_push_events();
    let paths = json!({"parentId": protected, "paths": ["first", "second"]});
    assert!(host
        .favor_add_paths(&paths)
        .unwrap_err()
        .contains("batch fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
    assert!(host
        .favor_add_text_items(
            &json!({"parentId": protected, "text": "first\nsecond", "lineByLine": true})
        )
        .unwrap_err()
        .contains("batch fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
    let _clipboard = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
        files: vec!["first".into(), "second".into()],
        ..Default::default()
    });
    assert!(host
        .favor_from_clipboard(&json!({"parentId": protected, "optionKey": "files"}))
        .unwrap_err()
        .contains("batch fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
    db.execute_batch("DROP TRIGGER reject_second;").unwrap();
    let result = host.favor_add_paths(&paths).unwrap();
    assert_eq!(result["count"], 2);
    let saved = host
        .inner
        .lock()
        .store
        .lock()
        .get_favor(&protected)
        .unwrap();
    assert_eq!(
        &saved.children[1..],
        &[
            result["createdIds"][0].as_str().unwrap(),
            result["createdIds"][1].as_str().unwrap()
        ]
    );
}

#[test]
fn favorite_persistence_folder_insert_rolls_back_if_parent_save_fails() {
    let (_dir, host, db, protected) = fixture();
    db.execute_batch("CREATE TRIGGER reject_parent BEFORE UPDATE ON favor_item WHEN OLD.is_folder=1 BEGIN SELECT RAISE(ABORT, 'parent fixture'); END;").unwrap();
    let before = snapshot(&host);
    host.drain_push_events();
    assert!(host
        .favor_folder_create(&json!({"parentId": protected, "title": "child"}))
        .unwrap_err()
        .contains("parent fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
}

#[test]
fn favorite_persistence_move_rolls_back_all_links_then_retries_in_order() {
    let (_dir, host, db, protected) = fixture();
    let target = folder(&host, "Target", None);
    let anchor = add(&host, &target, "anchor");
    let item = add(&host, &protected, "moving");
    db.execute_batch("CREATE TRIGGER reject_move BEFORE UPDATE ON favor_item WHEN OLD.body_text='moving' BEGIN SELECT RAISE(ABORT, 'move fixture'); END;").unwrap();
    let before = snapshot(&host);
    host.drain_push_events();
    let request = json!({"itemId": item, "targetFolderId": target, "beforeItemId": anchor});
    assert!(host
        .panel_move(&request)
        .unwrap_err()
        .contains("move fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
    db.execute_batch("DROP TRIGGER reject_move;").unwrap();
    host.panel_move(&request).unwrap();
    let store = host.inner.lock().store.clone();
    assert_eq!(
        store.lock().get_favor(&target).unwrap().children,
        [item.clone(), anchor.clone()]
    );
    assert!(!store
        .lock()
        .get_favor(&protected)
        .unwrap()
        .children
        .contains(&item));
    assert_eq!(
        store.lock().get_favor(&item).unwrap().parent_id,
        Some(target.clone())
    );
    assert!(host
        .drain_push_events()
        .iter()
        .any(|(name, _)| name == "favoritesChanged"));
    host.panel_reorder(&json!({"folderId": target, "orderedChildIds": [anchor, item]}))
        .unwrap();
    assert_eq!(
        store.lock().get_favor(&target).unwrap().children,
        [anchor, item]
    );
}

#[test]
fn favorite_persistence_move_rejects_self_and_descendant_folders() {
    let (_dir, host, _db, protected) = fixture();
    let child = folder(&host, "Child", Some(&protected));
    let grandchild = folder(&host, "Grandchild", Some(&child));
    let before = snapshot(&host);
    host.drain_push_events();
    for target in [&protected, &child, &grandchild] {
        assert!(host
            .panel_move(&json!({"itemId": protected, "targetFolderId": target}))
            .unwrap_err()
            .contains("自身或子文件夹"));
        assert_eq!(snapshot(&host), before);
        no_favorite_events(&host);
    }
    assert!(host.panel_layout().is_ok());
}

#[test]
fn favorite_persistence_delete_rolls_back_subtree_and_batch_on_storage_error() {
    let (_dir, host, db, _protected) = fixture();
    let root = folder(&host, "Delete tree", None);
    let nested = folder(&host, "Nested", Some(&root));
    let blocked = add(&host, &nested, "blocked");
    let sibling = add(&host, &root, "sibling");
    db.execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON favor_item WHEN OLD.body_text='blocked' BEGIN SELECT RAISE(ABORT, 'delete fixture'); END;").unwrap();
    let before = snapshot(&host);
    host.drain_push_events();
    assert!(host
        .favor_delete(&json!({"id": root}))
        .unwrap_err()
        .contains("delete fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
    assert!(host
        .favor_batch_delete(&json!({"ids": [sibling, blocked]}))
        .unwrap_err()
        .contains("delete fixture"));
    assert_eq!(snapshot(&host), before);
    no_favorite_events(&host);
    db.execute_batch("DROP TRIGGER reject_delete;").unwrap();
    assert_eq!(
        host.favor_batch_delete(&json!({"ids": [root, nested, "missing"]}))
            .unwrap()["count"],
        1
    );
    let after = snapshot(&host);
    for id in [root, nested, blocked, sibling] {
        assert!(!after.contains_key(&id));
    }
    assert_eq!(after.len(), 2);
}

#[test]
fn favorite_persistence_delete_uses_parent_ownership_despite_stale_children() {
    let (_dir, host, _db, protected) = fixture();
    let root = folder(&host, "Delete tree", None);
    let owned = add(&host, &root, "owned but missing from child list");
    let store = host.inner.lock().store.clone();
    let unrelated = store.lock().get_favor(&protected).unwrap().children[0].clone();
    let mut corrupt = store.lock().get_favor(&root).unwrap();
    corrupt.children = vec![unrelated.clone()];
    store.lock().save_favor(&corrupt).unwrap();
    host.favor_delete(&json!({"id": root})).unwrap();
    assert!(store.lock().get_favor(&owned).is_err());
    assert!(store.lock().get_favor(&unrelated).is_ok());
}

#[test]
fn favorite_persistence_delete_removes_shared_and_stale_child_references() {
    let (_dir, host, _db, _protected) = fixture();
    let owner = folder(&host, "Owner", None);
    let other = folder(&host, "Other", None);
    let target = folder(&host, "Target", Some(&owner));
    let inner = add(&host, &target, "inner");
    let keep = add(&host, &other, "keep");
    let store = host.inner.lock().store.clone();
    // A shared membership in another folder, plus a reference to a deleted
    // descendant: neither may survive as a broken edge.
    let mut shared = store.lock().get_favor(&other).unwrap();
    shared.children = vec![target.clone(), inner.clone(), keep.clone()];
    store.lock().save_favor(&shared).unwrap();
    host.favor_delete(&json!({"id": target})).unwrap();
    assert!(store.lock().get_favor(&inner).is_err());
    assert_eq!(store.lock().get_favor(&other).unwrap().children, vec![keep]);
    assert!(store.lock().get_favor(&owner).unwrap().children.is_empty());
}

#[test]
fn favorite_persistence_remove_missing_reference_only_drops_broken_edge() {
    let (_dir, host, _db, _protected) = fixture();
    let folder_id = folder(&host, "Folder", None);
    let keep = add(&host, &folder_id, "keep");
    let store = host.inner.lock().store.clone();
    let mut row = store.lock().get_favor(&folder_id).unwrap();
    row.children = vec!["gone".into(), keep.clone()];
    store.lock().save_favor(&row).unwrap();
    // An existing child is never detached by this command.
    assert!(host
        .favor_remove_missing_reference(&json!({"parentId": folder_id, "childId": keep}))
        .is_err());
    host.drain_push_events();
    let result = host
        .favor_remove_missing_reference(&json!({"parentId": folder_id, "childId": "gone"}))
        .unwrap();
    assert_eq!(result["removed"], true);
    assert_eq!(store.lock().get_favor(&folder_id).unwrap().children, vec![keep.clone()]);
    assert!(store.lock().get_favor(&keep).is_ok());
    assert_eq!(
        host.favor_remove_missing_reference(&json!({"parentId": folder_id, "childId": "gone"}))
            .unwrap()["removed"],
        false
    );
}

#[test]
fn folder_rename_preserves_structure_and_refreshes_panel_labels() {
    let (_dir, host, _db, root) = fixture();
    let nested = folder(&host, "Nested", Some(&root));
    add(&host, &nested, "child content");
    for (id, title) in [(&root, "新根目录"), (&nested, "新子目录")] {
        let before = host.favor_get(&json!({"id": id})).unwrap();
        let mut before_record: Value = serde_json::from_str(before["rawJson"].as_str().unwrap()).unwrap();
        host.drain_push_events();
        let updated = host.favor_update(&json!({
            "id": id, "title": format!("  {title}  "), "expectedRevision": before["revision"],
            "bodyText": "not folder content", "filePaths": ["not a folder path"],
            "qqRichText": "not QQ content", "linkUrl": "not a link", "linkTitle": "not a title",
            "command": "not a command", "stepData": "not steps", "executeParam": "not params",
            "parentId": "not a parent", "children": []
        })).unwrap();
        assert_eq!(updated["title"], title);
        assert_eq!(updated["kind"], "FavoriteClipItem");
        assert_eq!(updated, host.favor_get(&json!({"id": id})).unwrap());
        assert_ne!(updated["revision"], before["revision"]);
        let mut after_record: Value = serde_json::from_str(updated["rawJson"].as_str().unwrap()).unwrap();
        for record in [&mut before_record, &mut after_record] {
            record.as_object_mut().unwrap().remove("name");
            record.as_object_mut().unwrap().remove("updateTime");
        }
        assert_eq!(after_record, before_record, "rename must preserve every other stored field");
        assert!(host.drain_push_events().iter().any(|(name, _)| name == "favoritesChanged"));
        let tree = host.favor_tree().unwrap();
        assert_eq!(tree["items"][id]["title"], title);
        assert_eq!(host.panel_page(&json!({"favorId": root, "folderId": id})).unwrap()["title"], title);
    }
    assert_eq!(host.panel_catalog().unwrap()["panels"][0]["title"], "新根目录");
    let layout = host.panel_layout().unwrap();
    assert_eq!(layout["rootTitle"], "新根目录");
    assert_eq!(layout["navEntries"][0]["title"], "新子目录");
    let first = host.favor_get(&json!({"id": nested})).unwrap();
    let second = host.favor_update(&json!({"id": nested, "title": "再次重命名", "expectedRevision": first["revision"]})).unwrap();
    assert_eq!(second["title"], "再次重命名", "returned revision must support a second save");
    host.drain_push_events();
    assert!(host.favor_update(&json!({"id": nested, "title": "过期输入", "expectedRevision": first["revision"]}))
        .unwrap_err().starts_with("WRITE_CONFLICT:"));
    assert_eq!(host.favor_get(&json!({"id": nested})).unwrap(), second);
    no_favorite_events(&host);
}

#[test]
fn folder_rename_storage_failure_keeps_title_and_allows_retry() {
    let (_dir, host, db, root) = fixture();
    let before = host.favor_get(&json!({"id": root})).unwrap();
    let all_before = snapshot(&host);
    db.execute_batch("CREATE TRIGGER reject_folder_title BEFORE UPDATE ON favor_item WHEN OLD.is_folder=1 BEGIN SELECT RAISE(ABORT, 'rename fixture'); END;").unwrap();
    host.drain_push_events();
    let request = json!({"id": root, "title": "renamed", "expectedRevision": before["revision"]});
    assert!(host.favor_update(&request).unwrap_err().contains("rename fixture"));
    assert_eq!(snapshot(&host), all_before);
    assert_eq!(host.favor_get(&json!({"id": root})).unwrap(), before);
    no_favorite_events(&host);
    db.execute_batch("DROP TRIGGER reject_folder_title;").unwrap();
    assert_eq!(host.favor_update(&request).unwrap()["title"], "renamed");
    assert_eq!(host.favor_update(&json!({"id": "missing", "title": "renamed"})).unwrap_err(), "Favorite not found.");
}
