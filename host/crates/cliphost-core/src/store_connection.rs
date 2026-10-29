//! In-place C# document storage. Relational indexes live only in an attached
//! memory database; original document tables and image paths remain intact.
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{functions::FunctionFlags, Connection, Params, Row, Statement, Transaction, TransactionBehavior};
use serde_json::Value;
use std::{
    cell::Cell,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[path = "legacy_explorer_source.rs"]
mod legacy_explorer_source;

const CACHE: &[&str] = &["clip_item", "favor_item", "usage_info"];
const AUX: &[&str] = &[
    "text_action",
    "lan_peer",
    "lan_conversation",
    "lan_message",
    "collect_draft",
    "kv",
    "explorer_record",
    "process_icon_meta",
];
fn invalid(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(Box::new(std::io::Error::other(message.into())))
}
pub(crate) struct StoreConnection {
    inner: Connection,
    documents: Option<DocumentCache>,
    core: Option<crate::storage_core::Runtime>,
}
struct DocumentCache {
    image_root: std::path::PathBuf,
    /// Set between `mark_projection_pending` and a successful initialization;
    /// a failed projection keeps it with the error so the empty cache is never used.
    pending: std::cell::RefCell<Option<String>>,
    loading: Arc<AtomicBool>,
    ready: Cell<bool>,
    version: Cell<i64>,
}

impl StoreConnection {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::open_with_core(path, false)
    }
    pub fn open_isolated_core(path: &Path) -> rusqlite::Result<Self> {
        Self::open_with_core(path, true)
    }
    fn open_with_core(path: &Path, allow_core: bool) -> rusqlite::Result<Self> {
        let mut inner = Connection::open(path)?;
        inner.busy_timeout(Duration::from_secs(2))?;
        let core_marker: bool = inner.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='store_info')", [], |r|r.get(0))?;
        if core_marker {
            if !allow_core { crate::storage_v2::reject_unsupported_runtime(&inner).map_err(invalid)?; return Err(invalid("STORAGE_CORE_ISOLATED_ONLY: core candidates require an explicit isolated opening; production activation is not implemented.")); }
            let core=crate::storage_core::Runtime::open(&inner,path).map_err(invalid)?;
            return Ok(Self {inner,documents:None,core:Some(core)});
        }
        if allow_core { return Err(invalid("The explicitly selected core candidate has no supported core format marker.")); }
        crate::storage_v2::reject_unsupported_runtime(&inner).map_err(invalid)?;
        match inspect_storage_schema(&inner, path)? {
            StorageSchema::Native => return Ok(Self { inner, documents: None, core: None }),
            StorageSchema::Documents(None) => {}
            StorageSchema::Documents(Some(_)) => {
                // C# creates each document table when its service is first
                // constructed. Recheck every rejection condition while holding
                // the writer reservation, before adding the unused empty table.
                let transaction = inner.transaction_with_behavior(TransactionBehavior::Immediate)?;
                match inspect_storage_schema(&transaction, path)? {
                    StorageSchema::Documents(Some(table)) => {
                        transaction.execute_batch(&format!(
                            "CREATE TABLE main.\"{table}\" (Id NVARCHAR(255) PRIMARY KEY NOT NULL,
                             Data TEXT, ObjectType NVARCHAR(100), CreatedTime DATETIME NOT NULL,
                             UpdateTime DATETIME NOT NULL);"
                        ))?;
                    }
                    StorageSchema::Documents(None) => {}
                    StorageSchema::Native => return Err(invalid("数据库格式在启动期间发生变化；未补建文档表。")),
                }
                transaction.commit()?;
            }
        }
        let loading = Arc::new(AtomicBool::new(true));
        let image_root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("Image");
        let write_image_root = image_root.clone();
        let cache_image_root = image_root.clone();
        let flag = loading.clone();
        inner.create_scalar_function(
            "cliphost_cache_loading",
            0,
            FunctionFlags::SQLITE_UTF8,
            move |_| Ok(flag.load(Ordering::Relaxed) as i32),
        )?;
        let pure = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
        inner.create_scalar_function("cliphost_document", 3, pure, move |c| {
            let table: String = c.get(0)?;
            let record: String = c.get(1)?;
            let previous: Option<String> = c.get(2)?;
            let mut record = parse(&record)?;
            let previous = previous.map(|v| parse(&v)).transpose()?;
            if let Some(previous) = &previous {
                preserve_image_reference(&table, &mut record, previous, &write_image_root)?;
            }
            crate::csharp_store_document::write_document(&table, &record, previous.as_ref())
                .and_then(|v| serde_json::to_string(&v).map_err(|e| e.to_string()))
                .map_err(invalid)
        })?;
        inner.create_scalar_function("cliphost_usage_document", 4, pure, |c| {
            let id: String = c.get(0)?;
            let ms: Option<i64> = c.get(1)?;
            let count: i32 = c.get(2)?;
            let previous: Option<String> = c.get(3)?;
            let previous = previous.map(|v| parse(&v)).transpose()?;
            crate::csharp_store_document::write_usage(
                &id,
                ms.filter(|ms| *ms != 0 || count != 0)
                    .and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
                count,
                previous.as_ref(),
            )
            .and_then(|v| serde_json::to_string(&v).map_err(|e| e.to_string()))
            .map_err(invalid)
        })?;
        inner.create_scalar_function("cliphost_read_document", 3, pure, move |c| {
            let table: String = c.get(0)?;
            let id: String = c.get(1)?;
            let raw: String = c.get(2)?;
            let value = parse(&raw)?;
            let value = match table.as_str() {
                "clip_item" => {
                    let mut item=crate::csharp_store_document::parse_clip(&id,&value).map_err(invalid)?;
                    resolve_image_reference(&mut item.image_path,&image_root);
                    serde_json::to_value(item)
                }
                "favor_item" => {
                    let mut item=crate::csharp_store_document::parse_favor(&id,&value).map_err(invalid)?;
                    resolve_image_reference(&mut item.image_path,&image_root);
                    serde_json::to_value(item)
                }
                _ => {
                    let usage = crate::csharp_store_document::parse_usage(&id, &value).map_err(invalid)?;
                    Ok(serde_json::json!({"lastUseTime":usage.last_use_time,"usageCount":usage.usage_count}))
                }
            }.map_err(|e| invalid(e.to_string()))?;
            serde_json::to_string(&value).map_err(|e| invalid(e.to_string()))
        })?;
        inner.create_scalar_function("cliphost_timestamp_ms", 1, pure, |c| {
            let date: Option<String> = c.get(0)?;
            date.map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|d| d.timestamp_millis())
                    .map_err(|e| invalid(e.to_string()))
            })
            .transpose()
        })?;
        inner.create_scalar_function("cliphost_document_revision", 2, pure, |c| {
            let data: String = c.get(0)?;
            let token: Option<String> = c.get(1)?;
            Ok(document_revision(&data, token.as_deref()))
        })?;
        inner.execute_batch("ATTACH DATABASE ':memory:' AS rust;
            CREATE TABLE rust.cliphost_document_errors (
                source TEXT NOT NULL, id TEXT NOT NULL, reason TEXT NOT NULL);")?;
        Ok(Self {
            inner,
            core: None,
            documents: Some(DocumentCache {
                image_root: cache_image_root,
                pending: std::cell::RefCell::new(None),
                loading,
                ready: Cell::new(false),
                version: Cell::new(-1),
            }),
        })
    }
    pub fn mark_projection_pending(&self) {
        if let Some(cache) = &self.documents {
            *cache.pending.borrow_mut() = Some("剪贴板历史尚未加载完成。".into());
        }
    }
    pub fn source_clip_count(&self) -> rusqlite::Result<i64> {
        if self.core.is_some() { return self.inner.query_row("SELECT count(*) FROM items WHERE scope='history'",[],|r|r.get(0)); }
        self.inner.query_row("SELECT count(*) FROM main.\"clip_item\"", [], |r| r.get(0))
    }
    pub(crate) fn core(&self)->Option<(&Connection,&crate::storage_core::Runtime)> { self.core.as_ref().map(|runtime|(&self.inner,runtime)) }
    pub fn is_core_store(&self)->bool { self.core.is_some() }
    fn ensure_projected(&self) -> rusqlite::Result<()> {
        match self.documents.as_ref().and_then(|c| c.pending.borrow().clone()) {
            Some(message) => Err(invalid(message)),
            None => Ok(()),
        }
    }
    pub fn change_stamp(&self) -> rusqlite::Result<(u64, i64)> {
        Ok((self.inner.total_changes(), self.version()?))
    }
    pub fn is_document_store(&self) -> bool {
        self.documents.is_some()
    }
    pub fn revision_column(&self) -> &'static str {
        if self.documents.is_some() {
            "legacy_revision"
        } else {
            "NULL"
        }
    }
    pub fn verify_revision(
        &self,
        table: &str,
        id: &str,
        expected: Option<&str>,
    ) -> rusqlite::Result<()> {
        if self.documents.is_none() {
            return Ok(());
        }
        let expected =
            expected.ok_or_else(|| invalid("旧库条目缺少原始修订信息，请重新读取后保存。"))?;
        if !["clip_item", "favor_item"].contains(&table) {
            return Err(invalid("Invalid document table"));
        }
        let token = if has(&columns(&self.inner, table)?, "RowToken") {
            "RowToken"
        } else {
            "NULL"
        };
        let actual: String = self.inner.query_row(
            &format!(
                "SELECT cliphost_document_revision(Data,{token}) FROM main.\"{table}\" WHERE Id=?1"
            ),
            [id],
            |r| r.get(0),
        )?;
        if actual != expected {
            return Err(invalid("数据已由其他程序修改，请重新读取后保存。"));
        }
        Ok(())
    }
    /// A favorite copy inherits unmodeled C# fields as well as the Rust payload.
    /// Called only inside the drag transaction after inserting the new row.
    pub(crate) fn inherit_favorite_document(
        &self,
        source_table: &str,
        source_id: &str,
        item: &crate::model::FavorRecord,
    ) -> rusqlite::Result<()> {
        if let Some(core)=&self.core { return core.inherit(&self.inner,source_table,source_id,item).map_err(invalid); }
        if self.documents.is_none() { return Ok(()); }
        if !["clip_item", "favor_item"].contains(&source_table) {
            return Err(invalid("Invalid drag document source"));
        }
        self.before_write()?;
        let raw: String = self.inner.query_row(&format!("SELECT Data FROM main.\"{source_table}\" WHERE Id=?1"),
            [source_id], |row| row.get(0))?;
        let mut previous = parse(&raw)?;
        let object = previous.as_object_mut().ok_or_else(|| invalid("Invalid drag source document"))?;
        let id_key = object.keys().find(|key| key.eq_ignore_ascii_case("Id")).cloned().unwrap_or_else(|| "Id".into());
        object.insert(id_key, Value::String(item.id.clone()));
        let record = serde_json::to_value(item).map_err(|e| invalid(e.to_string()))?;
        let document = crate::csharp_store_document::write_document("favor_item", &record, Some(&previous))
            .map_err(invalid)?;
        let encoded = serde_json::to_string(&document).map_err(|e| invalid(e.to_string()))?;
        let gate = !columns(&self.inner, "__clip_versioned_write_gate")?.is_empty();
        if gate {
            self.inner.execute("UPDATE __clip_versioned_write_gate SET Permit=1 WHERE TableName='favor_item'", [])?;
        }
        let names = columns(&self.inner, "favor_item")?;
        let token = has(&names, "RowToken");
        let token_update = if token { ", RowToken=lower(hex(randomblob(16)))" } else { "" };
        let result = self.inner.execute(&format!("UPDATE main.favor_item SET Data=?2{token_update} WHERE Id=?1"),
            rusqlite::params![item.id, encoded]);
        if gate {
            self.inner.execute("UPDATE __clip_versioned_write_gate SET Permit=0 WHERE TableName='favor_item'", [])?;
        }
        if result? != 1 { return Err(invalid("Copied favorite is missing")); }
        // Changing legacy_data intentionally bypasses the cache update trigger;
        // the modeled columns already hold this exact item, only extensions vary.
        let token_expr = if token { "RowToken" } else { "NULL" };
        self.inner.execute(&format!(
            "UPDATE rust.cliphost_cache_favor_item SET legacy_data=(SELECT Data FROM main.favor_item WHERE Id=?1),
             legacy_token=(SELECT {token_expr} FROM main.favor_item WHERE Id=?1),
             legacy_revision=(SELECT cliphost_document_revision(Data,{token_expr}) FROM main.favor_item WHERE Id=?1) WHERE id=?1"),
            [&item.id])?;
        Ok(())
    }
    pub fn finish_initialization(&self) -> rusqlite::Result<()> {
        let Some(cache) = &self.documents else {
            return Ok(());
        };
        if cache.ready.get() {
            return self.before_read();
        }
        let result = (|| {
            for table in CACHE {
                self.inner.execute_batch(&format!(
                    "ALTER TABLE rust.cliphost_cache_{table} ADD COLUMN legacy_data TEXT;
                     ALTER TABLE rust.cliphost_cache_{table} ADD COLUMN legacy_token TEXT;
                     ALTER TABLE rust.cliphost_cache_{table} ADD COLUMN legacy_revision TEXT;"
                ))?;
            }
            if columns(&self.inner, "UsageInfo")?.is_empty() {
                self.inner.execute_batch(
                    "CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY NOT NULL, Data TEXT,
                    ObjectType TEXT, CreatedTime DATETIME NOT NULL, UpdateTime DATETIME NOT NULL);",
                )?;
            }
            self.reload_cache(false)?;
            self.install_triggers()
        })();
        if let Err(error) = &result {
            // Keep the store unusable: an empty projection without triggers must
            // never be read as an empty history or accept unsynchronized writes.
            *cache.pending.borrow_mut() = Some(format!("剪贴板历史加载失败：{error}"));
            return result;
        }
        cache.loading.store(false, Ordering::Relaxed);
        cache.ready.set(true);
        *cache.pending.borrow_mut() = None;
        Ok(())
    }
    fn version(&self) -> rusqlite::Result<i64> {
        self.inner
            .query_row("PRAGMA main.data_version", [], |r| r.get(0))
    }
    fn before_read(&self) -> rusqlite::Result<()> {
        self.ensure_projected()?;
        if let Some(c) = &self.documents {
            if c.ready.get() && self.inner.is_autocommit() && self.version()? != c.version.get() {
                // Another writer committed: re-project only rows whose original document changed.
                self.reload_cache(true)?;
            }
        }
        Ok(())
    }
    fn before_write(&self) -> rusqlite::Result<()> {
        self.ensure_projected()?;
        if let Some(c) = &self.documents {
            if c.ready.get() && self.inner.is_autocommit() && self.version()? != c.version.get() {
                // Never rebase a previously read edit onto a concurrent change.
                return Err(invalid("数据已由其他程序修改，请刷新后重试。"));
            }
            if self.has_isolated_documents()? {
                return Err(invalid("DATABASE_RECOVERY_READ_ONLY: 部分旧记录无法读取，数据库已进入只读保护；原始数据已保留，请修复后重启。"));
            }
        }
        Ok(())
    }
    fn has_isolated_documents(&self) -> rusqlite::Result<bool> {
        if self.documents.is_none() { return Ok(false); }
        self.inner.query_row("SELECT EXISTS(SELECT 1 FROM rust.cliphost_document_errors)", [], |r| r.get(0))
    }
    pub fn document_recovery_status(&self) -> rusqlite::Result<Value> {
        self.before_read()?;
        let mut issues = Vec::new();
        if self.documents.is_some() {
            let mut statement = self.inner.prepare("SELECT source,id,reason FROM rust.cliphost_document_errors ORDER BY source,id")?;
            let rows = statement.query_map([], |row| Ok(serde_json::json!({
                "table": row.get::<_, String>(0)?, "id": row.get::<_, String>(1)?,
                "reason": row.get::<_, String>(2)?
            })))?;
            for row in rows { issues.push(row?); }
        }
        Ok(serde_json::json!({"readOnly": !issues.is_empty(), "isolatedRows": issues.len(), "issues": issues}))
    }
    pub fn execute<P: Params>(&self, sql: &str, p: P) -> rusqlite::Result<usize> {
        self.before_write()?;
        if let Some(core)=&self.core {
            let _lease=core.writer.enter(crate::storage_core::WriteScope::Auxiliary).map_err(invalid)?;
            return self.inner.execute(sql,p);
        }
        self.inner.execute(&self.sql(sql), p)
    }
    pub fn execute_batch(&self, sql: &str) -> rusqlite::Result<()> {
        self.before_write()?;
        if let Some(core)=&self.core {
            let _lease=core.writer.enter(crate::storage_core::WriteScope::Auxiliary).map_err(invalid)?;
            return self.inner.execute_batch(sql);
        }
        self.inner.execute_batch(&self.sql(sql))
    }
    pub fn prepare(&self, sql: &str) -> rusqlite::Result<Statement<'_>> {
        self.before_read()?;
        self.inner.prepare(&self.sql(sql))
    }
    pub fn query_row<T, P: Params, F: FnOnce(&Row<'_>) -> rusqlite::Result<T>>(
        &self,
        sql: &str,
        p: P,
        map: F,
    ) -> rusqlite::Result<T> {
        self.before_read()?;
        self.inner.query_row(&self.sql(sql), p, map)
    }
    pub fn unchecked_transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        self.before_read()?;
        self.start_snapshot_transaction()
    }
    fn start_snapshot_transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        let transaction = self.inner.unchecked_transaction()?;
        if let Some(cache) = &self.documents {
            // BEGIN DEFERRED alone does not fix main's snapshot. The projection
            // lives in the attached memory database, so reading only its rows
            // would miss a writer between before_read and BEGIN. Read main
            // first, then require its external version to match the projection.
            let _: i64 = self.inner.query_row(
                "SELECT count(*) FROM main.sqlite_master", [], |row| row.get(0),
            )?;
            if self.version()? != cache.version.get() {
                return Err(invalid("WRITE_CONFLICT:数据在开始事务前已由其他程序修改，请刷新后重试。"));
            }
        }
        Ok(transaction)
    }
    #[cfg(test)]
    pub fn busy_handler(&self, handler: Option<fn(i32) -> bool>) -> rusqlite::Result<()> {
        self.inner.busy_handler(handler)
    }
    fn sql(&self, sql: &str) -> String {
        if self.documents.is_some() {
            rewrite_sql(sql)
        } else {
            sql.into()
        }
    }

    /// Project the original documents into the memory tables. `changed_only`
    /// keeps rows whose original `Data`/token are unchanged (and their rowids),
    /// removes vanished rows and re-projects the rest.
    fn reload_cache(&self, changed_only: bool) -> rusqlite::Result<()> {
        let Some(cache) = &self.documents else {
            return Ok(());
        };
        cache.loading.store(true, Ordering::Relaxed);
        let result = (|| {
            self.inner.execute_batch("BEGIN DEFERRED;")?;
            let version = self.version()?;
            let result = (|| {
                self.inner.execute_batch("DELETE FROM rust.cliphost_document_errors;")?;
                for (source, target) in [("clip_item", "clip_item"), ("favor_item", "favor_item"), ("UsageInfo", "usage_info")] {
                    self.inner.execute_batch(&if changed_only {
                        format!("DELETE FROM rust.cliphost_cache_{target} AS c WHERE NOT EXISTS (SELECT 1 FROM main.\"{source}\" m WHERE m.Id=c.id);")
                    } else {
                        format!("DELETE FROM rust.cliphost_cache_{target};")
                    })?;
                }
                for (source, target, projection) in [
                    ("clip_item", "clip_item", clip_projection()),
                    ("favor_item", "favor_item", favor_projection()),
                    ("UsageInfo", "usage_info", vec![("id","Id".into()),
                        ("last_use_time","COALESCE(cliphost_timestamp_ms(json_extract(decoded,'$.lastUseTime')),0)".into()),
                        ("usage_count","json_extract(decoded,'$.usageCount')".into())]),
                ] {
                    self.project_documents(source, target, projection, changed_only, &cache.image_root)?;
                }
                Ok::<_, rusqlite::Error>(())
            })();
            match result {
                Ok(()) => {
                    self.inner.execute_batch("COMMIT;")?;
                    cache.version.set(version);
                    Ok(())
                }
                Err(e) => {
                    let _ = self.inner.execute_batch("ROLLBACK;");
                    Err(e)
                }
            }
        })();
        cache.loading.store(false, Ordering::Relaxed);
        result
    }
    /// Only document decoding is recoverable here. SQLite read/transaction/write
    /// failures still abort the snapshot; they must never masquerade as bad rows.
    fn project_documents(&self, source: &str, target: &str, projection: Vec<(&str, String)>, changed_only: bool, image_root: &Path) -> rusqlite::Result<()> {
        use rusqlite::types::Value as Sql;
        let token = if has(&columns(&self.inner, source)?, "RowToken") { "RowToken" } else { "NULL" };
        let changed = if changed_only { unchanged_filter(target, token) } else { String::new() };
        let rows = {
            let mut statement = self.inner.prepare(&format!(
                "SELECT Id,Data,{token} FROM main.\"{source}\" m WHERE true {changed}"))?;
            // SQL NULL/BLOB/numeric cells are malformed documents too. Read the
            // cell without conversion so one such row cannot abort all others.
            let rows = statement.query_map([], |r| Ok((r.get::<_, Sql>(0)?, r.get::<_, Sql>(1)?, r.get::<_, Sql>(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() { return Ok(()); }
        let decode = |(id, data, token): &(Sql, Sql, Sql)| -> Result<Vec<Sql>, &'static str> {
            let Sql::Text(id) = id else { return Err("INVALID_ID_CELL"); };
            let Sql::Text(data) = data else { return Err("INVALID_DOCUMENT_CELL"); };
            let token = match token { Sql::Text(t) => Some(t.as_str()), Sql::Null => None, _ => return Err("INVALID_REVISION_CELL") };
            if target == "clip_item" {
                return project_clip_document(id, data, token, image_root).map_err(|_| "INVALID_HISTORY_DOCUMENT");
            }
            let raw: Value = serde_json::from_str(data).map_err(|_| "INVALID_JSON")?;
            let decoded = if target == "favor_item" {
                let mut item = crate::csharp_store_document::parse_favor(id, &raw).map_err(|_| "INVALID_FAVORITE_DOCUMENT")?;
                resolve_image_reference(&mut item.image_path, image_root);
                serde_json::to_value(item).map_err(|_| "INVALID_FAVORITE_DOCUMENT")?
            } else {
                let usage = crate::csharp_store_document::parse_usage(id, &raw).map_err(|_| "INVALID_USAGE_DOCUMENT")?;
                serde_json::json!({"lastUseTime":usage.last_use_time,"usageCount":usage.usage_count})
            };
            // Favor/usage column extraction is executed below, after decoding;
            // a SQLite error there remains a real database error.
            Ok(vec![Sql::Text(decoded.to_string()), Sql::Text(id.clone()), Sql::Text(data.clone()),
                token.map_or(Sql::Null, |t| Sql::Text(t.into()))])
        };
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(rows.len().div_ceil(256)).max(1);
        let projected: Vec<Result<Vec<Sql>, &'static str>> = std::thread::scope(|scope| {
            let workers: Vec<_> = rows.chunks(rows.len().div_ceil(threads))
                .map(|chunk| { let decode = &decode; scope.spawn(move || chunk.iter().map(decode).collect::<Vec<_>>()) })
                .collect();
            workers.into_iter().map(|w| w.join().map_err(|_| invalid("Document projection worker panicked")))
                .collect::<rusqlite::Result<Vec<_>>>().map(|parts| parts.into_iter().flatten().collect())
        })?;
        let names: Vec<_> = projection.iter().map(|p| p.0)
            .chain(["legacy_data", "legacy_token", "legacy_revision"]).collect();
        let updates = names.iter().filter(|n| **n != "id").map(|n| format!("{n}=excluded.{n}")).collect::<Vec<_>>().join(",");
        let mut insert = self.inner.prepare(&format!(
            "INSERT INTO rust.cliphost_cache_{target}({}) VALUES({}) ON CONFLICT(id) DO UPDATE SET {updates}",
            names.join(","), vec!["?"; names.len()].join(",")))?;
        let expressions = projection.iter().map(|p| p.1.as_str()).chain([
            "Data", "token", "cliphost_document_revision(Data,token)"
        ]).collect::<Vec<_>>().join(",");
        let mut extract = self.inner.prepare(&format!(
            "WITH record(decoded,Id,Data,token) AS (VALUES (?1,?2,?3,?4)) SELECT {expressions} FROM record"))?;
        for (index, (row, result)) in rows.iter().zip(projected).enumerate() {
            let values = match result {
                Ok(values) if target == "clip_item" => values,
                Ok(values) => extract.query_row(rusqlite::params_from_iter(values), |r| (0..names.len()).map(|n| r.get::<_, Sql>(n)).collect::<rusqlite::Result<Vec<_>>>())?,
                Err(reason) => {
                    let id = match &row.0 { Sql::Text(id) => id.clone(), _ => format!("<invalid Id #{}>", index + 1) };
                    // A formerly valid row can become invalid in another process.
                    // Remove its stale projection without touching the source.
                    if let Sql::Text(source_id) = &row.0 {
                        self.inner.execute(&format!("DELETE FROM rust.cliphost_cache_{target} WHERE id=?1"), [source_id])?;
                    }
                    self.inner.execute("INSERT INTO rust.cliphost_document_errors(source,id,reason) VALUES(?1,?2,?3)", rusqlite::params![source,id,reason])?;
                    tracing::warn!(table=source, record_id=%id, reason, "Original document isolated; database protected from writes");
                    continue;
                }
            };
            insert.execute(rusqlite::params_from_iter(values))?;
        }
        Ok(())
    }
    fn install_triggers(&self) -> rusqlite::Result<()> {
        // Also cover direct transaction SQL, so a partial projection can never
        // be used to mutate a graph, reset usage, or replace a whole collection.
        for table in CACHE.iter().map(|name| format!("rust.cliphost_cache_{name}"))
            .chain(["clip_item", "favor_item", "UsageInfo"].iter().map(|name| format!("main.{name}")))
            .chain(AUX.iter().map(|name| format!("main.cliphost_{name}"))) {
            let name = table.replace('.', "_");
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                self.inner.execute_batch(&format!(
                    "CREATE TEMP TRIGGER cliphost_recovery_{name}_{operation} BEFORE {operation} ON {table}
                     WHEN cliphost_cache_loading()=0 AND EXISTS(SELECT 1 FROM rust.cliphost_document_errors)
                     BEGIN SELECT RAISE(ABORT,'DATABASE_RECOVERY_READ_ONLY'); END;"))?;
            }
        }
        let gate = !columns(&self.inner, "__clip_versioned_write_gate")?.is_empty();
        for (cache_name, source, record) in [
            ("clip_item", "clip_item", "NEW.extra_json".to_string()),
            ("favor_item", "favor_item", favor_json()),
            ("usage_info", "UsageInfo", String::new()),
        ] {
            let names = columns(&self.inner, source)?;
            let token = has(&names, "RowToken");
            let target = format!("cliphost_cache_{cache_name}");
            let serialize = if cache_name == "usage_info" {
                format!("cliphost_usage_document(NEW.id,NEW.last_use_time,NEW.usage_count,(SELECT Data FROM \"{source}\" WHERE Id=NEW.id))")
            } else {
                format!("cliphost_document('{cache_name}',{record},(SELECT Data FROM \"{source}\" WHERE Id=NEW.id))")
            };
            let mut insert_columns = vec!["Id".to_string(), "Data".into()];
            let mut insert_values = vec!["NEW.id".to_string(), serialize.clone()];
            let mut updates = vec![format!("Data={serialize}")];
            for name in ["CreatedTime", "UpdateTime", "ObjectType", "RowToken"] {
                if !has(&names, name) {
                    continue;
                }
                insert_columns.push(name.into());
                let value = match name {
                    "RowToken" => "lower(hex(randomblob(16)))",
                    "ObjectType" => "NULL",
                    _ => "strftime('%Y-%m-%d %H:%M:%f','now','localtime')",
                };
                insert_values.push(value.into());
                if ["UpdateTime", "RowToken"].contains(&name) {
                    updates.push(format!("\"{name}\"={value}"));
                }
            }
            let begin = if gate && cache_name != "usage_info" {
                format!(
                    "UPDATE __clip_versioned_write_gate SET Permit=1 WHERE TableName='{source}';"
                )
            } else {
                String::new()
            };
            let end = if begin.is_empty() {
                String::new()
            } else {
                format!(
                    "UPDATE __clip_versioned_write_gate SET Permit=0 WHERE TableName='{source}';"
                )
            };
            let cas = if token {
                " AND RowToken IS OLD.legacy_token"
            } else {
                ""
            };
            let source_token = if token { "RowToken" } else { "NULL" };
            let revision = format!("(SELECT cliphost_document_revision(Data,{source_token}) FROM \"{source}\" WHERE Id=NEW.id)");
            let extra = if cache_name == "clip_item" {
                format!(", extra_json=json_set(extra_json,'$.storageRevision',{revision})")
            } else {
                String::new()
            };
            let sync = format!(
                "UPDATE {target} SET legacy_data=(SELECT Data FROM \"{source}\" WHERE Id=NEW.id),
                legacy_token=(SELECT {source_token} FROM \"{source}\" WHERE Id=NEW.id),
                legacy_revision={revision}{extra} WHERE id=NEW.id;"
            );
            self.inner.execute_batch(&format!(
                "CREATE TEMP TRIGGER cliphost_{cache_name}_insert AFTER INSERT ON rust.{target}
                 WHEN cliphost_cache_loading()=0 BEGIN {begin}
                   INSERT INTO \"{source}\" ({}) VALUES({}); {end} {sync} END;
                 CREATE TEMP TRIGGER cliphost_{cache_name}_update AFTER UPDATE ON rust.{target}
                 WHEN cliphost_cache_loading()=0 AND NEW.legacy_data IS OLD.legacy_data BEGIN {begin}
                   UPDATE \"{source}\" SET {} WHERE Id=OLD.id AND Data IS OLD.legacy_data{cas};
                   SELECT CASE WHEN changes()<>1 THEN RAISE(ABORT,'original data changed; refresh and retry') END;
                   {end} {sync} END;
                 CREATE TEMP TRIGGER cliphost_{cache_name}_delete AFTER DELETE ON rust.{target}
                 WHEN cliphost_cache_loading()=0 BEGIN {begin}
                   DELETE FROM \"{source}\" WHERE Id=OLD.id AND Data IS OLD.legacy_data{cas};
                   SELECT CASE WHEN changes()<>1 THEN RAISE(ABORT,'original data changed; refresh and retry') END;
                   {end} END;",insert_columns.join(","),insert_values.join(","),updates.join(",")))?;
        }
        Ok(())
    }
}
fn resolve_image_reference(image: &mut Option<String>, root: &Path) {
    if let Some(path) = image.as_mut() {
        if !Path::new(path).is_absolute() {
            *path = root.join(&*path).to_string_lossy().into_owned();
        }
    }
}
fn preserve_image_reference(
    table: &str,
    record: &mut Value,
    previous: &Value,
    root: &Path,
) -> rusqlite::Result<()> {
    let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
    let old = if table == "clip_item" {
        crate::csharp_store_document::parse_clip(id, previous)
            .map_err(invalid)?
            .image_path
    } else {
        crate::csharp_store_document::parse_favor(id, previous)
            .map_err(invalid)?
            .image_path
    };
    if let Some(old) = old.filter(|p| !Path::new(p).is_absolute()) {
        for key in ["imagePath", "image_path"] {
            if record
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|new| Path::new(new) == root.join(&old))
            {
                record[key] = Value::String(old.clone());
            }
        }
    }
    Ok(())
}
fn parse(raw: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|e| invalid(e.to_string()))
}
fn document_revision(data: &str, token: Option<&str>) -> String {
    crate::model::sha256_hex(format!("{}\0{}", token.unwrap_or(""), data).as_bytes())
}

enum StorageSchema {
    Native,
    Documents(Option<&'static str>),
}

fn inspect_storage_schema(connection: &Connection, path: &Path) -> rusqlite::Result<StorageSchema> {
    let clips = columns(connection, "clip_item")?;
    let favors = columns(connection, "favor_item")?;
    for (table, names, old_table) in [
        ("clip_item", &clips, "ClipboardItem"),
        ("favor_item", &favors, "FavorItem"),
    ] {
        if !names.is_empty() { continue; }
        // These are the actual SQL names declared by ClipItem_250618 and
        // FavorItem_250710, not their CLR class names. Existing modern tables
        // may legitimately coexist with obsolete rows left after migration.
        let old_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type IN ('table','view') AND name=?1 COLLATE NOCASE)",
            [old_table], |row| row.get(0))?;
        if old_exists && connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM main.\"{old_table}\" LIMIT 1)"),
            [], |row| row.get::<_, bool>(0))? {
            return Err(invalid(format!(
                "旧表 {old_table} 仍有尚未升级的记录，而 {table} 不存在；未创建空表、转换或替换原数据库。数据库路径={}。",
                path.display()
            )));
        }
    }
    let wrapper = |names: &[String]| has(names, "Id") && has(names, "Data") && !has(names, "kind");
    let native = |names: &[String]| names.is_empty() || (has(names, "kind") && !has(names, "Data"));
    if native(&clips) && native(&favors) { return Ok(StorageSchema::Native); }
    if !(clips.is_empty() || wrapper(&clips)) || !(favors.is_empty() || wrapper(&favors)) {
        // Report schema metadata only, so Release startup diagnostics can
        // explain a rejected old database without exporting its contents.
        return Err(invalid(format!(
            "旧数据库格式不受支持；未转换或替换原数据库。数据库路径={}；clip_item 列={clips:?}；favor_item 列={favors:?}。",
            path.display()
        )));
    }
    let usage = columns(connection, "UsageInfo")?;
    if !usage.is_empty() && !wrapper(&usage) {
        return Err(invalid("旧 UsageInfo 表格式不受支持。"));
    }
    // Validate all existing wrapper metadata before any repair DDL. An
    // unsupported UsageInfo or required extension must leave the DB untouched.
    for (table, names) in [("clip_item", &clips), ("favor_item", &favors), ("UsageInfo", &usage)] {
        let is_table: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE)",
            [table], |row| row.get(0))?;
        if !names.is_empty() && !is_table {
            return Err(invalid(format!("旧表 {table} 不是可写文档表；未转换或替换原数据库。")));
        }
        if !names.is_empty() && !document_id_has_unique_key(connection, table)? {
            return Err(invalid(format!(
                "旧表 {table} 的 Id 缺少单列主键或非部分唯一索引；未转换或替换原数据库。数据库路径={}。",
                path.display()
            )));
        }
        for name in names {
            if !["id", "data", "objecttype", "createdtime", "updatetime", "rowtoken"]
                .contains(&name.to_ascii_lowercase().as_str()) {
                let required: bool = connection.query_row(
                    "SELECT \"notnull\"<>0 AND dflt_value IS NULL AND pk=0 FROM pragma_table_info(?1) WHERE name=?2",
                    [table, name], |row| row.get(0))?;
                if required {
                    return Err(invalid(format!("旧表 {table} 的必填扩展字段 {name} 无默认值。")));
                }
            }
        }
    }
    Ok(StorageSchema::Documents(if clips.is_empty() { Some("clip_item") }
        else if favors.is_empty() { Some("favor_item") } else { None }))
}

fn document_id_has_unique_key(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    let mut primary = connection.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk<>0 ORDER BY pk")?;
    let primary = primary.query_map([table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if primary.len() == 1 && primary[0].eq_ignore_ascii_case("Id") { return Ok(true); }
    // Writes use ON CONFLICT(Id), which cannot target a composite, partial or
    // expression index. A separate single-column UNIQUE(Id) is also valid.
    let mut indexes = connection.prepare("SELECT name FROM pragma_index_list(?1) WHERE \"unique\"<>0 AND partial=0")?;
    let indexes = indexes.query_map([table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for index in indexes {
        let mut keys = connection.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        let keys = keys.query_map([index], |row| row.get::<_, Option<String>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if keys.len() == 1 && keys[0].as_deref().is_some_and(|name| name.eq_ignore_ascii_case("Id")) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn columns(c: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut s = c.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let rows = s.query_map([table], |r| r.get(0))?;
    rows.collect()
}
fn has(columns: &[String], name: &str) -> bool {
    columns.iter().any(|s| s.eq_ignore_ascii_case(name))
}
fn projection(
    fields: &[(&'static str, &str)],
    dates: &[(&'static str, &str)],
) -> Vec<(&'static str, String)> {
    let mut out: Vec<_> = fields
        .iter()
        .map(|(sql, key)| (*sql, format!("json_extract(decoded,'$.{key}')")))
        .collect();
    out.extend(dates.iter().map(|(sql, key)| {
        (
            *sql,
            format!("cliphost_timestamp_ms(json_extract(decoded,'$.{key}'))"),
        )
    }));
    out
}
const CLIP_COLUMNS: &[(&str, &str)] = &[
    ("kind", "kind"),
    ("title", "title"),
    ("body_text", "bodyText"),
    ("html_data", "htmlData"),
    ("file_paths", "filePaths"),
    ("image_path", "imagePath"),
    ("is_top", "isTop"),
    ("source_process", "sourceProcessName"),
    ("content_hash", "contentHash"),
    ("usage_count", "usageCount"),
];
const CLIP_DATE_COLUMNS: &[(&str, &str)] = &[
    ("create_time", "createTime"),
    ("update_time", "updateTime"),
    ("last_access", "lastAccess"),
];
/// Skips source rows (alias `m`) whose document and token match their projection.
fn unchanged_filter(target: &str, token: &str) -> String {
    let token = if token == "NULL" { "NULL".to_string() } else { format!("m.{token}") };
    format!("AND NOT EXISTS (SELECT 1 FROM rust.cliphost_cache_{target} c
        WHERE c.id=m.Id AND c.legacy_data IS m.Data AND c.legacy_token IS {token})")
}
/// The SQLite `json_extract` result for one decoded value.
fn json_sql_value(value: Option<&Value>) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        None | Some(Value::Null) => Sql::Null,
        Some(Value::Bool(b)) => Sql::Integer(*b as i64),
        Some(Value::Number(n)) => n.as_i64().map(Sql::Integer).unwrap_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0))),
        Some(Value::String(s)) => Sql::Text(s.clone()),
        Some(other) => Sql::Text(other.to_string()),
    }
}
/// One `clip_item` row of `clip_projection()` in column order, followed by
/// id, extra_json and the legacy data/token/revision columns.
fn project_clip_document(id: &str, data: &str, token: Option<&str>, image_root: &Path)
    -> Result<Vec<rusqlite::types::Value>, String> {
    use rusqlite::types::Value as Sql;
    let raw: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
    let mut item = crate::csharp_store_document::parse_clip(id, &raw)?;
    resolve_image_reference(&mut item.image_path, image_root);
    let mut decoded = serde_json::to_value(item).map_err(|e| e.to_string())?;
    let revision = document_revision(data, token);
    let mut values: Vec<Sql> = CLIP_COLUMNS.iter().map(|(_, key)| json_sql_value(decoded.get(*key))).collect();
    for (_, key) in CLIP_DATE_COLUMNS {
        values.push(match decoded.get(*key) {
            None | Some(Value::Null) => Sql::Null,
            Some(Value::String(date)) => Sql::Integer(DateTime::parse_from_rfc3339(date).map_err(|e| e.to_string())?.timestamp_millis()),
            Some(other) => return Err(format!("invalid date {other}")),
        });
    }
    decoded["storageRevision"] = Value::String(revision.clone());
    values.extend([Sql::Text(id.into()), Sql::Text(decoded.to_string()), Sql::Text(data.into()),
        token.map_or(Sql::Null, |t| Sql::Text(t.into())), Sql::Text(revision)]);
    Ok(values)
}
fn clip_projection() -> Vec<(&'static str, String)> {
    let mut p = projection(CLIP_COLUMNS, CLIP_DATE_COLUMNS);
    p.extend([
        ("id", "Id".into()),
        (
            "extra_json",
            "json_set(decoded,'$.storageRevision',cliphost_document_revision(Data,token))".into(),
        ),
    ]);
    p
}
fn favor_projection() -> Vec<(&'static str, String)> {
    let mut p = projection(
        &[
            ("parent_id", "parentId"),
            ("is_folder", "isFolder"),
            ("name", "name"),
            ("kind", "kind"),
            ("body_text", "bodyText"),
            ("file_paths", "filePaths"),
            ("image_path", "imagePath"),
            ("children", "children"),
            ("source_clip_id", "sourceClipId"),
            ("html_data", "htmlData"),
            ("html_source_url", "htmlSourceUrl"),
            ("qq_rich_text", "qqRichText"),
            ("source_process", "sourceProcessName"),
            ("sub_index", "subIndex"),
            ("icon", "icon"),
            ("execute_param", "executeParam"),
        ],
        &[("create_time", "createTime"), ("update_time", "updateTime")],
    );
    p.push(("id", "Id".into()));
    p
}
fn favor_json() -> String {
    let mut p = favor_projection()
        .into_iter()
        .filter(|(key, _)| *key != "id")
        .map(|(key, _)| format!("'{key}',NEW.{key}"))
        .collect::<Vec<_>>();
    p.push("'id',NEW.id".into());
    format!("json_object({})", p.join(","))
}
// SQL emitted by AppStore: quoted values/JSON are copied verbatim. Index and
// PRAGMA schemas require their own SQLite syntax, unlike ordinary table refs.
fn rewrite_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 64);
    for statement in sql_statements(sql) {
        let lower = statement.trim_start().to_ascii_lowercase();
        let index = lower.starts_with("create index") || lower.starts_with("create unique index");
        let pragma = lower.starts_with("pragma table_info(");
        let mut chars = statement.char_indices().peekable();
        while let Some((start, ch)) = chars.next() {
            if matches!(ch, '\'' | '"' | '\u{60}' | '[') {
                let quote = if ch == '[' { ']' } else { ch };
                let mut end = start + ch.len_utf8();
                while let Some((offset, q)) = chars.next() {
                    end = offset + q.len_utf8();
                    if q == quote {
                        if chars.peek().is_some_and(|(_, q)| *q == quote) {
                            let (i, q) = chars.next().unwrap();
                            end = i + q.len_utf8();
                        } else {
                            break;
                        }
                    }
                }
                out.push_str(&statement[start..end]);
                continue;
            }
            if ch == '-' && chars.peek().is_some_and(|(_, c)| *c == '-') {
                let mut end = start + ch.len_utf8();
                for (offset, c) in chars.by_ref() {
                    end = offset + c.len_utf8();
                    if c == '\n' {
                        break;
                    }
                }
                out.push_str(&statement[start..end]);
                continue;
            }
            if ch == '/' && chars.peek().is_some_and(|(_, c)| *c == '*') {
                let mut end = start + 1;
                let mut star = false;
                for (offset, c) in chars.by_ref() {
                    end = offset + c.len_utf8();
                    if star && c == '/' {
                        break;
                    }
                    star = c == '*';
                }
                out.push_str(&statement[start..end]);
                continue;
            }
            if ch.is_ascii_alphabetic() || ch == '_' {
                let mut end = start + ch.len_utf8();
                while chars
                    .peek()
                    .is_some_and(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                {
                    let (i, c) = chars.next().unwrap();
                    end = i + c.len_utf8();
                }
                let word = &statement[start..end];
                let lower = word.to_ascii_lowercase();
                if pragma && lower == "table_info" {
                    out.push_str("rust.table_info");
                } else if CACHE.contains(&lower.as_str()) {
                    out.push_str(&format!(
                        "{}cliphost_cache_{lower}",
                        if index || pragma { "" } else { "rust." }
                    ));
                } else if lower.starts_with("idx_clip_") && index {
                    out.push_str(&format!("rust.cliphost_{lower}"));
                } else if AUX.contains(&lower.as_str()) || lower == "idx_explorer_update" {
                    out.push_str(&format!("cliphost_{lower}"));
                } else {
                    out.push_str(word);
                }
            } else {
                out.push(ch);
            }
        }
    }
    out
}

fn sql_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let (mut start, mut i) = (0, 0);
    let (mut quote, mut line, mut block) = (None, false, false);
    let mut out = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if line {
            if b == b'\n' {
                line = false;
            }
        } else if block {
            if b == b'*' && next == Some(b'/') {
                block = false;
                i += 1;
            }
        } else if let Some(q) = quote {
            if b == q {
                if next == Some(q) {
                    i += 1;
                } else {
                    quote = None;
                }
            }
        } else if b == b'-' && next == Some(b'-') {
            line = true;
            i += 1;
        } else if b == b'/' && next == Some(b'*') {
            block = true;
            i += 1;
        } else if [b'\'', b'"', b'\x60', b'['].contains(&b) {
            quote = Some(if b == b'[' { b']' } else { b });
        } else if b == b';' {
            out.push(&sql[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < sql.len() {
        out.push(&sql[start..]);
    }
    out
}

#[cfg(test)]
#[path = "store_document_recovery_tests.rs"]
mod document_recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_schema_reports_columns_without_contents_or_database_changes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("userdata.db");
        {
            let database = Connection::open(&path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, LegacyPayload TEXT);
                     CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT);
                     INSERT INTO clip_item VALUES('private-id', 'private clipboard contents');",
                )
                .unwrap();
        }
        let original = std::fs::read(&path).unwrap();
        let error = match StoreConnection::open(&path) {
            Ok(_) => panic!("An unsupported schema must not be accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("未转换或替换原数据库"));
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("clip_item 列=[\"Id\", \"LegacyPayload\"]"));
        assert!(error.contains("favor_item 列=[\"Id\", \"Data\"]"));
        assert!(!error.contains("private-id"));
        assert!(!error.contains("private clipboard contents"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn document_sql_keeps_quoted_payloads_and_resumes_after_comments() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "ATTACH ':memory:' AS rust; CREATE TABLE rust.cliphost_cache_clip_item(id TEXT);
            INSERT INTO rust.cliphost_cache_clip_item VALUES('row');",
        )
        .unwrap();
        for sql in [
            "SELECT 'clip_item; favor_item' FROM clip_item",
            "-- clip_item; comment\nSELECT 'clip_item; usage_info' FROM clip_item",
            "/* clip_item; comment */ SELECT 'clip_item'';favor_item' FROM clip_item",
        ] {
            let value: String = c.query_row(&rewrite_sql(sql), [], |r| r.get(0)).unwrap();
            assert!(value.starts_with("clip_item"));
            assert!(!value.contains("cache"));
        }
    }

    fn csharp_fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("userdata.db");
        Connection::open(&path).unwrap().execute_batch(
            "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);
             CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
             CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);",
        ).unwrap();
        let store = crate::store::AppStore::open(&path).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 10, 2, 9, 30, 0).unwrap();
        let records: Vec<_> = (0..40).map(|i| {
            let mut item = crate::model::ClipRecord::new_text(format!("文本 {i} \"quoted\" \\ line\r\n"), "proc.exe");
            item.id = format!("proj-{i:02}");
            item.create_time = base + chrono::Duration::nanoseconds(i * 1_234_500);
            item.update_time = item.create_time;
            item.is_top = i % 5 == 0;
            item.last_access = (i % 3 == 0).then(|| base + chrono::Duration::seconds(i));
            item.usage_count = (i % 4) as i32;
            match i % 6 {
                1 => { item.kind = crate::model::ClipKind::HtmlClipItem; item.html_data = Some("<b>x</b>".into()); }
                2 => { item.kind = crate::model::ClipKind::FileClipItem; item.file_paths = vec![r"C:\a b\c.txt".into(), r"D:\d".into()]; }
                3 => { item.kind = crate::model::ClipKind::ImageClipItem; item.image_path = Some(if i % 2 == 0 { "Perf/x.png".into() } else { r"C:\abs\y.png".into() }); }
                4 => { item.kind = crate::model::ClipKind::LinkClipItem; item.title = Some("标题".into()); }
                _ => {}
            }
            item
        }).collect();
        store.insert_clips(&records).unwrap();
        path
    }

    fn sql_projection_rows(c: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        let projection = clip_projection();
        let values = projection.iter().map(|p| p.1.as_str()).collect::<Vec<_>>().join(",");
        let sql = format!("WITH records AS MATERIALIZED (SELECT Id,Data,RowToken AS token,
            cliphost_read_document('clip_item',Id,Data) AS decoded FROM main.clip_item)
            SELECT {values},Data,token,cliphost_document_revision(Data,token) FROM records ORDER BY Id");
        let mut s = c.prepare(&sql).unwrap();
        let n = s.column_count();
        s.query_map([], |r| (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect()).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap()
    }

    fn cached_rows(c: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        let names = clip_projection().iter().map(|p| p.0).collect::<Vec<_>>().join(",");
        let mut s = c.prepare(&format!(
            "SELECT {names},legacy_data,legacy_token,legacy_revision FROM rust.cliphost_cache_clip_item ORDER BY id")).unwrap();
        let n = s.column_count();
        s.query_map([], |r| (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect()).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap()
    }

    fn assert_rows_equal(rust: Vec<Vec<rusqlite::types::Value>>, sql: Vec<Vec<rusqlite::types::Value>>) {
        use rusqlite::types::Value as Sql;
        let extra = clip_projection().iter().position(|p| p.0 == "extra_json").unwrap();
        assert_eq!(rust.len(), sql.len());
        for (a, b) in rust.iter().zip(&sql) {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                if i == extra {
                    // json_set and serde may order keys differently; compare documents.
                    let (Sql::Text(x), Sql::Text(y)) = (x, y) else { panic!("extra_json not text") };
                    assert_eq!(serde_json::from_str::<Value>(x).unwrap(), serde_json::from_str::<Value>(y).unwrap());
                } else {
                    assert_eq!(x, y, "column {} of {:?}", clip_projection().get(i).map_or("legacy", |p| p.0), a.last());
                }
            }
        }
    }

    #[test]
    fn rust_clip_projection_matches_sql_projection_and_refreshes_only_changed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = csharp_fixture(dir.path());
        let store = crate::store::AppStore::open(&path).unwrap();
        let c = store.connection();
        assert_rows_equal(cached_rows(&c.inner), sql_projection_rows(&c.inner));
        let rowids = |c: &Connection| -> Vec<(String, i64)> {
            let mut s = c.prepare("SELECT id, rowid FROM rust.cliphost_cache_clip_item ORDER BY id").unwrap();
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
        };
        let before = rowids(&c.inner);
        // Another writer edits one document, deletes one and inserts a copy of another.
        let other = Connection::open(&path).unwrap();
        other.execute_batch(
            "UPDATE clip_item SET Data=json_set(Data,'$.Item.Title','外部修改'), RowToken='t2' WHERE Id='proj-07';
             DELETE FROM clip_item WHERE Id='proj-08';
             INSERT INTO clip_item(Id,Data,ObjectType,CreatedTime,UpdateTime,RowToken)
               SELECT 'proj-99',replace(Data,'proj-09','proj-99'),ObjectType,CreatedTime,UpdateTime,'t3' FROM clip_item WHERE Id='proj-09';",
        ).unwrap();
        drop(other);
        c.before_read().unwrap();
        assert_rows_equal(cached_rows(&c.inner), sql_projection_rows(&c.inner));
        let after = rowids(&c.inner);
        assert!(after.iter().all(|row| row.0 != "proj-08") && after.iter().any(|row| row.0 == "proj-99"));
        // Unchanged and edited rows keep their position for equal-time tie order.
        for (id, rowid) in before.iter().filter(|row| row.0 != "proj-08") {
            assert_eq!(after.iter().find(|row| &row.0 == id).map(|row| row.1), Some(*rowid), "{id}");
        }
        let (token, edited): (String, bool) = c.inner.query_row(
            "SELECT legacy_token, instr(legacy_data,'外部修改')>0 FROM rust.cliphost_cache_clip_item WHERE id='proj-07'",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((token.as_str(), edited), ("t2", true));
    }

    #[test]
    fn document_transaction_rejects_external_commit_between_projection_and_begin_then_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let path = csharp_fixture(dir.path());
        let store = crate::store::AppStore::open(&path).unwrap();
        let mut parent = crate::model::FavorRecord::from_clip(
            &crate::model::ClipRecord::new_text("Parent", ""), None,
        );
        parent.is_folder = true;
        parent.kind = crate::model::ClipKind::FavoriteClipItem;
        parent.children = vec!["first".into()];
        store.insert_favor_checked(&parent).unwrap();
        let connection = store.connection();
        // This is the exact seam inside unchecked_transaction: projection is
        // current, then the other host changes an edge before BEGIN starts.
        connection.before_read().unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute("UPDATE favor_item SET Data=json_set(Data,'$.Children',json('[\"first\",\"external-edge\"]')) WHERE Id=?1", [&parent.id]).unwrap();
        let error = match connection.start_snapshot_transaction() {
            Ok(_) => panic!("an old relationship projection must not be admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("WRITE_CONFLICT"));
        assert!(connection.inner.is_autocommit(), "failed admission must roll back BEGIN");
        assert!(store.kv_get("snapshot-test-write").unwrap().is_none());
        let current = store.get_favor(&parent.id).unwrap();
        assert_eq!(current.children, ["first", "external-edge"]);
        let transaction = connection.unchecked_transaction().unwrap();
        assert_eq!(store.get_favor(&parent.id).unwrap().children, current.children);
        store.kv_set("snapshot-test-write", "committed after refresh").unwrap();
        transaction.commit().unwrap();
        assert_eq!(store.kv_get("snapshot-test-write").unwrap().as_deref(), Some("committed after refresh"));
    }
}
