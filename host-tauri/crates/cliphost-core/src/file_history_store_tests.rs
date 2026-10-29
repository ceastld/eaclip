use super::*;
use tempfile::TempDir;

fn fixture(documents: bool) -> (TempDir, AppStore, rusqlite::Connection) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/file-history-store-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let path = directory.path().join("userdata.db");
    let database = rusqlite::Connection::open(&path).unwrap();
    if documents {
        database.execute_batch(
            "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);
             CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);
             CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);"
        ).unwrap();
    }
    let store = AppStore::open(&path).unwrap();
    (directory, store, database)
}

fn file(store: &AppStore, directory: &Path, id: &str, pinned: bool) -> ClipRecord {
    let mut item = ClipRecord::new_text("", "screenshot-fixture");
    item.id = id.into();
    item.kind = ClipKind::FileClipItem;
    item.body_text = None;
    item.file_paths = vec![directory.join(format!("{id}.png")).to_string_lossy().into_owned()];
    item.content_hash = hash_files(&item.file_paths);
    item.is_top = pinned;
    item.title = Some("user title".into());
    item.sub_index = 7;
    item.description = "user description".into();
    store.insert_clip(&item, true).unwrap();
    store.get_clip(id).unwrap()
}

fn replacement(directory: &Path, snapshot: &ClipRecord) -> FileHistoryImageReplacement {
    FileHistoryImageReplacement {
        snapshot: snapshot.clone(),
        image_path: directory.join("Image/Temp/preserved.png").to_string_lossy().into_owned(),
        content_hash: "validated-pixel-content".into(),
        perceptual_hash: "validated-perceptual-content".into(),
    }
}

fn payload(item: &ClipRecord) -> serde_json::Value {
    serde_json::to_value(item).unwrap()
}

#[test]
fn file_history_store_reads_only_bounded_requested_ids_and_preserves_pins() {
    for documents in [false, true] {
        let (directory, store, database) = fixture(documents);
        for id in ["a", "b", "d"] {
            let mut item = ClipRecord::new_text("large unrelated body", "text-fixture");
            item.id = id.into();
            store.insert_clip(&item, true).unwrap();
        }
        file(&store, directory.path(), "c", false);
        let mut image = file(&store, directory.path(), "e", true);
        image.kind = ClipKind::ImageClipItem;
        image.image_path = image.file_paths.pop();
        store.save_clip(&image).unwrap();
        if !documents {
            // A broken unrelated document proves that narrow candidate reads
            // do not decode text rows, including a batch with no candidates.
            database.execute("UPDATE clip_item SET extra_json='invalid json' WHERE id='a'", []).unwrap();
        }
        let ids = ["e", "a", "missing", "c", "e"].map(str::to_owned);
        let candidates = store.file_history_by_ids(&ids).unwrap();
        assert_eq!(candidates.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), ["e", "c"]);
        assert_eq!(candidates[0].kind, ClipKind::ImageClipItem);
        assert!(candidates[0].is_top);
        assert!(store.file_history_by_ids(&[]).unwrap().is_empty());
        assert!(store.file_history_by_ids(&["a".into(), "b".into()]).unwrap().is_empty());
        let mut bounded = vec!["a".to_owned(); 32];
        bounded.push("c".into());
        assert!(store.file_history_by_ids(&bounded).unwrap().is_empty(), "never process more than the first 32 requested ids");
    }
}

#[test]
fn file_history_store_replacement_preserves_metadata_usage_and_favorite_copy() {
    for documents in [false, true] {
        let (directory, store, _) = fixture(documents);
        let original = file(&store, directory.path(), "preserve", true);
        store.update_usage(&clip_usage_key(&original.id)).unwrap();
        let snapshot = store.get_clip(&original.id).unwrap();
        let favorite = FavorRecord::from_clip(&snapshot, None);
        store.insert_favor(&favorite).unwrap();
        let favorite_before = serde_json::to_value(store.get_favor(&favorite.id).unwrap()).unwrap();
        let prepared = replacement(directory.path(), &snapshot);
        // No file is created: this repository layer must trust the worker's
        // prepared result and perform no metadata/read/copy/delete filesystem I/O.
        let result = store.apply_file_history_maintenance(&[prepared.clone()], &[]).unwrap();
        assert!(result.removed.is_empty());
        assert_eq!(result.changed.len(), 1);
        let mut expected = snapshot;
        expected.kind = ClipKind::ImageClipItem;
        expected.image_path = Some(prepared.image_path);
        expected.file_paths.clear();
        expected.content_hash = prepared.content_hash;
        expected.perceptual_hash = prepared.perceptual_hash;
        assert_eq!(payload(&result.changed[0]), payload(&expected));
        assert_eq!(payload(&store.get_clip(&expected.id).unwrap()), payload(&expected));
        assert_eq!(serde_json::to_value(store.get_favor(&favorite.id).unwrap()).unwrap(), favorite_before);
        assert!(store.apply_file_history_maintenance(&[], &[result.changed[0].clone()]).unwrap().removed.is_empty(), "pinned images remain protected");
    }
}

#[test]
fn file_history_store_snapshot_cas_skips_edited_pinned_used_and_deleted_rows() {
    for documents in [false, true] {
        let (directory, store, _) = fixture(documents);
        let edited = file(&store, directory.path(), "edited", false);
        let pinned = file(&store, directory.path(), "pinned", false);
        let used = file(&store, directory.path(), "used", false);
        let deleted = file(&store, directory.path(), "deleted", false);
        let other = AppStore::open(&directory.path().join("userdata.db")).unwrap();
        let mut changed = other.get_clip(&edited.id).unwrap();
        changed.file_paths = vec![directory.path().join("different.png").to_string_lossy().into_owned()];
        changed.content_hash = hash_files(&changed.file_paths);
        other.save_clip(&changed).unwrap();
        let mut changed = other.get_clip(&pinned.id).unwrap();
        changed.is_top = true;
        other.save_clip(&changed).unwrap();
        other.update_usage(&clip_usage_key(&used.id)).unwrap();
        other.delete_clip(&deleted.id).unwrap();
        let snapshots = [edited, pinned, used, deleted];
        let replacements = snapshots.iter().map(|item| replacement(directory.path(), item)).collect::<Vec<_>>();
        let result = store.apply_file_history_maintenance(&replacements, &snapshots).unwrap();
        assert!(result.changed.is_empty());
        assert!(result.removed.is_empty());
        let fresh = store.get_clip("edited").unwrap();
        assert_ne!(fresh.file_paths, snapshots[0].file_paths);
        assert!(store.get_clip("pinned").unwrap().is_top);
        assert_eq!(store.get_clip("used").unwrap().usage_count, 1);
        assert!(store.get_clip("deleted").is_err());
    }
}

#[test]
fn file_history_store_legacy_snapshot_detects_unmodeled_external_document_edits() {
    let (directory, store, database) = fixture(true);
    let original = file(&store, directory.path(), "external", false);
    database.execute(
        "UPDATE clip_item SET Data=json_set(Data,'$.VendorKeep','external edit'),RowToken='external-writer' WHERE Id=?1",
        [&original.id],
    ).unwrap();
    let prepared = replacement(directory.path(), &original);
    let result = store.apply_file_history_maintenance(&[prepared], std::slice::from_ref(&original)).unwrap();
    assert!(result.changed.is_empty());
    assert!(result.removed.is_empty());
    assert_eq!(store.get_clip(&original.id).unwrap().kind, ClipKind::FileClipItem);
    let kept: String = database.query_row("SELECT json_extract(Data,'$.VendorKeep') FROM clip_item WHERE Id=?1", [&original.id], |row| row.get(0)).unwrap();
    assert_eq!(kept, "external edit");
}

#[test]
fn file_history_store_mixed_batch_rolls_back_update_and_all_deletes_on_failure() {
    for documents in [false, true] {
        let (directory, store, database) = fixture(documents);
        let preserved = file(&store, directory.path(), "preserved", false);
        let first = file(&store, directory.path(), "delete-first", false);
        let last = file(&store, directory.path(), "delete-fail", false);
        let prepared = replacement(directory.path(), &preserved);
        for item in [&preserved, &first, &last] {
            store.kv_set(&format!("file-history-missing-v1:{}", item.id), "missing snapshot").unwrap();
        }
        database.execute_batch(
            "CREATE TRIGGER reject_file_history_delete BEFORE DELETE ON clip_item WHEN OLD.id='delete-fail' BEGIN SELECT RAISE(ABORT,'file history fixture'); END;"
        ).unwrap();
        assert!(store.apply_file_history_maintenance(&[prepared.clone()], &[first.clone(), last.clone()]).unwrap_err().to_string().contains("file history fixture"));
        for before in [&preserved, &first, &last] {
            let actual = store.get_clip(&before.id).unwrap();
            assert_eq!(payload(&actual), payload(before));
            assert_eq!(actual.storage_revision, before.storage_revision);
            assert_eq!(store.kv_get(&format!("file-history-missing-v1:{}", before.id)).unwrap().as_deref(), Some("missing snapshot"));
        }
        database.execute_batch("DROP TRIGGER reject_file_history_delete").unwrap();
        let result = store.apply_file_history_maintenance(&[prepared], &[first, last]).unwrap();
        assert_eq!(result.changed.len(), 1);
        assert_eq!(result.removed, ["delete-first", "delete-fail"]);
        for id in ["preserved", "delete-first", "delete-fail"] {
            assert!(store.kv_get(&format!("file-history-missing-v1:{id}")).unwrap().is_none());
        }
        let reopened = AppStore::open(&directory.path().join("userdata.db")).unwrap();
        assert_eq!(reopened.list_clips().unwrap().len(), 1);
        assert_eq!(reopened.get_clip("preserved").unwrap().kind, ClipKind::ImageClipItem);
    }
}

#[test]
fn file_history_store_manual_deletion_removes_only_its_missing_status_atomically() {
    for documents in [false, true] {
        let (directory, store, database) = fixture(documents);
        let first = file(&store, directory.path(), "manual-first", false);
        let second = file(&store, directory.path(), "manual-second", false);
        let third = file(&store, directory.path(), "manual-third", false);
        for item in [&first, &second, &third] {
            store.kv_set(&format!("file-history-missing-v1:{}", item.id), "pending").unwrap();
        }
        store.kv_set("unrelated-setting", "keep").unwrap();
        let table = if documents { "cliphost_kv" } else { "kv" };
        database.execute_batch(&format!(
            "CREATE TRIGGER reject_missing_status_delete BEFORE DELETE ON {table} WHEN OLD.key='file-history-missing-v1:manual-first' BEGIN SELECT RAISE(ABORT,'missing status fixture'); END;"
        )).unwrap();
        assert!(store.delete_clip(&first.id).unwrap_err().to_string().contains("missing status fixture"));
        assert!(store.get_clip(&first.id).is_ok());
        assert!(store.kv_get("file-history-missing-v1:manual-first").unwrap().is_some());
        database.execute_batch("DROP TRIGGER reject_missing_status_delete").unwrap();
        store.delete_clip(&first.id).unwrap();
        assert_eq!(store.delete_clips(&[second.id.clone(), third.id.clone()]).unwrap(), 2);
        for item in [&first, &second, &third] {
            assert!(store.kv_get(&format!("file-history-missing-v1:{}", item.id)).unwrap().is_none());
        }
        assert_eq!(store.kv_get("unrelated-setting").unwrap().as_deref(), Some("keep"));
        store.kv_delete("unrelated-setting").unwrap();
        store.kv_delete("already absent").unwrap();
        assert!(store.kv_get("unrelated-setting").unwrap().is_none());
    }
}

#[test]
fn file_history_store_rejects_invalid_replacements_and_only_removes_file_image_records() {
    for documents in [false, true] {
        let (directory, store, _) = fixture(documents);
        let original = file(&store, directory.path(), "single", false);
        let pinned = file(&store, directory.path(), "pinned", true);
        let mut multiple = file(&store, directory.path(), "multiple", false);
        multiple.file_paths.push("second.png".into());
        store.save_clip(&multiple).unwrap();
        let multiple = store.get_clip(&multiple.id).unwrap();
        let text = ClipRecord::new_text("do not remove", "fixture");
        store.insert_clip(&text, true).unwrap();
        for source in [&multiple, &text] {
            assert!(store.apply_file_history_maintenance(&[replacement(directory.path(), source)], &[]).is_err());
        }
        let mut invalid = replacement(directory.path(), &original);
        invalid.image_path = "relative.png".into();
        assert!(store.apply_file_history_maintenance(&[invalid], std::slice::from_ref(&original)).is_err());
        let mut wrong_revision = original.clone();
        wrong_revision.storage_revision = Some("unrelated-revision".into());
        let result = store.apply_file_history_maintenance(&[], &[pinned.clone(), text.clone(), wrong_revision]).unwrap();
        assert!(result.removed.is_empty());
        let result = store.apply_file_history_maintenance(&[], std::slice::from_ref(&original)).unwrap();
        assert_eq!(result.removed, [original.id]);
        assert!(store.get_clip(&pinned.id).is_ok());
        assert!(store.get_clip(&text.id).is_ok());
        assert!(store.get_clip(&multiple.id).is_ok());
    }
}
