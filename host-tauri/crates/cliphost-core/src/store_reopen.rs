use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AppStore, StoreError};
use crate::reopen_history::{
    directory_key, is_shell_location, ReopenExclusion, ReopenHistoryEntry, ReopenRuleUndo,
    ReopenStoreMutation, ReopenStoreSnapshot, ReopenUndoReceipt, ReopenUndoRecord, EXCLUSIONS_KEY,
    LEGACY_IMPORT_KEY, UNDO_KEY,
};

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyImportState {
    complete: bool,
    processed_ids: BTreeSet<String>,
    imported: usize,
    malformed: usize,
}

impl AppStore {
    /// Complete history including paths outside the quick-menu limit. Reading
    /// this list never checks directory availability or removes stale rows.
    pub fn list_explorer_history_entries(&self) -> Result<Vec<ReopenHistoryEntry>, StoreError> {
        self.import_legacy_explorer_history()?;
        self.list_explorer_history_entries_unimported()
    }

    pub(super) fn list_explorer_history_entries_unimported(
        &self,
    ) -> Result<Vec<ReopenHistoryEntry>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT path,update_time FROM explorer_record ORDER BY update_time DESC,path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ReopenHistoryEntry {
                path: row.get(0)?,
                recorded_at: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn list_explorer_exclusions(&self) -> Result<Vec<ReopenExclusion>, StoreError> {
        self.kv_get(EXCLUSIONS_KEY)?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map(|rules| rules.unwrap_or_default())
            .map_err(StoreError::from)
    }

    pub(crate) fn reopen_store_snapshot(&self) -> Result<ReopenStoreSnapshot, StoreError> {
        self.import_legacy_explorer_history()?;
        let transaction = self.conn.unchecked_transaction()?;
        let snapshot = self.reopen_snapshot_unlocked()?;
        transaction.commit()?;
        Ok(snapshot)
    }

    fn reopen_snapshot_unlocked(&self) -> Result<ReopenStoreSnapshot, StoreError> {
        Ok(ReopenStoreSnapshot {
            entries: self.list_explorer_history_entries_unimported()?,
            exclusions: self.list_explorer_exclusions()?,
        })
    }

    fn reopen_finish_mutation(
        &self,
        changed: usize,
        skipped: usize,
        rule_changed: bool,
        removed: Vec<ReopenHistoryEntry>,
        rule: Option<ReopenRuleUndo>,
    ) -> Result<ReopenStoreMutation, StoreError> {
        let undo = if removed.is_empty() && rule.is_none() {
            self.kv_delete(UNDO_KEY)?;
            None
        } else {
            let token = uuid::Uuid::new_v4().to_string();
            self.kv_set(
                UNDO_KEY,
                &serde_json::to_string(&ReopenUndoRecord {
                    token: token.clone(),
                    entries: removed,
                    rule,
                })?,
            )?;
            Some(ReopenUndoReceipt { token })
        };
        // Read the response before committing: a failed read rolls back the
        // mutation instead of reporting an ambiguous post-commit error.
        Ok(ReopenStoreMutation {
            changed,
            skipped,
            rule_changed,
            undo,
            snapshot: self.reopen_snapshot_unlocked()?,
        })
    }

    fn reopen_remove_entries(
        &self,
        entries: &[ReopenHistoryEntry],
    ) -> Result<Vec<ReopenHistoryEntry>, StoreError> {
        let mut removed = Vec::new();
        let mut seen = HashSet::new();
        for entry in entries {
            if !seen.insert((&entry.path, entry.recorded_at)) {
                continue;
            }
            if self.conn.execute(
                "DELETE FROM explorer_record WHERE path=?1 AND update_time=?2",
                params![entry.path, entry.recorded_at],
            )? != 0
            {
                removed.push(entry.clone());
            }
        }
        Ok(removed)
    }

    /// All record/rule mutations and their single-use undo receipt commit in
    /// one SQLite transaction. A later user mutation retires an older receipt;
    /// background Explorer records do not retire it and are protected on undo.
    pub(crate) fn reopen_store_apply(
        &self,
        params: &Value,
    ) -> Result<ReopenStoreMutation, StoreError> {
        self.import_legacy_explorer_history()?;
        let action = params["action"].as_str().unwrap_or("");
        let transaction = self.conn.unchecked_transaction()?;
        let result = match action {
            "remove" => {
                let entries = requested_entries(params)?;
                let removed = self.reopen_remove_entries(&entries)?;
                self.reopen_finish_mutation(
                    removed.len(),
                    entries.len().saturating_sub(removed.len()),
                    false,
                    removed,
                    None,
                )?
            }
            "exclude" => {
                let path = requested_path(params)?;
                let include_children = params["includeChildren"].as_bool().unwrap_or(false);
                if include_children && is_shell_location(path) {
                    return Err(StoreError::Core(
                        "Shell 位置只支持排除此位置，不能推断子目录范围。".into(),
                    ));
                }
                let mut rules = self.list_explorer_exclusions()?;
                let key = directory_key(path);
                let before = rules
                    .iter()
                    .find(|rule| directory_key(&rule.path) == key)
                    .cloned();
                rules.retain(|rule| directory_key(&rule.path) != key);
                let rule = ReopenExclusion {
                    path: path.to_string(),
                    include_children,
                    revision: uuid::Uuid::new_v4().to_string(),
                };
                rules.push(rule.clone());
                let requested = if params["removeExisting"].as_bool().unwrap_or(false) {
                    requested_entries(params)?
                        .into_iter()
                        .filter(|entry| rule.matches(&entry.path))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let removed = self.reopen_remove_entries(&requested)?;
                self.kv_set(EXCLUSIONS_KEY, &serde_json::to_string(&rules)?)?;
                self.reopen_finish_mutation(
                    removed.len(),
                    requested.len().saturating_sub(removed.len()),
                    true,
                    removed,
                    Some(ReopenRuleUndo {
                        after: rule,
                        before,
                    }),
                )?
            }
            "unexclude" => {
                let key = directory_key(requested_path(params)?);
                let revision = params["revision"]
                    .as_str()
                    .ok_or_else(|| StoreError::Core("排除规则缺少修订，请刷新后重试。".into()))?;
                let mut rules = self.list_explorer_exclusions()?;
                let previous_len = rules.len();
                rules.retain(|rule| {
                    !(directory_key(&rule.path) == key && rule.revision == revision)
                });
                let changed = previous_len - rules.len();
                if changed != 0 {
                    self.kv_set(EXCLUSIONS_KEY, &serde_json::to_string(&rules)?)?;
                }
                self.reopen_finish_mutation(
                    changed,
                    usize::from(changed == 0),
                    changed != 0,
                    Vec::new(),
                    None,
                )?
            }
            "restore" => {
                let receipt: ReopenUndoReceipt = serde_json::from_value(params["undo"].clone())?;
                let raw = self
                    .kv_get(UNDO_KEY)?
                    .ok_or_else(|| StoreError::Core("这次撤销已失效，请查看最新记录。".into()))?;
                let undo: ReopenUndoRecord = serde_json::from_str(&raw)?;
                if receipt.token != undo.token {
                    return Err(StoreError::Core("这次撤销已失效，请查看最新记录。".into()));
                }
                let mut rules = self.list_explorer_exclusions()?;
                let mut rule_changed = false;
                if let Some(change) = undo.rule {
                    let key = directory_key(&change.after.path);
                    if rules.iter().any(|rule| {
                        directory_key(&rule.path) == key && rule.revision == change.after.revision
                    }) {
                        rules.retain(|rule| directory_key(&rule.path) != key);
                        if let Some(before) = change.before {
                            rules.push(before);
                        }
                        self.kv_set(EXCLUSIONS_KEY, &serde_json::to_string(&rules)?)?;
                        rule_changed = true;
                    }
                }
                let current: HashSet<String> = self
                    .list_explorer_history_entries_unimported()?
                    .iter()
                    .map(|entry| directory_key(&entry.path))
                    .collect();
                let mut changed = 0;
                for entry in &undo.entries {
                    if rules.iter().any(|rule| rule.matches(&entry.path))
                        || current.contains(&directory_key(&entry.path))
                    {
                        continue;
                    }
                    self.conn.execute(
                        "INSERT INTO explorer_record(path,update_time) VALUES(?1,?2)",
                        params![entry.path, entry.recorded_at],
                    )?;
                    changed += 1;
                }
                self.reopen_finish_mutation(
                    changed,
                    undo.entries.len().saturating_sub(changed),
                    rule_changed,
                    Vec::new(),
                    None,
                )?
            }
            "invalidateUndo" => self.reopen_finish_mutation(0, 0, false, Vec::new(), None)?,
            _ => return Err(StoreError::Core("不支持的重开历史操作。".into())),
        };
        transaction.commit()?;
        Ok(result)
    }

    /// One-time, same-connection import. Partial imports remember successful
    /// source IDs so retrying damaged rows cannot resurrect history the user
    /// has already removed. Source documents are always left untouched.
    pub(super) fn import_legacy_explorer_history(&self) -> Result<(), StoreError> {
        if !self.conn.is_document_store() {
            return Ok(());
        }
        let previous_state = self.kv_get(LEGACY_IMPORT_KEY)?;
        let mut state: LegacyImportState = previous_state.as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        if state.complete {
            return Ok(());
        }
        // Clipboard-document recovery protection also covers auxiliary writes.
        // Preserve that read-only mode instead of making an optional history
        // import turn a recoverable store into a failed product startup.
        if self.conn.document_recovery_status()?["readOnly"] == true {
            return Ok(());
        }
        let transaction = self.conn.unchecked_transaction()?;
        let Some(source) = self.conn.legacy_explorer_documents()? else {
            transaction.commit()?;
            return Ok(());
        };
        let mut existing = self.list_explorer_history_entries_unimported()?;
        let mut identities: HashMap<String, usize> = HashMap::new();
        for (index, entry) in existing.iter().enumerate() {
            let key = directory_key(&entry.path);
            if identities.get(&key).is_none_or(|previous| existing[*previous].recorded_at < entry.recorded_at) {
                identities.insert(key, index);
            }
        }
        let rules = self.list_explorer_exclusions()?;
        state.malformed = 0;
        for document in source {
            if state.processed_ids.contains(&document.id) {
                continue;
            }
            let parsed = (|| -> Result<(String, Option<i64>), String> {
                if document.id.trim().is_empty() {
                    return Err("旧重开记录ID为空。".into());
                }
                let value: Value =
                    serde_json::from_str(document.data.as_deref().ok_or("旧重开记录Data为空。")?)
                        .map_err(|error| error.to_string())?;
                let field = |name| crate::config_compat::field(&value, name);
                if let Some(id) = field("Id")? {
                    if id.as_str() != Some(document.id.as_str()) {
                        return Err("旧重开记录ID不一致。".into());
                    }
                }
                let path = field("Path")?
                    .and_then(Value::as_str)
                    .filter(|path| !path.trim().is_empty())
                    .ok_or("旧重开记录Path无效。")?
                    .trim()
                    .to_string();
                let trusted_time = |value: &Value| {
                    crate::csharp_import_document::parse_datetime(value)
                        .ok()
                        .map(|time| time.timestamp_millis())
                        .filter(|time| *time > 0)
                };
                let time = field("UpdateTime")?.and_then(trusted_time).or_else(|| {
                    document
                        .updated
                        .as_deref()
                        .and_then(|text| trusted_time(&Value::String(text.into())))
                });
                Ok((path, time))
            })();
            let (path, time) = match parsed {
                Ok(value) => value,
                Err(_) => {
                    state.malformed += 1;
                    continue;
                }
            };
            let key = directory_key(&path);
            if !rules.iter().any(|rule| rule.matches(&path)) {
                if let Some(index) = identities.get(&key).copied() {
                    let current = &mut existing[index];
                    if time.is_some_and(|time| time > current.recorded_at) {
                        let time = time.unwrap();
                        self.conn.execute(
                            "UPDATE explorer_record SET update_time=?2 WHERE path=?1",
                            params![current.path, time],
                        )?;
                        current.recorded_at = time;
                        state.imported += 1;
                    }
                } else {
                    let recorded_at = time.unwrap_or(0);
                    self.conn.execute(
                        "INSERT INTO explorer_record(path,update_time) VALUES(?1,?2)",
                        params![path, recorded_at],
                    )?;
                    identities.insert(key, existing.len());
                    existing.push(ReopenHistoryEntry { path, recorded_at });
                    state.imported += 1;
                }
            }
            state.processed_ids.insert(document.id);
        }
        state.complete = state.malformed == 0;
        let next_state = serde_json::to_string(&state)?;
        let state_changed = previous_state.as_deref() != Some(next_state.as_str());
        if state_changed { self.kv_set(LEGACY_IMPORT_KEY, &next_state)?; }
        transaction.commit()?;
        if state.malformed != 0 && state_changed {
            tracing::warn!(malformed_rows = state.malformed, "Some legacy Explorer history documents were preserved without importing; migration remains incomplete");
        }
        Ok(())
    }
}

fn requested_entries(params: &Value) -> Result<Vec<ReopenHistoryEntry>, StoreError> {
    let entries: Vec<ReopenHistoryEntry> = serde_json::from_value(params["entries"].clone())?;
    let mut seen = HashSet::new();
    Ok(entries
        .into_iter()
        .filter(|entry| seen.insert((entry.path.clone(), entry.recorded_at)))
        .collect())
}

fn requested_path(params: &Value) -> Result<&str, StoreError> {
    let path = params["path"].as_str().unwrap_or("").trim();
    if path.is_empty() || path.len() > 32768 {
        return Err(StoreError::Core("目录地址为空或过长。".into()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, AppStore) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("reopen-core-")
            .tempdir_in(root)
            .unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        (dir, store)
    }

    fn legacy_fixture() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("reopen-legacy-import-")
            .tempdir_in(root)
            .unwrap();
        let path = dir.path().join("userdata.db");
        let source = rusqlite::Connection::open(&path).unwrap();
        source.execute_batch(
            "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE explorer_record (Id TEXT PRIMARY KEY, Data TEXT, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE cliphost_explorer_record (path TEXT PRIMARY KEY, update_time INTEGER NOT NULL);",
        ).unwrap();
        (dir, path, source)
    }

    #[test]
    fn reopen_legacy_import_merges_same_database_preserves_newer_native_and_survives_restart_without_trimming(
    ) {
        let (_dir, path, source) = legacy_fixture();
        for index in 0..205 {
            source.execute("INSERT INTO explorer_record(Id,Data) VALUES(?1,?2)", params![format!("old-{index}"),
                json!({"Path":format!(r"C:\Legacy\{index}"),"UpdateTime":"2026-10-01T01:00:00Z"}).to_string()]).unwrap();
        }
        for (id, path, time) in [
            (
                "existing",
                r"c:\example\keep\",
                Some("2026-10-01T09:00:00+08:00"),
            ),
            ("existing-unknown", r"C:\Example\Unknown", None),
            ("new-unknown", r"C:\Legacy\UnknownTime", None),
        ] {
            source
                .execute(
                    "INSERT INTO explorer_record(Id,Data) VALUES(?1,?2)",
                    params![
                        id,
                        json!({"Id":id,"Path":path,"UpdateTime":time}).to_string()
                    ],
                )
                .unwrap();
        }
        let newer = crate::csharp_import_document::parse_datetime(&json!("2026-10-24T01:00:00Z"))
            .unwrap()
            .timestamp_millis();
        source
            .execute(
                "INSERT INTO cliphost_explorer_record(path,update_time) VALUES(?1,?2)",
                params![r"C:\Example\Keep", newer],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO cliphost_explorer_record(path,update_time) VALUES(?1,?2)",
                params![r"C:\Example\Unknown", newer],
            )
            .unwrap();
        let read_source = || {
            source
                .prepare("SELECT Id,Data FROM explorer_record ORDER BY Id")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let original = read_source();
        let store = AppStore::open(&path).unwrap();
        let entries = store.list_explorer_history_entries().unwrap();
        assert_eq!(
            entries.len(),
            208,
            "import is complete before the first capture and never applies the default 200 cap"
        );
        for keep in [r"C:\Example\Keep", r"C:\Example\Unknown"] {
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| entry.path == keep)
                    .unwrap()
                    .recorded_at,
                newer
            );
        }
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.path == r"C:\Legacy\UnknownTime")
                .unwrap()
                .recorded_at,
            0
        );
        let selected = entries
            .into_iter()
            .filter(|entry| entry.path == r"C:\Legacy\0")
            .collect::<Vec<_>>();
        store
            .reopen_store_apply(&json!({"action":"remove","entries":selected}))
            .unwrap();
        drop(store);
        let store = AppStore::open(&path).unwrap();
        assert_eq!(store.list_explorer_history_entries().unwrap().len(), 207);
        assert!(
            !store
                .list_explorer_history_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.path == r"C:\Legacy\0"),
            "a completed import cannot resurrect a removed old record"
        );
        assert_eq!(
            read_source(),
            original,
            "the original legacy documents are byte-for-byte unchanged"
        );
        store
            .upsert_explorer_record_with_limit(r"C:\New\Capture", 3)
            .unwrap();
        assert_eq!(
            store.list_explorer_history_entries().unwrap().len(),
            3,
            "the configured cap applies on the next actual capture only"
        );
        assert_eq!(read_source(), original);
    }

    #[test]
    fn reopen_legacy_import_is_atomic_and_retries_damage_without_resurrecting_processed_rows() {
        let (_dir, path, source) = legacy_fixture();
        source
            .execute(
                "INSERT INTO explorer_record(Id,Data) VALUES('good',?1)",
                [
                    json!({"Path":r"C:\Legacy\Good","UpdateTime":"2026-10-01T01:00:00Z"})
                        .to_string(),
                ],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO explorer_record(Id,Data) VALUES('broken','{broken-json')",
                [],
            )
            .unwrap();
        source.execute_batch("CREATE TRIGGER reopen_import_failure BEFORE INSERT ON cliphost_explorer_record BEGIN SELECT RAISE(ABORT,'fixture import failure'); END;").unwrap();
        assert!(AppStore::open(&path).is_err());
        assert_eq!(
            source
                .query_row("SELECT COUNT(*) FROM cliphost_explorer_record", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            source
                .query_row(
                    "SELECT COUNT(*) FROM cliphost_kv WHERE key=?1",
                    [LEGACY_IMPORT_KEY],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0,
            "a failed transaction does not mark migration complete"
        );
        source
            .execute_batch("DROP TRIGGER reopen_import_failure")
            .unwrap();
        let store = AppStore::open(&path).unwrap();
        let state: LegacyImportState =
            serde_json::from_str(&store.kv_get(LEGACY_IMPORT_KEY).unwrap().unwrap()).unwrap();
        assert!(!state.complete);
        assert_eq!(state.malformed, 1);
        let entries = store.list_explorer_history_entries().unwrap();
        assert_eq!(entries.len(), 1);
        store
            .reopen_store_apply(&json!({"action":"remove","entries":entries}))
            .unwrap();
        assert!(
            store.list_explorer_history_entries().unwrap().is_empty(),
            "retrying a damaged row must not restore a processed record the user removed"
        );
        drop(store);
        source
            .execute(
                "UPDATE explorer_record SET Data=?1 WHERE Id='broken'",
                [
                    json!({"Path":r"C:\Legacy\Repaired","UpdateTime":"2026-10-02T01:00:00Z"})
                        .to_string(),
                ],
            )
            .unwrap();
        let store = AppStore::open(&path).unwrap();
        let entries = store.list_explorer_history_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, r"C:\Legacy\Repaired");
        let state: LegacyImportState =
            serde_json::from_str(&store.kv_get(LEGACY_IMPORT_KEY).unwrap().unwrap()).unwrap();
        assert!(state.complete);
        assert_eq!(state.processed_ids.len(), 2);
        assert_eq!(
            source
                .query_row(
                    "SELECT Data FROM explorer_record WHERE Id='good'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            json!({"Path":r"C:\Legacy\Good","UpdateTime":"2026-10-01T01:00:00Z"}).to_string()
        );
    }

    #[test]
    fn reopen_remove_snapshot_does_not_delete_newer_same_path_and_undo_preserves_new_records() {
        let (_dir, store) = fixture();
        store
            .upsert_explorer_record_at(r"C:\Example\A", 200, 1000)
            .unwrap();
        let old = store.list_explorer_history_entries().unwrap();
        store
            .upsert_explorer_record_at(r"C:\Example\A", 200, 1001)
            .unwrap();
        let stale = store
            .reopen_store_apply(&json!({"action":"remove","entries":old}))
            .unwrap();
        assert_eq!((stale.changed, stale.skipped), (0, 1));
        let current = store.list_explorer_history_entries().unwrap();
        let removed = store
            .reopen_store_apply(&json!({"action":"remove","entries":current}))
            .unwrap();
        assert_eq!(removed.changed, 1);
        store
            .upsert_explorer_record_at(r"c:\example\a\", 200, 1002)
            .unwrap();
        let restored = store
            .reopen_store_apply(&json!({"action":"restore","undo":removed.undo}))
            .unwrap();
        assert_eq!((restored.changed, restored.skipped), (0, 1));
        assert_eq!(restored.snapshot.entries.len(), 1);
        assert_eq!(restored.snapshot.entries[0].recorded_at, 1002);
    }

    #[test]
    fn reopen_clear_uses_fixed_snapshot_and_transaction_failure_restores_the_whole_batch() {
        let (_dir, store) = fixture();
        for (index, path) in ["shell:::one", "shell:::two"].iter().enumerate() {
            store
                .upsert_explorer_record_at(path, 200, index as i64)
                .unwrap();
        }
        let selected = store.list_explorer_history_entries().unwrap();
        store
            .upsert_explorer_record_at("shell:::later", 200, 10)
            .unwrap();
        store.conn.execute_batch("CREATE TEMP TRIGGER reopen_test_failure BEFORE DELETE ON explorer_record WHEN OLD.path='shell:::one' BEGIN SELECT RAISE(ABORT,'fixture write failure'); END;").unwrap();
        assert!(store
            .reopen_store_apply(&json!({"action":"remove","entries":selected}))
            .is_err());
        assert_eq!(store.list_explorer_history_entries().unwrap().len(), 3);
        assert!(store.kv_get(UNDO_KEY).unwrap().is_none());
        store
            .conn
            .execute_batch("DROP TRIGGER reopen_test_failure")
            .unwrap();
        let result = store
            .reopen_store_apply(&json!({"action":"remove","entries":selected}))
            .unwrap();
        assert_eq!(result.changed, 2);
        assert_eq!(result.snapshot.entries[0].path, "shell:::later");
        let restored = store
            .reopen_store_apply(&json!({"action":"restore","undo":result.undo}))
            .unwrap();
        assert_eq!(restored.snapshot.entries.len(), 3);
    }

    #[test]
    fn reopen_exclusions_persist_and_block_commits_without_hiding_current_history() {
        let (dir, store) = fixture();
        store
            .upsert_explorer_record_at(r"C:\Example\Docs", 200, 1)
            .unwrap();
        let rule = store
            .reopen_store_apply(
                &json!({"action":"exclude","path":r"C:\Example\Docs","includeChildren":true}),
            )
            .unwrap();
        assert_eq!(
            rule.snapshot.entries.len(),
            1,
            "rule defaults to preserving old history"
        );
        store
            .upsert_explorer_record_at(r"c:\example\docs\child", 1, 2)
            .unwrap();
        assert_eq!(
            store.list_explorer_history_entries().unwrap().len(),
            1,
            "excluded event must not trigger cap cleanup"
        );
        store
            .upsert_explorer_record_at(r"C:\Example\Docs-old", 200, 3)
            .unwrap();
        assert_eq!(store.list_explorer_history_entries().unwrap().len(), 2);
        drop(store);
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let rule = store.list_explorer_exclusions().unwrap().remove(0);
        let result = store
            .reopen_store_apply(
                &json!({"action":"unexclude","path":rule.path,"revision":rule.revision}),
            )
            .unwrap();
        assert!(result.snapshot.exclusions.is_empty());
        store
            .upsert_explorer_record_at(r"C:\Example\Docs\child", 200, 4)
            .unwrap();
        assert_eq!(store.list_explorer_history_entries().unwrap().len(), 3);
    }

    #[test]
    fn reopen_exact_rules_undo_replaced_rule_and_retire_old_receipts_on_next_mutation() {
        let (_dir, store) = fixture();
        store
            .upsert_explorer_record_at(r"C:\Example\Docs", 200, 1)
            .unwrap();
        let selected = store.list_explorer_history_entries().unwrap();
        let exact = store
            .reopen_store_apply(&json!({"action":"exclude","path":r"C:\Example\Docs"}))
            .unwrap();
        store
            .upsert_explorer_record_at(r"C:\Example\Docs\child", 200, 2)
            .unwrap();
        assert_eq!(store.list_explorer_history_entries().unwrap().len(), 2);
        let replaced = store.reopen_store_apply(&json!({"action":"exclude","path":r"c:\example\docs\","includeChildren":true,"removeExisting":true,"entries":selected})).unwrap();
        assert_eq!(replaced.changed, 1);
        assert!(store
            .reopen_store_apply(&json!({"action":"restore","undo":exact.undo}))
            .is_err());
        let restored = store
            .reopen_store_apply(&json!({"action":"restore","undo":replaced.undo}))
            .unwrap();
        assert!(!restored.snapshot.exclusions[0].include_children);
        assert_eq!(
            restored.skipped, 1,
            "the previous exact rule still excludes the removed directory"
        );
        assert_eq!(restored.snapshot.entries[0].path, r"C:\Example\Docs\child");
        assert!(store
            .reopen_store_apply(
                &json!({"action":"exclude","path":"shell:::{virtual}","includeChildren":true})
            )
            .is_err());
    }

    #[test]
    fn reopen_new_case_variant_reuses_original_identity_without_deleting_legacy_alias_rows() {
        let (_dir, store) = fixture();
        store
            .upsert_explorer_record_at(r"C:\Example\Docs", 200, 1)
            .unwrap();
        store
            .upsert_explorer_record_at(r"c:\example\docs\", 200, 2)
            .unwrap();
        let rows = store.list_explorer_history_entries().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, r"C:\Example\Docs");
        store
            .conn
            .execute(
                "INSERT INTO explorer_record(path,update_time) VALUES(?1,?2)",
                params![r"C:\EXAMPLE\DOCS", 3],
            )
            .unwrap();
        store
            .upsert_explorer_record_at(r"c:\Example\Docs", 200, 4)
            .unwrap();
        assert_eq!(
            store.list_explorer_history_entries().unwrap().len(),
            2,
            "pre-existing aliases remain explicit manageable records"
        );
        let selected = store.list_explorer_history_entries().unwrap();
        let removed = store
            .reopen_store_apply(&json!({"action":"remove","entries":selected}))
            .unwrap();
        let restored = store
            .reopen_store_apply(&json!({"action":"restore","undo":removed.undo}))
            .unwrap();
        assert_eq!(
            restored.changed, 2,
            "undo preserves the original aliases when neither has been replaced by a newer record"
        );
    }

    #[test]
    fn reopen_history_transactions_work_in_document_storage_without_rewriting_clipboard_documents()
    {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("reopen-documents-")
            .tempdir_in(root)
            .unwrap();
        let path = dir.path().join("userdata.db");
        rusqlite::Connection::open(&path).unwrap().execute_batch(
            "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);
             CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);",
        ).unwrap();
        let store = AppStore::open(&path).unwrap();
        let clip = crate::model::ClipRecord::new_text("synthetic clipboard fixture", "fixture.exe");
        store.insert_clip(&clip, true).unwrap();
        let read_document = || {
            rusqlite::Connection::open(&path)
                .unwrap()
                .query_row(
                    "SELECT Data FROM clip_item WHERE Id=?1",
                    [&clip.id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        let before = read_document();
        store
            .upsert_explorer_record_at(r"\\example.invalid\share\folder", 200, 1)
            .unwrap();
        let selected = store.list_explorer_history_entries().unwrap();
        let excluded = store.reopen_store_apply(&json!({"action":"exclude","path":r"\\example.invalid\share\folder","removeExisting":true,"entries":selected})).unwrap();
        assert!(excluded.snapshot.entries.is_empty());
        let restored = store
            .reopen_store_apply(&json!({"action":"restore","undo":excluded.undo}))
            .unwrap();
        assert_eq!(restored.changed, 1);
        assert!(restored.snapshot.exclusions.is_empty());
        assert_eq!(
            read_document(),
            before,
            "history management must not write clipboard documents"
        );
    }
}
