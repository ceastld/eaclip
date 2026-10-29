use super::*;
use crate::model::{ClipKind, ClipRecord, FavorRecord};
use crate::store::{clip_usage_key, AppStore};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/storage-core-runtime");
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(parent).unwrap();
    std::fs::create_dir_all(dir.path().join("Image/Temp")).unwrap();
    let db = dir.path().join("userdata.db");
    // Reuse the actual native AUX schema without invoking any migration or
    // selecting a machine data root. The old record tables remain sentinels.
    drop(AppStore::open(&db).unwrap());
    let c = Connection::open(&db).unwrap();
    c.execute_batch("DROP TABLE usage_info").unwrap();
    c.execute_batch(CORE_SCHEMA).unwrap();
    let writer = register_writer(&c).unwrap();
    {
        let _scope = writer.enter(WriteScope::Migration).unwrap();
        c.execute(
            "INSERT INTO store_info VALUES(1,'dataset','cliphost',2,'core',2,'migration')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO schema_migrations VALUES(1,?1,'2026-10-17T00:00:00.000000000Z')",
            [schema_checksum()],
        )
        .unwrap();
    }
    c.execute_batch("CREATE TABLE storage_core_candidate(singleton INTEGER PRIMARY KEY CHECK(singleton=1),migration_id TEXT NOT NULL,source_roots_json TEXT NOT NULL DEFAULT '[]'); INSERT INTO storage_core_candidate VALUES(1,'migration','[]')").unwrap();
    install_aux_guards(&c).unwrap();
    drop(c);
    (dir, db)
}
fn text(id: &str) -> ClipRecord {
    let mut record = ClipRecord::new_text("正文🙂\r\n", "fixture.exe");
    record.id = id.into();
    record.create_time = chrono::DateTime::parse_from_rfc3339("2026-10-17T00:00:00.123456789Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    record.update_time = record.create_time;
    record
}
fn folder(id: &str) -> FavorRecord {
    let mut f = FavorRecord::from_clip(&text(id), None);
    f.id = id.into();
    f.is_folder = true;
    f.kind = ClipKind::FavoriteClipItem;
    f.body_text = None;
    f.source_clip_id = None;
    f.name = id.into();
    f
}

#[test]
fn explicit_open_only_and_old_record_tables_stay_empty() {
    let (_dir, db) = fixture();
    assert!(AppStore::open(&db).is_err());
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_clip(&text("same"), true).unwrap();
    let mut f = FavorRecord::from_clip(&text("other"), None);
    f.id = "same".into();
    store.insert_favor(&f).unwrap();
    assert_eq!(store.get_clip("same").unwrap().id, "same");
    assert_eq!(store.get_favor("same").unwrap().id, "same");
    let (c, _) = store.connection().core().unwrap();
    for table in ["clip_item", "favor_item"] {
        assert_eq!(
            c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    drop(store);
    assert_eq!(
        AppStore::open_isolated_core(&db)
            .unwrap()
            .get_clip("same")
            .unwrap()
            .create_time
            .timestamp_subsec_nanos(),
        123456789
    );
}

#[test]
fn metadata_edit_preserves_exact_payload_tokens_null_overrides_and_usage_revision() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_clip(&text("h"), true).unwrap();
    let (c, runtime) = store.connection().core().unwrap();
    let payload = r#"{ "text":"正文🙂\r\n", "future":1.23400e+300, "other":1234567890123456789012345678901234567890 }"#;
    let extensions = r#"{"record":{"Future":{"x":1,"X":2}},"payload":{"Huge":1e400}}"#;
    {
        let _scope = runtime.writer.enter(WriteScope::History).unwrap();
        c.execute("UPDATE items SET payload_json=?1,source_extensions_json=?2 WHERE scope='history' AND id='h'",params![payload,extensions]).unwrap();
    }
    let mut item = store.get_clip("h").unwrap();
    let revision = item.storage_revision.clone();
    store.update_usage(&clip_usage_key("h")).unwrap();
    assert_eq!(store.get_clip("h").unwrap().storage_revision, revision);
    item.is_top = true;
    store.save_clip(&item).unwrap();
    let persisted = runtime.item(c, "history", "h").unwrap();
    assert_eq!(persisted.payload_json, payload);
    assert_eq!(persisted.source_extensions_json, extensions);
    assert_eq!(persisted.title, None);
    assert_eq!(persisted.created_at, "2026-10-17T00:00:00.123456789Z");
    assert!(store.save_clip(&item).is_err());
    let mut fresh = store.get_clip("h").unwrap();
    let current = fresh.storage_revision.clone();
    fresh.update_time = chrono::Utc::now();
    store.save_clip(&fresh).unwrap();
    assert_eq!(store.get_clip("h").unwrap().storage_revision, current);
    fresh.body_text = Some("编辑后".into());
    store.save_clip(&fresh).unwrap();
    let changed = runtime.item(c, "history", "h").unwrap();
    assert!(changed.payload_json.contains("1.23400e+300"));
    assert!(changed
        .payload_json
        .contains("1234567890123456789012345678901234567890"));
    assert_eq!(changed.source_extensions_json, extensions);
}

#[test]
fn scope_generation_source_guards_and_command_rollback_are_connection_bound() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_clip(&text("same"), true).unwrap();
    let mut f = FavorRecord::from_clip(&text("unknown"), None);
    f.id = "same".into();
    store.insert_favor(&f).unwrap();
    let (c, r) = store.connection().core().unwrap();
    assert!(c.execute("DELETE FROM items", []).is_err());
    {
        let _scope = r.writer.enter(WriteScope::History).unwrap();
        assert!(c.execute("DELETE FROM items", []).is_err());
        assert!(c
            .execute(
                "UPDATE items SET source_process='changed' WHERE scope='history'",
                []
            )
            .is_err());
        assert!(c
            .execute("DELETE FROM usage_info WHERE id='favor_item_same'", [])
            .is_ok()); // no row means no trigger
        assert!(c
            .execute(
                "INSERT INTO usage_info VALUES('favor_item_same',NULL,0)",
                []
            )
            .is_err());
        assert!(c
            .execute(
                "INSERT OR REPLACE INTO items SELECT * FROM items WHERE scope='history'",
                []
            )
            .is_err());
    }
    assert_eq!(
        c.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let result: Result<(), String> = write(c, &r.writer, WriteScope::History, || {
        c.execute(
            "UPDATE items SET title='must rollback' WHERE scope='history'",
            [],
        )
        .map_err(|e| e.to_string())?;
        Err("synthetic failure".into())
    });
    assert!(result.is_err());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<(), String> = write(c, &r.writer, WriteScope::History, || {
            c.execute(
                "UPDATE items SET title='must rollback' WHERE scope='history'",
                [],
            )
            .unwrap();
            panic!("synthetic cancellation")
        });
    }));
    assert!(panic.is_err());
    assert_eq!(r.item(c, "history", "same").unwrap().title, None);
    assert_eq!(
        c.query_row("SELECT cliphost_write_scope()", [], |r| r
            .get::<_, Option<String>>(0))
            .unwrap(),
        None
    );
    let old = Connection::open(&db).unwrap();
    assert!(old
        .execute("DELETE FROM items WHERE scope='history'", [])
        .is_err());
}

#[test]
fn favorite_copy_keeps_raw_source_hashes_extensions_and_independent_content() {
    let (dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    std::fs::write(
        dir.path().join("Image/Temp/source.png"),
        b"synthetic image bytes",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Image/copied.png"),
        b"synthetic image bytes",
    )
    .unwrap();
    let mut image = text("h-image");
    image.kind = ClipKind::ImageClipItem;
    image.body_text = None;
    image.image_path = Some(
        dir.path()
            .join("Image/Temp/source.png")
            .to_string_lossy()
            .into_owned(),
    );
    image.content_hash = "exact-content-hash".into();
    image.perceptual_hash = "010101".into();
    image.description = "source description".into();
    store.insert_clip(&image, true).unwrap();
    let (c, r) = store.connection().core().unwrap();
    {
        let _scope = r.writer.enter(WriteScope::History).unwrap();
        c.execute(
            "UPDATE items SET source_extensions_json=?1 WHERE scope='history' AND id='h-image'",
            [r#"{"payload":{"Future":1e400}}"#],
        )
        .unwrap();
    }
    let source = store.get_clip("h-image").unwrap();
    let mut favorite = FavorRecord::from_clip(&source, None);
    favorite.image_path = Some(
        dir.path()
            .join("Image/copied.png")
            .to_string_lossy()
            .into_owned(),
    );
    store
        .insert_favor_from_source(
            &favorite,
            "clip_item",
            "h-image",
            source.storage_revision.as_deref(),
        )
        .unwrap();
    let raw = r.item(c, "favorite", &favorite.id).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&raw.payload_json).unwrap();
    assert_eq!(payload["sourceContentHash"], "exact-content-hash");
    assert_eq!(payload["perceptualHash"], "010101");
    assert_eq!(raw.description, "source description");
    assert_eq!(raw.title, None);
    assert!(raw.source_extensions_json.contains("1e400"));
    assert_ne!(
        r.item(c, "history", "h-image").unwrap().payload_json,
        raw.payload_json
    );
    store.delete_clip("h-image").unwrap();
    assert!(store.get_favor(&favorite.id).is_ok());
}

#[test]
fn shared_favorite_children_are_ordered_and_removed_from_all_directories() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_favor(&folder("a")).unwrap();
    store.insert_favor(&folder("b")).unwrap();
    let mut one = FavorRecord::from_clip(&text("new"), None);
    one.id = "one".into();
    store.insert_favor(&one).unwrap();
    let mut two = one.clone();
    two.id = "two".into();
    store.insert_favor(&two).unwrap();
    let mut a = store.get_favor("a").unwrap();
    a.children = vec!["two".into(), "one".into()];
    store.save_favor(&a).unwrap();
    let mut b = store.get_favor("b").unwrap();
    b.children = vec!["one".into()];
    store.save_favor(&b).unwrap();
    store.validate_favorite_graph().unwrap();
    assert_eq!(store.get_favor("a").unwrap().children, ["two", "one"]);
    let mut invalid = store.get_favor("b").unwrap();
    invalid.children = vec!["missing".into()];
    assert!(store.save_favor(&invalid).is_err());
    assert_eq!(store.get_favor("b").unwrap().children, ["one"]);
    store.delete_favor("one").unwrap();
    assert_eq!(store.get_favor("a").unwrap().children, ["two"]);
    assert!(store.get_favor("b").unwrap().children.is_empty());
}

#[test]
fn auxiliary_writes_are_direct_guarded_and_schema_or_future_kind_refuses_open() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.kv_set("key", "value").unwrap();
    store.upsert_json("text_action", "a", r#"{"x":1}"#).unwrap();
    assert_eq!(store.kv_get("key").unwrap().as_deref(), Some("value"));
    assert!(store.list_json("items").is_err());
    let (c, r) = store.connection().core().unwrap();
    assert!(c.execute("DELETE FROM kv", []).is_err());
    store.insert_clip(&text("h"), true).unwrap();
    {
        let _scope = r.writer.enter(WriteScope::History).unwrap();
        c.execute(
            "UPDATE items SET kind='future_leaf' WHERE scope='history'",
            [],
        )
        .unwrap();
    }
    drop(store);
    assert!(AppStore::open_isolated_core(&db).is_err());
    let c = Connection::open(&db).unwrap();
    c.execute_batch("DROP TRIGGER core_kv_insert_guard; CREATE TRIGGER core_kv_insert_guard BEFORE INSERT ON kv BEGIN SELECT cliphost_writer_generation(),cliphost_write_scope(); END;").unwrap();
    drop(c);
    assert!(AppStore::open_isolated_core(&db).is_err());
}

#[test]
fn duplicate_payload_keys_refuse_activation_without_losing_opaque_tokens() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_clip(&text("h"), true).unwrap();
    let (c, runtime) = store.connection().core().unwrap();
    let nested = r#"{ "huge":123456789012345678901234567890, "number":1.2300e+42 }"#;
    let payload = format!(r#"{{"text":"original","future":1e400,"Future":{nested}}}"#);
    {
        let _scope = runtime.writer.enter(WriteScope::History).unwrap();
        c.execute("UPDATE items SET payload_json=?1 WHERE scope='history' AND id='h'", [&payload]).unwrap();
    }
    drop(store);
    let store = AppStore::open_isolated_core(&db).unwrap();
    let mut item = store.get_clip("h").unwrap();
    item.body_text = Some("edited".into());
    store.save_clip(&item).unwrap();
    let (c, runtime) = store.connection().core().unwrap();
    let edited = runtime.item(c, "history", "h").unwrap();
    assert!(edited.payload_json.contains(r#""future":1e400"#));
    assert!(edited.payload_json.contains(&format!(r#""Future":{nested}"#)));
    drop(store);

    for (column, duplicate) in [
        ("payload_json", r#"{"text":"first","\u0074ext":"second"}"#),
        ("source_extensions_json", r#"{"future":1,"future":2}"#),
    ] {
        let c = Connection::open(&db).unwrap();
        let writer = register_writer(&c).unwrap();
        {
            let _scope = writer.enter(WriteScope::History).unwrap();
            c.execute("UPDATE items SET payload_json=?1,source_extensions_json='{}' WHERE scope='history' AND id='h'", [&payload]).unwrap();
            c.execute(&format!("UPDATE items SET {column}=?1 WHERE scope='history' AND id='h'"), [duplicate]).unwrap();
        }
        drop(c);
        let error = match AppStore::open_isolated_core(&db) {
            Ok(_) => panic!("duplicate keys in {column} must refuse writable activation"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Invalid payload object or duplicate key."), "{error}");
        let c = Connection::open(&db).unwrap();
        let retained: String = c.query_row(&format!("SELECT {column} FROM items WHERE scope='history' AND id='h'"), [], |row| row.get(0)).unwrap();
        assert_eq!(retained, duplicate);
    }
}

#[test]
fn favorite_projection_is_checked_before_writable_activation() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    let mut favorite = FavorRecord::from_clip(&text("f"), None);
    favorite.id = "f".into();
    store.insert_favor(&favorite).unwrap();
    let payload = r#"{"text":"retained","sourceIcon":7}"#;
    let (c, runtime) = store.connection().core().unwrap();
    {
        let _scope = runtime.writer.enter(WriteScope::Favorite).unwrap();
        assert_eq!(
            c.execute(
                "UPDATE items SET payload_json=?1,icon_override='visible override' WHERE scope='favorite' AND id='f'",
                [payload],
            )
            .unwrap(),
            1,
        );
    }
    drop(store);
    let error = match AppStore::open_isolated_core(&db) {
        Ok(_) => panic!("the full favorite projection must be readable before activation"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("Invalid payload field sourceIcon."), "{error}");
    let retained: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT payload_json FROM items WHERE scope='favorite' AND id='f'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, payload);
}

#[test]
fn favorite_graph_rejects_cycle_across_children_and_owner_fallback() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_favor(&folder("a")).unwrap();
    store.insert_favor(&folder("b")).unwrap();
    let mut a = store.get_favor("a").unwrap();
    // Children alone is a->b; ownership alone is b->a. Only their union cycles.
    a.children = vec!["b".into()];
    a.parent_id = Some("b".into());
    assert!(store.save_favor(&a).is_err());
    let persisted = store.get_favor("a").unwrap();
    assert!(persisted.children.is_empty());
    assert_eq!(persisted.parent_id, None);
    store.validate_favorite_graph().unwrap();
}

#[test]
fn usage_reaches_maximum_once_then_refuses_without_touching_revision() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_clip(&text("h"), true).unwrap();
    let (c, r) = store.connection().core().unwrap();
    {
        let _scope = r.writer.enter(WriteScope::History).unwrap();
        c.execute(
            "INSERT INTO usage_info VALUES('clip_item_h',NULL,2147483646)",
            [],
        )
        .unwrap();
    }
    let revision = store.get_clip("h").unwrap().storage_revision;
    assert_eq!(
        store.update_usage("clip_item_h").unwrap().usage_count,
        i32::MAX
    );
    let before = store.get_usage("clip_item_h").unwrap().last_use_time;
    assert!(store.update_usage("clip_item_h").is_err());
    assert_eq!(
        store.get_usage("clip_item_h").unwrap().last_use_time,
        before
    );
    assert_eq!(store.get_clip("h").unwrap().storage_revision, revision);
    {
        let _scope = r.writer.enter(WriteScope::Migration).unwrap();
        c.execute(
            "UPDATE usage_info SET usage_count=?1 WHERE id='clip_item_h'",
            [i64::from(i32::MAX) + 1],
        )
        .unwrap();
    }
    drop(store);
    let error = match AppStore::open_isolated_core(&db) {
        Ok(_) => panic!("an unreadable usage count must refuse writable activation"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("Usage count is outside the supported range."));
    let retained: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT usage_count FROM usage_info WHERE id='clip_item_h'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, i64::from(i32::MAX) + 1);
}

#[test]
fn explicit_copy_refuses_a_source_changed_while_resources_were_prepared() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    store.insert_clip(&text("source"), true).unwrap();
    let original = store.get_clip("source").unwrap();
    let favorite = FavorRecord::from_clip(&original, None);
    let mut edited = original.clone();
    edited.body_text = Some("changed after copy preparation".into());
    store.save_clip(&edited).unwrap();
    assert!(store
        .insert_favor_from_source(
            &favorite,
            "clip_item",
            "source",
            original.storage_revision.as_deref()
        )
        .is_err());
    assert!(store.list_favors().unwrap().is_empty());
    assert_eq!(
        store.get_clip("source").unwrap().body_text,
        edited.body_text
    );
    assert!(store
        .insert_favor_from_source(&favorite, "clip_item", "source", None)
        .is_err());
}

#[test]
fn no_hash_image_metadata_edits_preserve_the_stored_identity_without_file_access() {
    let (_dir, db) = fixture();
    let store = AppStore::open_isolated_core(&db).unwrap();
    for (id, locator) in [
        ("inside", "Temp/NotPresent.png"),
        ("outside", r"Z:\offline-unmapped-fixture\NotPresent.png"),
    ] {
        let mut image = text(id);
        image.kind = ClipKind::ImageClipItem;
        image.body_text = None;
        image.content_hash.clear();
        image.perceptual_hash.clear();
        image.image_path = Some(locator.into());
        store.insert_clip(&image, true).unwrap();
        let (c, r) = store.connection().core().unwrap();
        let before = r.item(c, "history", id).unwrap();
        let sentinel = format!("original-migrated-{id}");
        {
            let _scope = r.writer.enter(WriteScope::History).unwrap();
            c.execute(
                "UPDATE items SET dedupe_key=?1 WHERE scope='history' AND id=?2",
                params![sentinel, id],
            )
            .unwrap();
        }
        let mut edit = store.get_clip(id).unwrap();
        edit.title = Some("metadata only".into());
        store.save_clip(&edit).unwrap();
        store.update_usage(&clip_usage_key(id)).unwrap();
        let after = r.item(c, "history", id).unwrap();
        assert_eq!(after.dedupe_key, sentinel);
        assert_eq!(after.payload_json, before.payload_json);
    }
    let source = store.get_clip("inside").unwrap();
    let favorite = FavorRecord::from_clip(&source, None);
    store
        .insert_favor_from_source(
            &favorite,
            "clip_item",
            "inside",
            source.storage_revision.as_deref(),
        )
        .unwrap();
    let (c, r) = store.connection().core().unwrap();
    let before = r.item(c, "favorite", &favorite.id).unwrap();
    {
        let _scope = r.writer.enter(WriteScope::Favorite).unwrap();
        c.execute(
            "UPDATE items SET dedupe_key='favorite-original-key' WHERE scope='favorite' AND id=?1",
            [&favorite.id],
        )
        .unwrap();
    }
    let mut edit = store.get_favor(&favorite.id).unwrap();
    edit.name = "favorite metadata only".into();
    edit.icon = "字符:图".into();
    store.save_favor(&edit).unwrap();
    let after = r.item(c, "favorite", &favorite.id).unwrap();
    assert_eq!(after.dedupe_key, "favorite-original-key");
    assert_eq!(after.payload_json, before.payload_json);
}
