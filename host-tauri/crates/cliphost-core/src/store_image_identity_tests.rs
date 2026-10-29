use super::*;
use crate::model::ClipRecord;

fn fixture(documents: bool) -> (tempfile::TempDir, AppStore) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp/image-identity-store-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let path = directory.path().join("userdata.db");
    if documents {
        rusqlite::Connection::open(&path).unwrap().execute_batch(
            "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);"
        ).unwrap();
    }
    let store = AppStore::open(&path).unwrap();
    (directory, store)
}

fn file_sha(symbol: char) -> String {
    symbol.to_string().repeat(64)
}
fn key(symbol: char) -> String {
    format!("{PIXEL_PREFIX}{}", file_sha(symbol))
}
fn image(id: &str, symbol: char) -> ClipRecord {
    let mut item = ClipRecord::new_text("", "image fixture");
    item.kind = ClipKind::ImageClipItem;
    item.id = id.into();
    item.content_hash = file_sha(symbol);
    item.image_path = Some(format!("fixture-{id}.png"));
    item
}
fn receipt(id: &str, sequence: u64) -> String {
    serde_json::json!({"origin":"peer","sequence":sequence,"id":id,"hash":"fixture",
        "ack":{"status":"accepted","history":"saved","historyId":id}})
    .to_string()
}

#[test]
fn cache_accepts_only_canonical_exact_identity_and_rejects_conflicts() {
    for documents in [false, true] {
        let (_directory, store) = fixture(documents);
        store
            .cache_image_identity(&file_sha('A'), &key('B'))
            .unwrap();
        assert_eq!(
            store.cached_image_identity(&file_sha('a')).unwrap(),
            Some(key('b'))
        );
        assert_eq!(store.cached_image_identity("not-a-sha").unwrap(), None);
        assert!(store.cache_image_identity("not-a-sha", &key('b')).is_err());
        assert!(store
            .cache_image_identity(&file_sha('a'), &format!("image-bytes-v1:{}", file_sha('b')))
            .is_err());
        assert!(store
            .cache_image_identity(&file_sha('a'), &key('c'))
            .is_err());
        assert_eq!(
            store.cached_image_identity(&file_sha('a')).unwrap(),
            Some(key('b'))
        );
    }
}

#[test]
fn different_encodings_merge_in_both_capture_and_sync_arrival_orders() {
    for documents in [false, true] {
        for sync_first in [false, true] {
            let (_directory, store) = fixture(documents);
            let native = image("native", 'a');
            let synced = image("synced", 'b');
            store
                .cache_image_identity(&native.content_hash, &key('c'))
                .unwrap();
            store
                .cache_image_identity(&synced.content_hash, &key('c'))
                .unwrap();
            let prepared = PreparedImageIdentities::default();
            let receipt_key = "device-sync-receipt:fixture";
            if sync_first {
                store
                    .commit_device_sync_receipt_prepared(
                        receipt_key,
                        &receipt(&synced.id, 1),
                        Some(&synced),
                        &prepared,
                    )
                    .unwrap();
                let merged = store
                    .persist_capture_prepared(&native, false, |_| false, &prepared)
                    .unwrap();
                assert_eq!(merged.id, synced.id);
            } else {
                store
                    .persist_capture_prepared(&native, false, |_| false, &prepared)
                    .unwrap();
                store
                    .commit_device_sync_receipt_prepared(
                        receipt_key,
                        &receipt(&synced.id, 1),
                        Some(&synced),
                        &prepared,
                    )
                    .unwrap();
                let accepted: serde_json::Value =
                    serde_json::from_str(&store.kv_get(receipt_key).unwrap().unwrap()).unwrap();
                assert_eq!(accepted["ack"]["history"], "duplicate");
                assert_eq!(accepted["ack"]["historyId"], native.id);
            }
            assert_eq!(store.list_clips().unwrap().len(), 1);
        }
    }
}

#[test]
fn newly_committed_external_row_requires_preparation_before_capture_or_receipt_write() {
    for documents in [false, true] {
        for sync in [false, true] {
            let (directory, store) = fixture(documents);
            let incoming = image("incoming", 'a');
            store
                .cache_image_identity(&incoming.content_hash, &key('c'))
                .unwrap();
            let prepared = PreparedImageIdentities::default();
            let external = AppStore::open(&directory.path().join("userdata.db")).unwrap();
            let native = image("external", 'b');
            external.insert_clip(&native, true).unwrap();
            let result = if sync {
                store
                    .commit_device_sync_receipt_prepared(
                        "device-sync-receipt:race",
                        &receipt(&incoming.id, 3),
                        Some(&incoming),
                        &prepared,
                    )
                    .map(|_| ())
            } else {
                store
                    .persist_capture_prepared(&incoming, false, |_| false, &prepared)
                    .map(|_| ())
            };
            assert!(matches!(
                result,
                Err(StoreError::ImageIdentityPreparationNeeded)
            ));
            assert_eq!(store.list_clips().unwrap().len(), 1);
            assert!(store.kv_get("device-sync-receipt:race").unwrap().is_none());
            store
                .cache_image_identity(&native.content_hash, &key('c'))
                .unwrap();
            if sync {
                store
                    .commit_device_sync_receipt_prepared(
                        "device-sync-receipt:race",
                        &receipt(&incoming.id, 3),
                        Some(&incoming),
                        &prepared,
                    )
                    .unwrap();
            } else {
                assert_eq!(
                    store
                        .persist_capture_prepared(&incoming, false, |_| false, &prepared)
                        .unwrap()
                        .id,
                    native.id
                );
            }
            assert_eq!(store.list_clips().unwrap().len(), 1);
        }
    }
}

#[test]
fn inaccessible_candidates_are_preserved_and_edited_candidates_must_be_checked_again() {
    for documents in [false, true] {
        let (_directory, store) = fixture(documents);
        let unavailable = image("unavailable", 'a');
        store.insert_clip(&unavailable, true).unwrap();
        let mut prepared = PreparedImageIdentities::default();
        let snapshot = store.get_clip(&unavailable.id).unwrap();
        prepared.checked(&snapshot);
        let mut edited = snapshot.clone();
        edited.image_path = Some("different-locator.png".into());
        store.save_clip(&edited).unwrap();
        let incoming = image("incoming", 'b');
        store
            .cache_image_identity(&incoming.content_hash, &key('c'))
            .unwrap();
        assert!(matches!(
            store.persist_capture_prepared(&incoming, false, |_| false, &prepared),
            Err(StoreError::ImageIdentityPreparationNeeded)
        ));
        prepared.checked(&store.get_clip(&unavailable.id).unwrap());
        let stored = store
            .persist_capture_prepared(&incoming, false, |_| false, &prepared)
            .unwrap();
        assert_eq!(stored.id, incoming.id);
        assert_eq!(store.list_clips().unwrap().len(), 2);
    }
}

#[test]
fn exact_pixel_difference_cannot_be_overruled_by_the_old_weak_hash() {
    for documents in [false, true] {
        let (_directory, store) = fixture(documents);
        let mut first = image("first", 'a');
        let mut incoming = image("incoming", 'b');
        first.perceptual_hash = "0".repeat(64);
        incoming.perceptual_hash = first.perceptual_hash.clone();
        assert!(
            first.looks_equal(&incoming),
            "fixture exposes the legacy weak comparison"
        );
        store.insert_clip(&first, true).unwrap();
        store
            .cache_image_identity(&first.content_hash, &key('c'))
            .unwrap();
        store
            .cache_image_identity(&incoming.content_hash, &key('d'))
            .unwrap();
        assert_eq!(
            store
                .persist_capture_prepared(
                    &incoming,
                    false,
                    |_| false,
                    &PreparedImageIdentities::default()
                )
                .unwrap()
                .id,
            incoming.id
        );
        assert_eq!(store.list_clips().unwrap().len(), 2);
    }
}

#[test]
fn allow_duplicates_and_hidden_history_keep_existing_capture_semantics() {
    for documents in [false, true] {
        let (_directory, store) = fixture(documents);
        let first = image("first", 'a');
        let incoming = image("incoming", 'b');
        store.insert_clip(&first, true).unwrap();
        store
            .cache_image_identity(&incoming.content_hash, &key('c'))
            .unwrap();
        // Unknown candidate keys normally request preparation. User-configured
        // duplicates must not invoke that gate or merge anything.
        store
            .persist_capture_prepared(
                &incoming,
                true,
                |_| false,
                &PreparedImageIdentities::default(),
            )
            .unwrap();
        assert_eq!(store.list_clips().unwrap().len(), 2);
        let hidden_copy = image("hidden-copy", 'd');
        store
            .cache_image_identity(&first.content_hash, &key('c'))
            .unwrap();
        store
            .cache_image_identity(&hidden_copy.content_hash, &key('c'))
            .unwrap();
        store
            .persist_capture_prepared(
                &hidden_copy,
                false,
                |_| true,
                &PreparedImageIdentities::default(),
            )
            .unwrap();
        assert_eq!(store.list_clips().unwrap().len(), 3);
    }
}

#[test]
fn identity_preparation_does_not_expand_the_recent_candidate_window() {
    let (_directory, store) = fixture(false);
    store.insert_clip(&image("old-image", 'a'), true).unwrap();
    for index in 0..RECENT_DUPLICATE_SCAN_COUNT {
        let mut item = ClipRecord::new_text(format!("newer-{index}"), "fixture");
        item.create_time += chrono::Duration::seconds(index as i64 + 1);
        store.insert_clip(&item, true).unwrap();
    }
    let incoming = image("incoming", 'b');
    store
        .cache_image_identity(&incoming.content_hash, &key('c'))
        .unwrap();
    store
        .persist_capture_prepared(
            &incoming,
            false,
            |_| false,
            &PreparedImageIdentities::default(),
        )
        .unwrap();
    assert_eq!(
        store.list_clips().unwrap().len(),
        RECENT_DUPLICATE_SCAN_COUNT + 2
    );
}

#[test]
fn prepared_receipt_failure_rolls_back_history_and_receipt_together() {
    for documents in [false, true] {
        let (directory, store) = fixture(documents);
        let incoming = image("incoming", 'a');
        store
            .cache_image_identity(&incoming.content_hash, &key('c'))
            .unwrap();
        let database = rusqlite::Connection::open(directory.path().join("userdata.db")).unwrap();
        let table = if documents { "cliphost_kv" } else { "kv" };
        database.execute_batch(&format!("CREATE TRIGGER reject_image_receipt BEFORE INSERT ON {table} WHEN NEW.key GLOB 'device-sync-receipt:*' BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap();
        assert!(store
            .commit_device_sync_receipt_prepared(
                "device-sync-receipt:rollback",
                &receipt(&incoming.id, 9),
                Some(&incoming),
                &PreparedImageIdentities::default()
            )
            .is_err());
        assert!(store.list_clips().unwrap().is_empty());
        assert!(store
            .kv_get("device-sync-receipt:rollback")
            .unwrap()
            .is_none());
        assert!(store
            .kv_get("device-sync-high-water:peer")
            .unwrap()
            .is_none());
    }
}

#[test]
fn failed_capture_commit_keeps_incoming_artifact_available_for_retry() {
    let (directory, store) = fixture(false);
    let kept_path = directory.path().join("kept.png");
    let incoming_path = directory.path().join("incoming.png");
    std::fs::write(&kept_path, b"fixture").unwrap();
    std::fs::write(&incoming_path, b"fixture").unwrap();
    let mut kept = image("kept", 'a');
    kept.image_path = Some(kept_path.to_string_lossy().into_owned());
    let mut incoming = image("incoming", 'b');
    incoming.image_path = Some(incoming_path.to_string_lossy().into_owned());
    store.insert_clip(&kept, true).unwrap();
    store
        .cache_image_identity(&kept.content_hash, &key('c'))
        .unwrap();
    store
        .cache_image_identity(&incoming.content_hash, &key('c'))
        .unwrap();
    // The merge succeeds, but its deferred constraint fails at COMMIT. The PNG
    // must remain available until the database transaction is durable.
    store.conn.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE fixture_parent (id INTEGER PRIMARY KEY);
         CREATE TABLE fixture_child (parent_id INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER reject_capture_commit AFTER UPDATE ON clip_item BEGIN INSERT INTO fixture_child VALUES (99); END;"
    ).unwrap();
    assert!(store
        .persist_capture_prepared(
            &incoming,
            false,
            |_| false,
            &PreparedImageIdentities::default()
        )
        .is_err());
    assert!(incoming_path.exists());
    assert!(kept_path.exists());
    assert_eq!(
        store.get_clip(&kept.id).unwrap().update_time,
        kept.update_time
    );
    store
        .conn
        .execute_batch("DROP TRIGGER reject_capture_commit;")
        .unwrap();
    let merged = store
        .persist_capture_prepared(
            &incoming,
            false,
            |_| false,
            &PreparedImageIdentities::default(),
        )
        .unwrap();
    assert_eq!(merged.id, kept.id);
    assert!(!incoming_path.exists());
    assert!(kept_path.exists());
}
