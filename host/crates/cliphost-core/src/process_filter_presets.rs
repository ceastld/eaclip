//! Shared process-filter authority in the selected AppStore, never a new database.
//! Register as a private child of host.rs: #[path = "process_filter_presets.rs"] mod process_filter_presets;

use super::HostState;
use crate::store::AppStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

const STORAGE_KEY: &str = "clip.processFilterPresets.v1";
const INITIAL_REVISION: &str = "0";
const CONFLICT: &str = "进程筛选预设已在另一窗口更新，请重新加载后再保存。";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preset {
    id: String,
    #[serde(default)]
    display_name: String,
    include_exe_names: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyImport {
    origin: String,
    // Keep the exact original source, including fields this version cannot use.
    source_json: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    version: u32,
    revision: String,
    presets: Vec<Preset>,
    deleted_ids: Vec<String>,
    imports: Vec<LegacyImport>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            revision: INITIAL_REVISION.into(),
            presets: Vec::new(),
            deleted_ids: Vec::new(),
            imports: Vec::new(),
        }
    }
}

impl Document {
    fn snapshot(&self) -> Value {
        json!({
            "revision": self.revision,
            "initialized": self.revision != INITIAL_REVISION,
            "presets": self.presets,
            "importedOrigins": self.imports.iter().map(|source| &source.origin).collect::<Vec<_>>(),
        })
    }

    fn check_revision(&self, expected: &str) -> Result<(), String> {
        if self.revision == expected {
            Ok(())
        } else {
            Err(CONFLICT.into())
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveRequest {
    expected_revision: String,
    presets: Vec<Preset>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportRequest {
    expected_revision: String,
    origin: String,
    source_json: String,
}

fn validate_presets(presets: &[Preset]) -> Result<(), String> {
    let mut ids = HashSet::new();
    for preset in presets {
        if preset.id.trim().is_empty() || !ids.insert(&preset.id) {
            return Err("进程筛选预设 ID 为空或重复；原数据未更改。".into());
        }
        if preset.include_exe_names.is_empty()
            || preset
                .include_exe_names
                .iter()
                .any(|name| name.trim().is_empty())
        {
            return Err("进程筛选预设成员为空或损坏；原数据未更改。".into());
        }
    }
    // Names, case, foreground tokens and member order remain exactly as supplied.
    // Missing executable files are valid historical members, not garbage to drop.
    Ok(())
}

fn read(store: &AppStore) -> Result<Document, String> {
    let Some(raw) = store
        .kv_get(STORAGE_KEY)
        .map_err(|error| error.to_string())?
    else {
        return Ok(Document::default());
    };
    let document: Document = serde_json::from_str(&raw)
        .map_err(|error| format!("进程筛选共享存储损坏；原数据未更改：{error}"))?;
    if document.version != 1
        || document.revision.is_empty()
        || document.revision == INITIAL_REVISION
    {
        return Err("进程筛选共享存储版本或修订无效；原数据未更改。".into());
    }
    validate_presets(&document.presets)?;
    let mut origins = HashSet::new();
    let active: HashSet<_> = document.presets.iter().map(|preset| &preset.id).collect();
    let mut deleted = HashSet::new();
    if document
        .deleted_ids
        .iter()
        .any(|id| id.trim().is_empty() || active.contains(id) || !deleted.insert(id))
        || document
            .imports
            .iter()
            .any(|source| source.origin.trim().is_empty() || !origins.insert(&source.origin))
    {
        return Err("进程筛选迁移记录损坏；原数据未更改。".into());
    }
    Ok(document)
}

fn write(store: &AppStore, mut document: Document) -> Result<Value, String> {
    document.revision = uuid::Uuid::new_v4().to_string();
    let raw = serde_json::to_string(&document).map_err(|error| error.to_string())?;
    // Snapshot, tombstones, revision and recovery source commit in one SQLite row.
    store
        .kv_set(STORAGE_KEY, &raw)
        .map_err(|error| error.to_string())?;
    Ok(document.snapshot())
}

fn save(store: &AppStore, body: &Value) -> Result<Value, String> {
    let request: SaveRequest =
        serde_json::from_value(body.clone()).map_err(|error| error.to_string())?;
    validate_presets(&request.presets)?;
    let mut document = read(store)?;
    document.check_revision(&request.expected_revision)?;
    if document.revision != INITIAL_REVISION && document.presets == request.presets {
        return Ok(document.snapshot());
    }
    let next_ids: HashSet<_> = request.presets.iter().map(|preset| &preset.id).collect();
    for preset in &document.presets {
        if !next_ids.contains(&preset.id) && !document.deleted_ids.contains(&preset.id) {
            document.deleted_ids.push(preset.id.clone());
        }
    }
    document.deleted_ids.retain(|id| !next_ids.contains(id));
    document.presets = request.presets;
    write(store, document)
}

fn import(store: &AppStore, body: &Value) -> Result<Value, String> {
    let request: ImportRequest =
        serde_json::from_value(body.clone()).map_err(|error| error.to_string())?;
    if request.origin.trim().is_empty() {
        return Err("未确认旧 WebView 来源，未迁移进程筛选预设。".into());
    }
    let mut document = read(store)?;
    document.check_revision(&request.expected_revision)?;
    if document
        .imports
        .iter()
        .any(|source| source.origin == request.origin)
    {
        return Ok(document.snapshot());
    }
    // Strict decoding: malformed rows must not silently become an empty import.
    let presets: Vec<Preset> = serde_json::from_str(&request.source_json)
        .map_err(|error| format!("旧进程筛选预设损坏，未迁移或覆盖：{error}"))?;
    validate_presets(&presets)?;
    let mut known: HashSet<String> = document
        .presets
        .iter()
        .map(|preset| preset.id.clone())
        .chain(document.deleted_ids.iter().cloned())
        .collect();
    for preset in presets {
        if known.insert(preset.id.clone()) {
            document.presets.push(preset);
        }
    }
    document.imports.push(LegacyImport {
        origin: request.origin,
        source_json: request.source_json,
    });
    write(store, document)
}

impl HostState {
    pub fn process_filter_presets_get(&self) -> Result<Value, String> {
        let store = self.store();
        let db = store.lock();
        read(&db).map(|document| document.snapshot())
    }

    pub fn process_filter_presets_save(&self, body: &Value) -> Result<Value, String> {
        let store = self.store();
        let result = { let db = store.lock(); save(&db, body) }?;
        self.push_event("processFilterPresetsChanged", result.clone());
        Ok(result)
    }

    pub fn process_filter_presets_import(&self, body: &Value) -> Result<Value, String> {
        let store = self.store();
        let result = { let db = store.lock(); import(&db, body) }?;
        self.push_event("processFilterPresetsChanged", result.clone());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, AppStore) {
        let root = std::env::var_os("CLIPHOST_PRESET_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../.tmp/process-preset-tests")
            });
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        (dir, store)
    }

    fn preset(id: &str, label: &str) -> Value {
        json!({"id": id, "displayName": label, "includeExeNames": ["missing.EXE", "foreground", "editor.exe"]})
    }

    fn persist(store: &AppStore, presets: Value) -> Value {
        save(
            store,
            &json!({"expectedRevision": read(store).unwrap().revision, "presets": presets}),
        )
        .unwrap()
    }

    fn migrate(store: &AppStore, origin: &str, presets: Value) -> Value {
        import(
            store,
            &json!({"expectedRevision": read(store).unwrap().revision, "origin": origin,
            "sourceJson": serde_json::to_string(&presets).unwrap()}),
        )
        .unwrap()
    }

    #[test]
    fn unknown_origin_is_not_a_write_and_roundtrip_retains_members() {
        let (dir, store) = fixture();
        assert!(!read(&store).unwrap().snapshot()["initialized"]
            .as_bool()
            .unwrap());
        assert!(store.kv_get(STORAGE_KEY).unwrap().is_none());
        let result = persist(&store, json!([preset("native", "自定义")]));
        assert_eq!(result["importedOrigins"], json!([]));
        drop(store);
        let reopened = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        assert_eq!(read(&reopened).unwrap().snapshot(), result);
        assert_eq!(result["presets"][0], preset("native", "自定义"));
    }

    #[test]
    fn stale_writer_cannot_replace_the_authority() {
        let (_dir, store) = fixture();
        let current = persist(&store, json!([preset("native", "new")]));
        assert_eq!(
            save(&store, &json!({"expectedRevision": "0", "presets": []})).unwrap_err(),
            CONFLICT
        );
        assert_eq!(read(&store).unwrap().snapshot(), current);
    }

    #[test]
    fn native_first_import_appends_legacy_and_preserves_renames_and_deletions() {
        let (_dir, store) = fixture();
        persist(
            &store,
            json!([
                preset("native", "Native"),
                preset("same", "原生改名"),
                preset("deleted", "old")
            ]),
        );
        persist(
            &store,
            json!([preset("native", "Native"), preset("same", "原生改名")]),
        );
        let result = migrate(
            &store,
            "https://tauri.localhost",
            json!([
                preset("same", "旧名称"),
                preset("deleted", "不要复活"),
                preset("legacy", "Legacy")
            ]),
        );
        assert_eq!(
            result["presets"],
            json!([
                preset("native", "Native"),
                preset("same", "原生改名"),
                preset("legacy", "Legacy")
            ])
        );
    }

    #[test]
    fn repeated_and_other_origin_imports_do_not_resurrect_deleted_presets() {
        let (_dir, store) = fixture();
        migrate(&store, "origin-a", json!([preset("old", "Old")]));
        persist(&store, json!([]));
        let current = read(&store).unwrap().snapshot();
        assert_eq!(
            migrate(&store, "origin-a", json!([preset("old", "Old")])),
            current
        );
        let result = migrate(
            &store,
            "origin-b",
            json!([preset("old", "Old"), preset("other", "Other")]),
        );
        assert_eq!(result["presets"], json!([preset("other", "Other")]));
    }

    #[test]
    fn malformed_or_unavailable_sources_do_not_initialize_or_erase() {
        let (_dir, store) = fixture();
        for source in [json!(null), json!("bad json"), json!("{}"), json!("[{}]")] {
            assert!(import(
                &store,
                &json!({"expectedRevision": "0", "origin": "origin", "sourceJson": source})
            )
            .is_err());
            assert!(store.kv_get(STORAGE_KEY).unwrap().is_none());
        }
        let current = persist(&store, json!([preset("native", "N")]));
        assert!(migrate_invalid(&store, current["revision"].clone()).is_err());
        assert_eq!(read(&store).unwrap().snapshot(), current);
    }

    fn migrate_invalid(store: &AppStore, revision: Value) -> Result<Value, String> {
        import(
            store,
            &json!({"expectedRevision": revision, "origin": "", "sourceJson": "[]"}),
        )
    }

    #[test]
    fn corrupt_shared_document_is_never_overwritten_and_source_is_archived_exactly() {
        let (_dir, store) = fixture();
        let raw = "[ {\"id\":\"legacy\",\"includeExeNames\":[\"foreground\"],\"extra\":42} ]";
        import(
            &store,
            &json!({"expectedRevision":"0", "origin":"origin", "sourceJson":raw}),
        )
        .unwrap();
        assert_eq!(read(&store).unwrap().imports[0].source_json, raw);
        store.kv_set(STORAGE_KEY, "broken").unwrap();
        assert!(read(&store).is_err());
        assert!(save(&store, &json!({"expectedRevision":"0", "presets":[]})).is_err());
        assert_eq!(
            store.kv_get(STORAGE_KEY).unwrap().as_deref(),
            Some("broken")
        );
    }

    #[test]
    fn competing_writers_and_import_share_one_revision_guard() {
        let (_dir, store) = fixture();
        let store = std::sync::Arc::new(parking_lot::Mutex::new(store));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2).map(|index| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let db = store.lock();
                if index == 0 {
                    save(&db, &json!({"expectedRevision":"0", "presets":[preset("native", "Native")]}))
                } else {
                    import(&db, &json!({"expectedRevision":"0", "origin":"origin",
                        "sourceJson":serde_json::to_string(&json!([preset("legacy", "Legacy")])).unwrap()}))
                }
            })
        }).collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().err().is_some_and(|error| error == CONFLICT))
                .count(),
            1
        );
        assert_eq!(read(&store.lock()).unwrap().presets.len(), 1);
    }

    #[test]
    fn failed_sql_write_keeps_revision_and_import_marker_unchanged() {
        let (dir, store) = fixture();
        let current = persist(&store, json!([preset("native", "Native")]));
        let connection = rusqlite::Connection::open(dir.path().join("userdata.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_presets BEFORE UPDATE ON kv BEGIN SELECT RAISE(ABORT, 'fixture disk write failure'); END;").unwrap();
        assert!(import(
            &store,
            &json!({"expectedRevision":current["revision"], "origin":"origin", "sourceJson":"[]"})
        )
        .is_err());
        assert!(save(
            &store,
            &json!({"expectedRevision":current["revision"], "presets":[]})
        )
        .is_err());
        assert_eq!(read(&store).unwrap().snapshot(), current);
    }

    #[test]
    fn existing_csharp_document_store_persists_authority_in_the_same_database() {
        let (dir, store) = fixture();
        drop(store);
        let path = dir.path().join("csharp-userdata.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY, Data TEXT, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
            CREATE TABLE favor_item(Id TEXT PRIMARY KEY, Data TEXT, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);").unwrap();
        drop(connection);
        let store = AppStore::open(&path).unwrap();
        assert!(store.is_document_store());
        let current = persist(&store, json!([preset("native", "Native")]));
        drop(store);
        let connection = rusqlite::Connection::open(&path).unwrap();
        let raw: String = connection
            .query_row(
                "SELECT value FROM cliphost_kv WHERE key=?1",
                [STORAGE_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Document>(&raw).unwrap().snapshot(),
            current
        );
        drop(connection);
        assert_eq!(
            read(&AppStore::open(&path).unwrap()).unwrap().snapshot(),
            current
        );
    }
}
