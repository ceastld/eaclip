use super::*;
use crate::{model::{ClipRecord, FavorRecord}, store::AppStore};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/db-startup-recovery/tests");
    std::fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::tempdir_in(parent).unwrap();
    let path = dir.path().join("userdata.db");
    Connection::open(&path).unwrap().execute_batch(
        "CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data,RowToken);
         CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data,RowToken);
         CREATE TABLE UsageInfo(Id TEXT PRIMARY KEY,Data,RowToken);"
    ).unwrap();
    let store = AppStore::open(&path).unwrap();
    let mut clip = ClipRecord::new_text("readable content", "fixture.exe");
    clip.id = "good-history".into();
    store.insert_clip(&clip, false).unwrap();
    let favor = FavorRecord::from_clip(&clip, None);
    store.insert_favor(&favor).unwrap();
    drop(store);
    (dir, path)
}

fn original(connection: &Connection) -> Vec<(String, String, String)> {
    let mut result = Vec::new();
    for table in ["clip_item", "favor_item", "UsageInfo"] {
        let mut statement = connection.prepare(&format!("SELECT quote(Id),quote(Data) FROM {table} ORDER BY Id")).unwrap();
        let rows = statement.query_map([], |r| Ok((table.into(),r.get(0)?,r.get(1)?))).unwrap();
        result.extend(rows.map(Result::unwrap));
    }
    result
}

#[test]
fn document_recovery_mixed_bad_documents_open_readable_rows_and_preserve_sources() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    let base: String = source.query_row("SELECT Data FROM clip_item WHERE Id='good-history'",[],|r|r.get(0)).unwrap();
    let mut cases = Vec::new();
    for (id, field, value) in [
        ("bad-date", "CreateTime", json!("not a date")),
        ("bad-id", "Id", json!("different-id")),
        ("bad-flag", "IsTop", json!("true")),
        ("missing-item", "Item", Value::Null),
    ] {
        let mut row: Value = serde_json::from_str(&base).unwrap();
        row["Id"] = json!(id);
        row[field] = value;
        cases.push((id, row.to_string()));
    }
    let mut unknown: Value = serde_json::from_str(&base).unwrap();
    unknown["Id"] = json!("unknown-kind");
    unknown["Item"]["ObjType"] = json!(99999);
    cases.push(("unknown-kind", unknown.to_string()));
    cases.push(("bad-json", "{ broken private contents".into()));
    for (id,data) in &cases {
        source.execute("INSERT INTO clip_item(Id,Data) VALUES(?1,?2)",rusqlite::params![id,data]).unwrap();
    }
    source.execute_batch("INSERT INTO favor_item(Id,Data) VALUES('bad-favorite','[]');
        INSERT INTO UsageInfo(Id,Data) VALUES('clip_item_good-history','{bad stats');").unwrap();
    let before = original(&source);
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.counts().unwrap().0, 1);
    assert_eq!(store.list_favors().unwrap().len(), 1);
    let status = store.document_recovery_status().unwrap();
    assert_eq!(status["isolatedRows"], cases.len() + 2);
    assert_eq!(status["readOnly"], true);
    assert!(!status.to_string().contains("private contents"));
    assert!(store.insert_clip(&ClipRecord::new_text("blocked", "test"), false).unwrap_err().to_string().contains("DATABASE_RECOVERY_READ_ONLY"));
    assert_eq!(original(&source), before);
}

#[test]
fn document_recovery_non_text_sql_cells_are_isolated_without_aborting_startup() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    source.execute_batch("INSERT INTO clip_item VALUES('null-data',NULL,NULL);
        INSERT INTO clip_item VALUES('blob-data',x'7b7d',NULL);
        INSERT INTO clip_item VALUES('number-data',123,NULL);
        INSERT INTO clip_item SELECT 'bad-token',Data,42 FROM clip_item WHERE Id='good-history';
        INSERT INTO favor_item VALUES(x'0102','{}',NULL);").unwrap();
    let before = original(&source);
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.counts().unwrap().0, 1);
    assert_eq!(store.document_recovery_status().unwrap()["isolatedRows"], 5);
    assert_eq!(original(&source), before);
}

#[test]
fn document_recovery_external_good_bad_good_refresh_never_exposes_stale_content() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    let data: String = source.query_row("SELECT Data FROM clip_item WHERE Id='good-history'",[],|r|r.get(0)).unwrap();
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.counts().unwrap().0, 1);
    source.execute("UPDATE clip_item SET Data='{' WHERE Id='good-history'",[]).unwrap();
    assert_eq!(store.counts().unwrap().0, 0);
    assert_eq!(store.document_recovery_status().unwrap()["readOnly"], true);
    source.execute("UPDATE clip_item SET Data=?1 WHERE Id='good-history'",[&data]).unwrap();
    assert_eq!(store.counts().unwrap().0, 1);
    assert_eq!(store.document_recovery_status().unwrap()["isolatedRows"], 0);
    store.insert_clip(&ClipRecord::new_text("writes resume", "test"), false).unwrap();
}

#[test]
fn document_recovery_favorite_graph_and_direct_transaction_writes_are_protected() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    source.execute("INSERT INTO favor_item(Id,Data) VALUES('unreadable-child','{')",[]).unwrap();
    let before = original(&source);
    let store = AppStore::open(&path).unwrap();
    let id = store.list_favors().unwrap()[0].id.clone();
    assert!(store.delete_favors(&[id]).is_err());
    assert!(store.replace_favors(&[]).is_err());
    let transaction = store.connection().unchecked_transaction().unwrap();
    assert!(transaction.execute("DELETE FROM rust.cliphost_cache_favor_item",[]).unwrap_err().to_string().contains("DATABASE_RECOVERY_READ_ONLY"));
    assert!(transaction.execute("DELETE FROM main.favor_item",[]).unwrap_err().to_string().contains("DATABASE_RECOVERY_READ_ONLY"));
    assert!(transaction.execute("INSERT INTO main.cliphost_kv(key,value) VALUES('unsafe','{}')",[]).unwrap_err().to_string().contains("DATABASE_RECOVERY_READ_ONLY"));
    drop(transaction);
    assert_eq!(original(&source), before);
}

#[test]
fn document_recovery_leaf_children_feedback_opens_writable_without_source_cleanup() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    source.execute("UPDATE favor_item SET Data=json_set(Data,'$.Children',json('[\"old-reference\"]'))",[]).unwrap();
    let before = original(&source);
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.document_recovery_status().unwrap()["readOnly"], false);
    let mut leaf = store.list_favors().unwrap().remove(0);
    assert!(leaf.children.is_empty());
    assert_eq!(original(&source), before);
    leaf.name = "renamed".into();
    store.save_favor(&leaf).unwrap();
    let children: String = source.query_row("SELECT json_extract(Data,'$.Children') FROM favor_item",[],|r|r.get(0)).unwrap();
    assert_eq!(children, "[\"old-reference\"]");
}

#[test]
fn document_recovery_physical_corruption_still_fails_without_replacing_database() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/db-startup-recovery/tests");
    std::fs::create_dir_all(&dir).unwrap();
    let dir = tempfile::tempdir_in(dir).unwrap();
    let path = dir.path().join("userdata.db");
    let contents = b"not a SQLite database; preserve this original file";
    std::fs::write(&path, contents).unwrap();
    assert!(AppStore::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), contents);
}

#[test]
fn document_recovery_diagnostic_labels_do_not_collide_with_actual_keys() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    // The diagnostic display label of a non-text key must not itself become
    // a uniqueness constraint on arbitrary user keys.
    source.execute_batch("DELETE FROM favor_item;
        INSERT INTO favor_item VALUES(x'0102','{',NULL);
        INSERT INTO favor_item VALUES('<invalid Id #1>','{',NULL);").unwrap();
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.document_recovery_status().unwrap()["isolatedRows"], 2);
    assert_eq!(store.counts().unwrap().0, 1);
}

#[test]
fn document_recovery_invalid_keys_never_hide_valid_rows_or_prevent_external_deletions() {
    let (_dir, path) = fixture();
    let source = Connection::open(&path).unwrap();
    source.execute_batch("DELETE FROM favor_item;
        INSERT INTO favor_item VALUES(x'0102','{',NULL);").unwrap();
    let valid = json!({"Id":"<invalid Id #1>","CreateTime":"2026-01-02T03:04:05Z","Item":{"ObjType":1,"Text":"valid"}});
    source.execute("INSERT INTO favor_item VALUES('<invalid Id #1>',?1,NULL)",[valid.to_string()]).unwrap();
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.list_favors().unwrap().len(), 1);
    source.execute_batch("INSERT INTO clip_item VALUES(NULL,'{',NULL);
        DELETE FROM clip_item WHERE Id='good-history';").unwrap();
    assert_eq!(store.counts().unwrap().0, 0, "a NULL source ID must not retain deleted cached rows");
    assert_eq!(store.list_favors().unwrap().len(), 1, "diagnostic labels must never delete real rows");
}

#[test]
fn document_recovery_full_and_readonly_write_failures_leave_sources_and_status_intact() {
    let (_dir, path) = fixture();
    let store = AppStore::open(&path).unwrap();
    let before = original(&Connection::open(&path).unwrap());
    let connection = &store.connection().inner;
    connection.execute_batch("PRAGMA main.max_page_count=1").unwrap();
    let large = ClipRecord::new_text("x".repeat(2 * 1024 * 1024), "test");
    let error = store.insert_clip(&large, true).unwrap_err();
    assert!(matches!(error, crate::store::StoreError::Sqlite(rusqlite::Error::SqliteFailure(e,_)) if e.code==rusqlite::ErrorCode::DiskFull));
    assert_eq!(original(&Connection::open(&path).unwrap()), before);
    assert_eq!(store.document_recovery_status().unwrap()["readOnly"], false);
    connection.execute_batch("PRAGMA query_only=ON").unwrap();
    let error = store.insert_clip(&ClipRecord::new_text("blocked by SQLite", "test"), true).unwrap_err();
    assert!(matches!(error, crate::store::StoreError::Sqlite(rusqlite::Error::SqliteFailure(e,_)) if e.code==rusqlite::ErrorCode::ReadOnly));
    assert_eq!(original(&Connection::open(&path).unwrap()), before);
    assert_eq!(store.document_recovery_status().unwrap()["readOnly"], false);
    connection.execute_batch("PRAGMA query_only=OFF; PRAGMA main.max_page_count=1073741823").unwrap();
    store.insert_clip(&ClipRecord::new_text("writes resume", "test"), true).unwrap();
}

#[test]
fn document_recovery_busy_is_not_quarantined_and_retries_after_lock_release() {
    let (_dir, path) = fixture();
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let error = AppStore::open(&path).err().unwrap().to_string();
    assert!(error.contains("locked"), "{error}");
    blocker.execute_batch("ROLLBACK").unwrap();
    let store = AppStore::open(&path).unwrap();
    assert_eq!(store.counts().unwrap().0, 1);
    assert_eq!(store.document_recovery_status().unwrap()["readOnly"], false);
}
