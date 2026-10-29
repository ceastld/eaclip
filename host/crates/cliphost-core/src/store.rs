use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, OptionalExtension};
use thiserror::Error;

use crate::clip_dedupe::{discard_duplicate_capture_artifact, RECENT_DUPLICATE_SCAN_COUNT};
use crate::model::{hash_files, ClipKind, ClipRecord, FavorRecord};

#[path = "store_image_identity.rs"]
mod image_identity_cache;
pub(crate) use image_identity_cache::PreparedImageIdentities;
#[path = "store_reopen.rs"]
mod reopen_storage;
#[path = "store_lan_outbox.rs"]
mod lan_outbox_storage;

/// C# `ClipItem.UsageInfoKey`.
pub fn clip_usage_key(id: &str) -> String {
    format!("clip_item_{id}")
}

/// C# `FavorItem.UsageInfoKey`.
pub fn favor_usage_key(id: &str) -> String {
    format!("favor_item_{id}")
}

fn core_error(error:String)->StoreError{
    if let Some(id)=error.strip_prefix("NOT_FOUND:"){StoreError::NotFound(id.into())}else{StoreError::Core(error)}
}

/// C# `BetterClip.Model.Records.UsageInfo`.
#[derive(Debug, Clone, Default)]
pub struct UsageInfo {
    pub last_use_time: Option<DateTime<Utc>>,
    pub usage_count: i32,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("core storage: {0}")]
    Core(String),
    #[error("Image candidates changed; prepare exact identities again outside the store lock.")]
    ImageIdentityPreparationNeeded,
}

/// Indexed list fields of a clip record (see `AppStore::list_clip_keys`).
#[derive(Debug, Clone)]
pub struct ClipListKey {
    pub id: String,
    pub kind: ClipKind,
    pub is_top: bool,
    pub source_process_name: String,
    pub create_ms: i64,
    pub last_access_ms: Option<i64>,
    pub usage_count: i32,
}

/// A file already copied and validated by a worker outside the database lock.
/// Only the image payload fields are applied; item metadata comes from the
/// unchanged persisted snapshot, never from the worker's replacement record.
#[derive(Debug, Clone)]
pub struct FileHistoryImageReplacement {
    pub snapshot: ClipRecord,
    pub image_path: String,
    pub content_hash: String,
    pub perceptual_hash: String,
}

#[derive(Debug, Default)]
pub struct FileHistoryMutationResult {
    pub changed: Vec<ClipRecord>,
    pub removed: Vec<String>,
}

pub struct AppStore {
    conn: crate::store_connection::StoreConnection,
    lan_outbox_schedule: std::cell::RefCell<Option<lan_outbox_storage::LanOutboxSchedule>>,
}

/// C# `FavorItemExtensions.AddToParentChildren`.
fn insert_child_at(children: &mut Vec<String>, id: &str, index: i32) {
    if children.iter().any(|c| c == id) {
        return;
    }
    if index < 0 {
        children.push(id.to_string());
        return;
    }
    let i = (index.max(0) as usize).min(children.len());
    children.insert(i, id.to_string());
}

impl AppStore {
    /// Only isolated copies beneath this repository's .tmp can opt into the
    /// candidate runtime. Normal startup never opts in and never converts.
    pub fn open_isolated_core(db_path:&Path)->Result<Self,StoreError>{
        let sandbox=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        let sandbox=std::fs::canonicalize(sandbox)?;
        let path=std::fs::canonicalize(db_path)?;
        if !path.starts_with(&sandbox){return Err(StoreError::Core("Core runtime is restricted to explicit repository .tmp copies.".into()));}
        let conn=crate::store_connection::StoreConnection::open_isolated_core(&path)?;
        Ok(Self{conn, lan_outbox_schedule: Default::default()})
    }
    pub fn is_core_store(&self)->bool{self.conn.is_core_store()}
    pub fn validate_favorite_graph(&self)->Result<(),StoreError>{
        if let Some((c,core))=self.conn.core(){core.validate_graph(c).map_err(core_error)?;}Ok(())
    }
    pub fn save_clip_edit(&self,item:&ClipRecord,title_override:Option<Option<String>>)->Result<(),StoreError>{
        if let Some((c,core))=self.conn.core(){return core.save_clip(c,item,title_override).map_err(core_error);}self.save_clip(item)
    }
    pub fn save_favor_edit(&self,item:&FavorRecord,title_override:Option<Option<String>>)->Result<(),StoreError>{
        if let Some((c,core))=self.conn.core(){return core.save_favor(c,item,title_override).map_err(core_error);}self.save_favor(item)
    }
    pub fn core_output_clip(&self,item:&ClipRecord)->Result<ClipRecord,StoreError>{
        let Some((_,core))=self.conn.core()else{return Ok(item.clone());};
        let resolver=crate::storage_core::resources::ResourceResolver::new(&core.root,&core.historical_roots).map_err(core_error)?;
        crate::storage_core::resources::project_clip_for_output(item,&resolver).map_err(core_error)
    }
    pub fn core_output_favor(&self,item:&FavorRecord)->Result<FavorRecord,StoreError>{
        if !self.is_core_store(){return Ok(item.clone());}
        let mut clip=ClipRecord::new_text("",&item.source_process_name);clip.kind=item.kind;clip.body_text=item.body_text.clone();clip.html_data=item.html_data.clone();clip.html_source_url=item.html_source_url.clone();clip.qq_rich_text=item.qq_rich_text.clone();clip.image_path=item.image_path.clone();clip.file_paths=item.file_paths.clone();
        let output=self.core_output_clip(&clip)?;let mut result=item.clone();result.html_data=output.html_data;result.html_source_url=output.html_source_url;result.qq_rich_text=output.qq_rich_text;result.image_path=output.image_path;result.file_paths=output.file_paths;Ok(result)
    }
    /// Disposable final-I/O projection. It cannot be saved as a CAS baseline.
    pub fn core_io_clip(&self,item:&ClipRecord)->Result<ClipRecord,StoreError>{
        let Some((_,core))=self.conn.core()else{return Ok(item.clone());};
        let resolver=crate::storage_core::resources::ResourceResolver::new(&core.root,&core.historical_roots).map_err(core_error)?;
        crate::storage_core::resources::project_clip_for_io(item,&resolver).map_err(core_error)
    }
    pub fn core_io_favor(&self,item:&FavorRecord)->Result<FavorRecord,StoreError>{
        if !self.is_core_store(){return Ok(item.clone());}
        let mut clip=ClipRecord::new_text("",&item.source_process_name);clip.storage_revision=item.storage_revision.clone();clip.kind=item.kind;clip.body_text=item.body_text.clone();clip.html_data=item.html_data.clone();clip.html_source_url=item.html_source_url.clone();clip.qq_rich_text=item.qq_rich_text.clone();clip.image_path=item.image_path.clone();clip.file_paths=item.file_paths.clone();
        let output=self.core_io_clip(&clip)?;let mut result=item.clone();result.storage_revision=None;result.html_data=output.html_data;result.html_source_url=output.html_source_url;result.qq_rich_text=output.qq_rich_text;result.image_path=output.image_path;result.file_paths=output.file_paths;Ok(result)
    }
    /// Exact row removal for a previously validated undo plan. No recursion or
    /// parent rewriting; the caller restores the complete graph atomically.
    pub fn delete_favors_exact(&self,ids:&[String])->Result<(),StoreError>{
        if let Some((c,core))=self.conn.core(){core.delete_exact(c,"favorite",ids).map_err(core_error)?;return Ok(());}
        for id in ids{self.conn.execute("DELETE FROM favor_item WHERE id=?1",[id])?;}Ok(())
    }
    #[cfg(test)]
    pub(crate) fn set_busy_handler(&self, handler: fn(i32) -> bool) {
        self.conn.busy_handler(Some(handler)).unwrap();
    }

    pub(crate) fn connection(&self) -> &crate::store_connection::StoreConnection {
        &self.conn
    }

    pub fn open(db_path: &Path) -> Result<Self, StoreError> {
        let store = Self::open_unprojected(db_path)?;
        store.finish_projection()?;
        Ok(store)
    }

    /// Open and migrate without projecting an in-place C# document store. Every
    /// other read or write of such a store fails until `finish_projection` succeeds.
    pub fn open_unprojected(db_path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = crate::store_connection::StoreConnection::open(db_path)?;
        let store = Self { conn, lan_outbox_schedule: Default::default() };
        store.migrate()?;
        store.conn.mark_projection_pending();
        Ok(store)
    }

    pub fn finish_projection(&self) -> Result<(), StoreError> {
        self.conn.finish_initialization()?;
        self.import_legacy_explorer_history()?;
        Ok(())
    }

    /// Rows in the original C# history table, without decoding them.
    pub fn source_clip_count(&self) -> Result<i64, StoreError> {
        Ok(self.conn.source_clip_count()?)
    }

    /// Changes when this connection writes or another writer commits.
    pub fn change_stamp(&self) -> Result<(u64, i64), StoreError> {
        Ok(self.conn.change_stamp()?)
    }

    pub fn is_document_store(&self) -> bool {
        self.conn.is_document_store()
    }

    /// Runtime document isolation is confined to the query cache. Never treat
    /// its partial result as a complete writable database.
    pub fn document_recovery_status(&self) -> Result<serde_json::Value, StoreError> {
        Ok(self.conn.document_recovery_status()?)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        if self.conn.is_core_store(){return Ok(());}
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS clip_item (
                id TEXT PRIMARY KEY NOT NULL,
                kind TEXT NOT NULL,
                title TEXT,
                body_text TEXT,
                html_data TEXT,
                file_paths TEXT NOT NULL DEFAULT '[]',
                image_path TEXT,
                is_top INTEGER NOT NULL DEFAULT 0,
                create_time INTEGER NOT NULL,
                update_time INTEGER NOT NULL,
                source_process TEXT NOT NULL DEFAULT '',
                content_hash TEXT NOT NULL,
                last_access INTEGER,
                usage_count INTEGER NOT NULL DEFAULT 0,
                extra_json TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_clip_create ON clip_item(create_time DESC);
            CREATE INDEX IF NOT EXISTS idx_clip_top_create ON clip_item(is_top DESC, create_time DESC);
            CREATE INDEX IF NOT EXISTS idx_clip_hash ON clip_item(content_hash);
            CREATE TABLE IF NOT EXISTS favor_item (
                id TEXT PRIMARY KEY NOT NULL,
                parent_id TEXT,
                is_folder INTEGER NOT NULL DEFAULT 0,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                body_text TEXT,
                file_paths TEXT NOT NULL DEFAULT '[]',
                image_path TEXT,
                children TEXT NOT NULL DEFAULT '[]',
                create_time INTEGER NOT NULL,
                source_clip_id TEXT
            );
            CREATE TABLE IF NOT EXISTS text_action (
                id TEXT PRIMARY KEY NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS lan_peer (
                id TEXT PRIMARY KEY NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS lan_conversation (
                id TEXT PRIMARY KEY NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS lan_message (
                id TEXT PRIMARY KEY NOT NULL,
                conversation_id TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS collect_draft (
                id TEXT PRIMARY KEY NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS explorer_record (
                path TEXT PRIMARY KEY NOT NULL,
                update_time INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_explorer_update ON explorer_record(update_time DESC);
            CREATE TABLE IF NOT EXISTS usage_info (
                id TEXT PRIMARY KEY NOT NULL,
                last_use_time INTEGER NOT NULL,
                usage_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS process_icon_meta (
                id TEXT PRIMARY KEY NOT NULL,
                exe_path TEXT NOT NULL,
                update_time INTEGER NOT NULL
            );
            "#,
        )?;
        self.conn.execute(
            r#"INSERT OR IGNORE INTO usage_info (id, last_use_time, usage_count)
               SELECT 'clip_item_' || id,
                      COALESCE(last_access, create_time),
                      usage_count
               FROM clip_item
               WHERE usage_count > 0 OR last_access IS NOT NULL"#,
            [],
        )?;
        self.ensure_column("favor_item", "html_data", "TEXT")?;
        self.ensure_column("favor_item", "html_source_url", "TEXT")?;
        self.ensure_column("favor_item", "qq_rich_text", "TEXT")?;
        self.ensure_column("favor_item", "update_time", "INTEGER")?;
        self.ensure_column("favor_item", "source_process", "TEXT NOT NULL DEFAULT ''")?;
        self.ensure_column("favor_item", "sub_index", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_column("favor_item", "icon", "TEXT NOT NULL DEFAULT ''")?;
        self.ensure_column("favor_item", "execute_param", "TEXT NOT NULL DEFAULT ''")?;
        self.conn.execute(
            "UPDATE favor_item SET update_time = create_time WHERE update_time IS NULL",
            [],
        )?;
        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, decl: &str) -> Result<(), StoreError> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let exists = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == column);
        if !exists {
            self.conn
                .execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
        }
        Ok(())
    }

    pub fn insert_clip(&self, item: &ClipRecord, allow_duplicate: bool) -> Result<Option<String>, StoreError> {
        if !allow_duplicate {
            if let Some(latest) = self.latest_hash()? {
                if latest == item.content_hash {
                    return Ok(None);
                }
            }
        }
        if let Some((c,core))=self.conn.core(){core.insert_clip(c,item).map_err(core_error)?;return Ok(Some(item.id.clone()));}
        self.conn.execute(
            r#"INSERT INTO clip_item
                (id, kind, title, body_text, html_data, file_paths, image_path, is_top, create_time, update_time, source_process, content_hash, last_access, usage_count, extra_json)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"#,
            params![
                item.id,
                item.kind.as_str(),
                item.title,
                item.body_text,
                item.html_data,
                serde_json::to_string(&item.file_paths)?,
                item.image_path,
                item.is_top as i32,
                item.create_time.timestamp_millis(),
                item.update_time.timestamp_millis(),
                item.source_process_name,
                item.content_hash,
                item.last_access.map(|t| t.timestamp_millis()),
                item.usage_count,
                serde_json::to_string(item)?,
            ],
        )?;
        Ok(Some(item.id.clone()))
    }

    /// Import records and original directory ordering as one transaction.
    pub(crate) fn import_csharp_records(
        &self, clips: &[ClipRecord], favorites: &[FavorRecord], usage: &[(String, UsageInfo)],
    ) -> Result<(), StoreError> {
        if self.is_core_store(){return Err(StoreError::Core("Legacy flat import cannot overwrite canonical core content; use the explicit core migrator.".into()));}
        let transaction = self.conn.unchecked_transaction()?;
        for item in clips { self.insert_clip(item, true)?; }
        for item in favorites { self.insert_favor(item)?; }
        // insert_favor links each child. Restore the original ordered directory
        // documents after all inserts rather than deriving order from SQL rows.
        for item in favorites { self.save_favor(item)?; }
        for (key, info) in usage {
            if let Some(time) = info.last_use_time {
                self.conn.execute(
                    "INSERT INTO usage_info(id,last_use_time,usage_count) VALUES(?1,?2,?3)",
                    params![key, time.timestamp_millis(), info.usage_count],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Explicit Add / Split keeps duplicates and publishes only complete batches.
    pub fn insert_clips(&self, items: &[ClipRecord]) -> Result<(), StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        for item in items {
            self.insert_clip(item, true)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// C# `ClipDataService.CollectClipItem` merge: refresh CreateTime/UpdateTime, keep id.
    pub fn persist_capture(
        &self,
        incoming: &ClipRecord,
        allow_duplicate: bool,
        is_deferred_clear_hidden: impl Fn(&str) -> bool,
    ) -> Result<ClipRecord, StoreError> {
        self.persist_capture_inner(incoming, allow_duplicate, is_deferred_clear_hidden, None)
    }

    /// The caller has decoded only bounded, safe candidates outside this lock.
    /// A write transaction prevents an external collector slipping another row
    /// between the last identity check and the insert/merge.
    pub(crate) fn persist_capture_prepared(
        &self,
        incoming: &ClipRecord,
        allow_duplicate: bool,
        is_deferred_clear_hidden: impl Fn(&str) -> bool,
        prepared: &PreparedImageIdentities,
    ) -> Result<ClipRecord, StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        let result = self.persist_capture_inner(
            incoming,
            allow_duplicate,
            is_deferred_clear_hidden,
            Some(prepared),
        )?;
        transaction.commit()?;
        if !self.is_core_store() {
            discard_duplicate_capture_artifact(incoming, &result);
        }
        Ok(result)
    }

    fn persist_capture_inner(
        &self,
        incoming: &ClipRecord,
        allow_duplicate: bool,
        is_deferred_clear_hidden: impl Fn(&str) -> bool,
        prepared: Option<&PreparedImageIdentities>,
    ) -> Result<ClipRecord, StoreError> {
        let recent = self.list_recent_by_create_time()?;
        if !allow_duplicate {
            if let Some(prepared) = prepared {
                self.guard_prepared_image_candidates(
                    incoming, &recent, prepared, &is_deferred_clear_hidden,
                )?;
            }
        }
        let mut target = None;
        if !allow_duplicate {
            for candidate in recent
                .iter()
                .take(RECENT_DUPLICATE_SCAN_COUNT)
                .filter(|item| !is_deferred_clear_hidden(&item.id))
            {
                let equal = if prepared.is_some()
                    && candidate.kind == ClipKind::ImageClipItem
                    && incoming.kind == ClipKind::ImageClipItem
                {
                    self.images_equal_exact(candidate, incoming)?
                } else if self.is_core_store()
                    && candidate.kind == ClipKind::FileClipItem
                    && incoming.kind == ClipKind::FileClipItem
                {
                    crate::storage_core::records::dedupe(candidate)
                        .eq_ignore_ascii_case(&crate::storage_core::records::dedupe(incoming))
                } else {
                    candidate.looks_equal(incoming)
                };
                if equal {
                    target = Some(candidate);
                    break;
                }
            }
        }
        if let Some(target) = target {
            let mut merged = self.get_clip(&target.id)?;
            let now = Utc::now();
            merged.create_time = now;
            merged.update_time = now;
            self.save_clip(&merged)?;
            if self.is_core_store(){return self.get_clip(&merged.id);}
            if prepared.is_none() {
                discard_duplicate_capture_artifact(incoming, &merged);
            }
            return Ok(merged);
        }
        self.insert_clip(incoming, true)?;
        if self.is_core_store(){return self.get_clip(&incoming.id);}
        Ok(incoming.clone())
    }

    pub(crate) fn list_recent_by_create_time(&self) -> Result<Vec<ClipRecord>, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.recent_clips(c,RECENT_DUPLICATE_SCAN_COUNT).map_err(core_error);}
        let fetch = (RECENT_DUPLICATE_SCAN_COUNT * 2) as i32;
        let mut stmt = self.conn.prepare(
            "SELECT extra_json FROM clip_item ORDER BY create_time DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([fetch], |row| row.get::<_, String>(0))?;
        let mut items: Vec<ClipRecord> = Vec::new();
        for row in rows {
            items.push(serde_json::from_str(&row?)?);
        }
        items.sort_by(|a, b| {
            b.create_time
                .cmp(&a.create_time)
                .then_with(|| a.sub_index.cmp(&b.sub_index))
        });
        items.truncate(RECENT_DUPLICATE_SCAN_COUNT);
        Ok(items)
    }

    fn latest_hash(&self) -> Result<Option<String>, StoreError> {
        if let Some((c,core))=self.conn.core(){return Ok(core.recent_clips(c,1).map_err(core_error)?.into_iter().next().map(|i|i.content_hash));}
        let hash = self
            .conn
            .query_row(
                "SELECT content_hash FROM clip_item ORDER BY create_time DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(hash)
    }

    pub fn get_clip(&self, id: &str) -> Result<ClipRecord, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.get_clip(c,id).map_err(core_error);}
        let json = self
            .conn
            .query_row(
                "SELECT extra_json FROM clip_item WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(id.into()))?;
        let mut item: ClipRecord = serde_json::from_str(&json)?;
        self.overlay_clip_usage(&mut item)?;
        Ok(item)
    }

    /// Read only the explicitly requested visible rows, capped at 32 input ids.
    /// Inspect narrow fields first so text/HTML bodies are never decoded for
    /// file maintenance. Pins are included for image preservation; automatic
    /// removal checks their protected status again when committing.
    pub fn file_history_by_ids(&self, ids: &[String]) -> Result<Vec<ClipRecord>, StoreError> {
        if self.is_core_store() || ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = &ids[..ids.len().min(32)];
        let rows = {
            let marks = vec!["?"; ids.len()].join(",");
            let mut query = self.conn.prepare(&format!("SELECT id,kind FROM clip_item WHERE id IN ({marks})"))?;
            let rows = query.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut file_ids: std::collections::HashSet<String> = rows.into_iter()
            .filter(|(_, kind)| matches!(ClipKind::parse(kind), ClipKind::FileClipItem | ClipKind::ImageClipItem))
            .map(|(id, _)| id)
            .collect();
        let mut candidates = Vec::new();
        for id in ids {
            if file_ids.remove(id) {
                // A concurrent writer can remove a row between these reads.
                match self.get_clip(id) {
                    Ok(item) if matches!(item.kind, ClipKind::FileClipItem | ClipKind::ImageClipItem) => {
                        candidates.push(item);
                    }
                    Ok(_) | Err(StoreError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(candidates)
    }

    /// Commit only conclusions that still belong to the exact scanned record.
    /// All filesystem checks and copies must finish before entering this method.
    /// A stale/deleted candidate is skipped; a database error rolls back the
    /// complete batch, so returned deltas always describe committed data.
    pub fn apply_file_history_maintenance(
        &self,
        replacements: &[FileHistoryImageReplacement],
        removals: &[ClipRecord],
    ) -> Result<FileHistoryMutationResult, StoreError> {
        if self.is_core_store() {
            return if replacements.is_empty() && removals.is_empty() {
                Ok(FileHistoryMutationResult::default())
            } else {
                Err(StoreError::Core("File history maintenance is not enabled for the isolated core candidate.".into()))
            };
        }
        for replacement in replacements {
            if !matches!(replacement.snapshot.kind, ClipKind::FileClipItem | ClipKind::ImageClipItem)
                || replacement.snapshot.kind == ClipKind::FileClipItem && replacement.snapshot.file_paths.len() != 1
                || replacement.image_path.trim().is_empty()
                || !Path::new(&replacement.image_path).is_absolute()
                || replacement.content_hash.trim().is_empty()
            {
                return Err(StoreError::Core("Invalid prepared file history image replacement.".into()));
            }
        }
        let transaction = self.conn.unchecked_transaction()?;
        let mut result = FileHistoryMutationResult::default();
        let mut handled = std::collections::HashSet::new();
        for replacement in replacements {
            if !handled.insert(replacement.snapshot.id.clone()) { continue; }
            let mut current = match self.get_clip(&replacement.snapshot.id) {
                Ok(item) => item,
                Err(StoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if !same_file_history_snapshot(&current, &replacement.snapshot)? { continue; }
            current.kind = ClipKind::ImageClipItem;
            current.image_path = Some(replacement.image_path.clone());
            current.file_paths.clear();
            current.content_hash = replacement.content_hash.clone();
            current.perceptual_hash = replacement.perceptual_hash.clone();
            self.save_clip(&current)?;
            self.kv_delete(&format!("file-history-missing-v1:{}", current.id))?;
            result.changed.push(self.get_clip(&current.id)?);
        }
        for snapshot in removals {
            if snapshot.is_top
                || !matches!(snapshot.kind, ClipKind::FileClipItem | ClipKind::ImageClipItem)
                || !handled.insert(snapshot.id.clone())
            {
                continue;
            }
            let current = match self.get_clip(&snapshot.id) {
                Ok(item) => item,
                Err(StoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if current.is_top || !same_file_history_snapshot(&current, snapshot)? { continue; }
            if self.delete_clip_with_missing_status(&current.id)? != 0 {
                result.removed.push(current.id);
            }
        }
        transaction.commit()?;
        Ok(result)
    }

    pub fn list_clips(&self) -> Result<Vec<ClipRecord>, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.list_clips(c).map_err(core_error);}
        // rowid fixes the order of equal indexed times so `list_clip_keys` agrees.
        let mut stmt = self
            .conn
            .prepare("SELECT extra_json FROM clip_item ORDER BY is_top DESC, create_time DESC, rowid ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut items: Vec<ClipRecord> = Vec::new();
        for row in rows {
            items.push(serde_json::from_str(&row?)?);
        }
        let usage = self.usage_map_by_prefix("clip_item_")?;
        for item in &mut items {
            if let Some(info) = usage.get(&item.id) {
                apply_usage(item, info);
            }
        }
        // C# `ClipDataService` cache: IsTop DESC, CreateTime DESC, SubIndex ASC.
        items.sort_by(|a, b| {
            b.is_top
                .cmp(&a.is_top)
                .then_with(|| b.create_time.cmp(&a.create_time))
                .then_with(|| a.sub_index.cmp(&b.sub_index))
        });
        Ok(items)
    }

    /// Indexed list fields of every record in `list_clips` order, with the same
    /// usage overlay, without decoding record documents. Times are the indexed
    /// milliseconds; callers decode only the rows they need for exact ordering.
    pub fn list_clip_keys(&self) -> Result<Vec<ClipListKey>, StoreError> {
        self.list_clip_keys_for_filter(crate::clip_filter::TYPE_ALL)
    }

    /// URL categories inspect projected body text, without decoding full record
    /// documents (HTML and other large payloads are loaded only for the page).
    pub fn list_clip_keys_for_filter(&self, filter: &str) -> Result<Vec<ClipListKey>, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.list_keys(c,filter).map_err(core_error);}
        let needs_text = crate::clip_filter::requires_text(filter);
        let text_column = if needs_text { ", c.body_text, c.title" } else { "" };
        let mut stmt = self.conn.prepare(
            &format!(r#"SELECT c.id, c.kind, c.is_top, c.source_process, c.create_time, c.last_access, c.usage_count,
                      u.id IS NOT NULL, u.last_use_time, u.usage_count{text_column}
               FROM clip_item c LEFT JOIN usage_info u ON u.id='clip_item_' || c.id
               ORDER BY c.is_top DESC, c.create_time DESC, c.rowid ASC"#),
        )?;
        let rows = stmt.query_map([], |row| {
            let kind = ClipKind::parse(&row.get::<_, String>(1)?);
            let is_top = row.get::<_, i32>(2)? != 0;
            let text = if needs_text { row.get_ref(10)?.as_str().ok() } else { None };
            let title = if needs_text { row.get_ref(11)?.as_str().ok() } else { None };
            if !crate::clip_filter::matches_with_text(kind, is_top, text, filter)
                && !(needs_text && crate::clip_filter::matches_with_text(kind, is_top, title, filter)) {
                return Ok(None);
            }
            let (mut usage_count, mut last_access_ms) = (row.get::<_, i32>(6)?, row.get::<_, Option<i64>>(5)?);
            if row.get::<_, bool>(7)? {
                let (ms, count) = (row.get::<_, i64>(8)?, row.get::<_, i32>(9)?);
                usage_count = count;
                last_access_ms = usage_timestamp(ms, count).map(|t| t.timestamp_millis());
            }
            Ok(Some(ClipListKey {
                id: row.get(0)?,
                kind,
                is_top,
                source_process_name: row.get(3)?,
                create_ms: row.get(4)?,
                last_access_ms,
                usage_count,
            }))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?.into_iter().flatten().collect())
    }

    /// Decode the given records with `list_clips` usage semantics, keyed by id.
    pub fn list_clips_by_ids(&self, ids: &[&str]) -> Result<HashMap<String, ClipRecord>, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.clips_by_ids(c,ids).map_err(core_error);}
        let mut items = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = self.conn.prepare(&format!(
                r#"SELECT c.extra_json, u.id IS NOT NULL, u.last_use_time, u.usage_count
                   FROM clip_item c LEFT JOIN usage_info u ON u.id='clip_item_' || c.id
                   WHERE c.id IN ({marks})"#
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i32>>(3)?,
            )))?;
            for row in rows {
                let (json, has_usage, ms, count) = row?;
                let mut item: ClipRecord = serde_json::from_str(&json)?;
                if has_usage {
                    let (ms, count) = (ms.unwrap_or(0), count.unwrap_or(0));
                    apply_usage(&mut item, &UsageInfo { last_use_time: usage_timestamp(ms, count), usage_count: count });
                }
                items.insert(item.id.clone(), item);
            }
        }
        Ok(items)
    }

    /// Only decode recent quick-paste candidates and all pinned records. The
    /// complete boundary millisecond is retained: JSON dates have finer precision
    /// than the indexed timestamp, and SubIndex must break equal-date ties.
    pub fn list_quick_paste_candidates(&self, limit: i32) -> Result<Vec<ClipRecord>, StoreError> {
        let limit = crate::quick_paste::clamp_list_limit(limit);
        if self.is_core_store(){return Ok(crate::quick_paste::select_session_items(&self.list_clips()?,limit));}
        let mut stmt = self.conn.prepare(
            r#"WITH boundary AS (
                   SELECT create_time FROM clip_item WHERE is_top=0
                   ORDER BY create_time DESC LIMIT 1 OFFSET ?1
               ), candidates AS (
                   SELECT rowid AS order_row, id, is_top, create_time, extra_json
                   FROM clip_item
                   WHERE is_top=0 AND create_time >= COALESCE((SELECT create_time FROM boundary), -9223372036854775808)
                   UNION ALL
                   SELECT rowid AS order_row, id, is_top, create_time, extra_json
                   FROM clip_item WHERE is_top=1
               )
               SELECT c.extra_json, u.last_use_time, u.usage_count
               FROM candidates c LEFT JOIN usage_info u ON u.id='clip_item_' || c.id
               ORDER BY c.is_top DESC, c.create_time DESC, c.order_row ASC"#,
        )?;
        let rows = stmt.query_map([limit - 1], |row| Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i32>>(2)?,
        )))?;
        let mut candidates = Vec::new();
        for row in rows {
            let (json, last_use_time, usage_count) = row?;
            let mut item: ClipRecord = serde_json::from_str(&json)?;
            if let Some(usage_count) = usage_count {
                apply_usage(&mut item, &UsageInfo {
                    last_use_time: last_use_time.and_then(|ms| usage_timestamp(ms, usage_count)),
                    usage_count,
                });
            }
            candidates.push(item);
        }
        Ok(crate::quick_paste::select_session_items(&candidates, limit))
    }

    pub fn get_usage(&self, key: &str) -> Result<UsageInfo, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.usage(c,key).map_err(core_error);}
        let row = self
            .conn
            .query_row(
                "SELECT last_use_time, usage_count FROM usage_info WHERE id=?1",
                [key],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(ms, count)| UsageInfo {
            last_use_time: usage_timestamp(ms, count),
            usage_count: count,
        })
        .unwrap_or_default())
    }

    /// C# `UsageInfoService.Update`: increment count, `LastUseTime = DateTime.Now`.
    pub fn update_usage(&self, key: &str) -> Result<UsageInfo, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.update_usage(c,key).map_err(core_error);}
        let mut info = self.get_usage(key)?;
        info.usage_count += 1;
        info.last_use_time = Some(Utc::now());
        self.conn.execute(
            r#"INSERT INTO usage_info(id, last_use_time, usage_count) VALUES(?1,?2,?3)
               ON CONFLICT(id) DO UPDATE SET last_use_time=excluded.last_use_time, usage_count=excluded.usage_count"#,
            params![
                key,
                info.last_use_time.map(|t| t.timestamp_millis()).unwrap_or(0),
                info.usage_count
            ],
        )?;
        Ok(info)
    }

    /// C# `ProcessIconMetaDataService.TryGetExePath` stored row when the exe still exists.
    pub fn get_process_icon_exe(&self, name: &str) -> Result<Option<String>, StoreError> {
        let key = crate::file_icon::normalize_exe_key(name);
        if key.is_empty() {
            return Ok(None);
        }
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT exe_path FROM process_icon_meta WHERE id=?1",
                [&key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(path.filter(|p| Path::new(p).is_file()))
    }

    /// C# `ProcessIconMetaDataService.RememberForegroundMapping`.
    pub fn remember_process_icon(&self, name: &str, exe_full_path: &str) -> Result<(), StoreError> {
        let key = crate::file_icon::normalize_exe_key(name);
        let trimmed = exe_full_path.trim();
        if key.is_empty() || trimmed.is_empty() {
            return Ok(());
        }
        if !Path::new(trimmed).is_file() {
            return Ok(());
        }
        let full = std::path::absolute(trimmed)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| trimmed.to_string());
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT exe_path FROM process_icon_meta WHERE id=?1",
                [&key],
                |row| row.get(0),
            )
            .optional()?;
        let now = Utc::now().timestamp_millis();
        if existing
            .as_deref()
            .is_some_and(|old| old.eq_ignore_ascii_case(&full))
        {
            self.conn.execute(
                "UPDATE process_icon_meta SET update_time=?2 WHERE id=?1",
                params![key, now],
            )?;
            return Ok(());
        }
        self.conn.execute(
            r#"INSERT INTO process_icon_meta(id, exe_path, update_time) VALUES(?1,?2,?3)
               ON CONFLICT(id) DO UPDATE SET exe_path=excluded.exe_path, update_time=excluded.update_time"#,
            params![key, full, now],
        )?;
        Ok(())
    }

    fn overlay_clip_usage(&self, item: &mut ClipRecord) -> Result<(), StoreError> {
        let info = self.get_usage(&clip_usage_key(&item.id))?;
        if info.usage_count > 0 || info.last_use_time.is_some() {
            apply_usage(item, &info);
        }
        Ok(())
    }

    fn usage_map_by_prefix(&self, prefix: &str) -> Result<HashMap<String, UsageInfo>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, last_use_time, usage_count FROM usage_info WHERE id GLOB ?1",
        )?;
        let pattern = format!("{prefix}*");
        let rows = stmt.query_map([pattern], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i32>(2)?,
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (key, ms, count) = row?;
            if let Some(id) = key.strip_prefix(prefix) {
                map.insert(
                    id.to_string(),
                    UsageInfo {
                        last_use_time: usage_timestamp(ms, count),
                        usage_count: count,
                    },
                );
            }
        }
        Ok(map)
    }

    pub fn save_clip(&self, item: &ClipRecord) -> Result<(), StoreError> {
        if let Some((c,core))=self.conn.core(){return core.save_clip(c,item,None).map_err(core_error);}
        self.conn.verify_revision("clip_item", &item.id, item.storage_revision.as_deref())?;
        self.conn.execute(
            r#"UPDATE clip_item SET kind=?2, title=?3, body_text=?4, html_data=?5, file_paths=?6, image_path=?7,
                is_top=?8, create_time=?9, update_time=?10, source_process=?11, content_hash=?12, last_access=?13, usage_count=?14, extra_json=?15
                WHERE id=?1"#,
            params![
                item.id,
                item.kind.as_str(),
                item.title,
                item.body_text,
                item.html_data,
                serde_json::to_string(&item.file_paths)?,
                item.image_path,
                item.is_top as i32,
                item.create_time.timestamp_millis(),
                item.update_time.timestamp_millis(),
                item.source_process_name,
                item.content_hash,
                item.last_access.map(|t| t.timestamp_millis()),
                item.usage_count,
                serde_json::to_string(item)?,
            ],
        )?;
        Ok(())
    }

    pub fn delete_clip(&self, id: &str) -> Result<(), StoreError> {
        if let Some((c,core))=self.conn.core(){let n=core.delete_exact(c,"history",&[id.into()]).map_err(core_error)?;return if n==1{Ok(())}else{Err(StoreError::NotFound(id.into()))};}
        let transaction = self.conn.unchecked_transaction()?;
        let n = self.delete_clip_with_missing_status(id)?;
        if n == 0 {
            return Err(StoreError::NotFound(id.into()));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Caller owns the transaction; core storage never enters this legacy/native
    /// path or changes auxiliary state as a side effect of history removal.
    fn delete_clip_with_missing_status(&self, id: &str) -> Result<usize, StoreError> {
        let removed = self.conn.execute("DELETE FROM clip_item WHERE id = ?1", [id])?;
        if removed != 0 {
            self.kv_delete(&format!("file-history-missing-v1:{id}"))?;
        }
        Ok(removed)
    }

    pub fn delete_clips(&self, ids: &[String]) -> Result<usize, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.delete_exact(c,"history",ids).map_err(core_error);}
        let transaction = self.conn.unchecked_transaction()?;
        let mut n = 0;
        for id in ids {
            n += self.delete_clip_with_missing_status(id)?;
        }
        transaction.commit()?;
        Ok(n)
    }

    pub fn counts(&self) -> Result<(i32, i32), StoreError> {
        if let Some((c,_))=self.conn.core(){return Ok(c.query_row("SELECT COALESCE(sum(is_pinned=0),0),COALESCE(sum(is_pinned=1),0) FROM items WHERE scope='history'",[],|r|Ok((r.get(0)?,r.get(1)?)))?);}
        let pinned: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM clip_item WHERE is_top = 1",
            [],
            |row| row.get(0),
        )?;
        let non_pinned: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM clip_item WHERE is_top = 0",
            [],
            |row| row.get(0),
        )?;
        Ok((non_pinned, pinned))
    }

    pub fn count_unpinned_over_limit(&self, max_item_size: i32) -> Result<usize, StoreError> {
        let (non_pinned, _) = self.counts()?;
        let max = max_item_size.max(1);
        Ok((non_pinned - max).max(0) as usize)
    }

    pub fn enforce_max_unpinned(&self, max_item_size: i32) -> Result<Vec<String>, StoreError> {
        // C# `ClipDataService.CleanUpData`: Max(1, MaxItemSize), OrderByDescending CreateTime, ThenBy SubIndex.
        let max = max_item_size.max(1) as usize;
        let mut items: Vec<ClipRecord> = self
            .list_clips()?
            .into_iter()
            .filter(|item| !item.is_top)
            .collect();
        items.sort_by(|a, b| {
            b.create_time
                .cmp(&a.create_time)
                .then_with(|| a.sub_index.cmp(&b.sub_index))
        });
        let removed: Vec<_> = items.into_iter().skip(max).map(|item| item.id).collect();
        self.delete_clips(&removed)?;
        Ok(removed)
    }

    pub fn insert_favor(&self, item: &FavorRecord) -> Result<(), StoreError> {
        self.insert_favor_at(item, -1)
    }

    /// Copy authority is an explicit command argument. source_clip_id remains
    /// provenance and never chooses which mutable payload should be copied.
    pub fn insert_favor_from_source(&self,item:&FavorRecord,source_table:&str,source_id:&str,expected_source_revision:Option<&str>)->Result<(),StoreError>{
        let transaction=self.conn.unchecked_transaction()?;
        if let Some((c,core))=self.conn.core(){core.validate_source_revision(c,source_table,source_id,expected_source_revision).map_err(core_error)?;}
        else{self.conn.verify_revision(source_table,source_id,expected_source_revision)?;}
        self.insert_favor(item)?;
        self.conn.inherit_favorite_document(source_table,source_id,item)?;
        self.validate_favorite_graph()?;
        transaction.commit()?;
        Ok(())
    }

    /// C# `FavorItem.MoveTo` / `AddToParentChildren`: `-1` appends, else insert at clamped index.
    pub fn insert_favor_at(&self, item: &FavorRecord, index: i32) -> Result<(), StoreError> {
        if let Some((c,core))=self.conn.core(){
            let transaction=if c.is_autocommit(){Some(c.unchecked_transaction()?)}else{None};
            core.insert_favor(c,item).map_err(core_error)?;
            if let Some(parent)=&item.parent_id{let mut folder=self.get_favor(parent)?;if !folder.is_folder{return Err(StoreError::Core("Favorite owner is not a folder.".into()));}insert_child_at(&mut folder.children,&item.id,index);self.save_favor(&folder)?;}
            if let Some(transaction)=transaction{self.validate_favorite_graph()?;transaction.commit()?;}
            return Ok(());
        }
        self.conn.execute(
            r#"INSERT INTO favor_item
                (id, parent_id, is_folder, name, kind, body_text, file_paths, image_path, children, create_time, update_time, source_clip_id, html_data, html_source_url, qq_rich_text, source_process, sub_index, icon, execute_param)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)"#,
            params![
                item.id,
                item.parent_id,
                item.is_folder as i32,
                item.name,
                item.kind.as_str(),
                item.body_text,
                serde_json::to_string(&item.file_paths)?,
                item.image_path,
                serde_json::to_string(&item.children)?,
                item.create_time.timestamp_millis(),
                item.update_time.timestamp_millis(),
                item.source_clip_id,
                item.html_data,
                item.html_source_url,
                item.qq_rich_text,
                item.source_process_name,
                item.sub_index,
                item.icon,
                item.execute_param,
            ],
        )?;
        if let Some(parent) = &item.parent_id {
            match self.get_favor(parent) {
                Ok(mut folder) => {
                    insert_child_at(&mut folder.children, &item.id, index);
                    self.save_favor(&folder)?;
                }
                Err(StoreError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Append both the item and its parent link, or neither.
    pub fn insert_favor_checked(&self, item: &FavorRecord) -> Result<(), StoreError> {
        self.insert_favor_checked_at(item, -1)
    }

    pub fn insert_favor_checked_at(&self, item: &FavorRecord, index: i32) -> Result<(), StoreError> {
        self.insert_favors_checked_at(std::slice::from_ref(item), index)
    }

    pub fn insert_favors_checked_at(&self, items: &[FavorRecord], mut index: i32) -> Result<(), StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        for item in items {
            if let Some(parent) = &item.parent_id {
                if !self.get_favor(parent)?.is_folder {
                    return Err(StoreError::NotFound(parent.clone()));
                }
            }
            self.insert_favor_at(item, index)?;
            if index >= 0 { index = index.saturating_add(1); }
        }
        self.validate_favorite_graph()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn get_favor(&self, id: &str) -> Result<FavorRecord, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.get_favor(c,id).map_err(core_error);}
        self.conn
            .query_row(
                &format!("SELECT id, parent_id, is_folder, name, kind, body_text, file_paths, image_path, children, create_time, source_clip_id, html_data, html_source_url, qq_rich_text, update_time, source_process, sub_index, icon, execute_param, {} FROM favor_item WHERE id=?1", self.conn.revision_column()),
                [id],
                map_favor_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(id.into()))
    }

    pub fn upsert_favor(&self, item: &FavorRecord) -> Result<(), StoreError> {
        match self.get_favor(&item.id) {
            Ok(_) => self.save_favor(item),
            Err(StoreError::NotFound(_)) => self.insert_favor(item),
            Err(err) => Err(err),
        }
    }

    pub fn save_favor(&self, item: &FavorRecord) -> Result<(), StoreError> {
        if let Some((c,core))=self.conn.core(){return core.save_favor(c,item,None).map_err(core_error);}
        self.conn.verify_revision("favor_item", &item.id, item.storage_revision.as_deref())?;
        let updated = self.conn.execute(
            r#"UPDATE favor_item SET parent_id=?2, is_folder=?3, name=?4, kind=?5, body_text=?6, file_paths=?7, image_path=?8, children=?9, source_clip_id=?10, html_data=?11, html_source_url=?12, qq_rich_text=?13, update_time=?14, source_process=?15, sub_index=?16, icon=?17, execute_param=?18 WHERE id=?1"#,
            params![
                item.id,
                item.parent_id,
                item.is_folder as i32,
                item.name,
                item.kind.as_str(),
                item.body_text,
                serde_json::to_string(&item.file_paths)?,
                item.image_path,
                serde_json::to_string(&item.children)?,
                item.source_clip_id,
                item.html_data,
                item.html_source_url,
                item.qq_rich_text,
                item.update_time.timestamp_millis(),
                item.source_process_name,
                item.sub_index,
                item.icon,
                item.execute_param,
            ],
        )?;
        if updated != 1 { return Err(StoreError::NotFound(item.id.clone())); }
        Ok(())
    }

    pub fn save_favors_atomic(&self, items: &[FavorRecord]) -> Result<(), StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        for item in items {
            self.save_favor(item)?;
        }
        self.validate_favorite_graph()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_favor(&self, id: &str) -> Result<(), StoreError> {
        self.get_favor(id)?;
        self.delete_favors(&[id.to_string()])?;
        Ok(())
    }

    /// Delete each requested subtree and unlink its parent in one transaction.
    /// Parent IDs establish ownership; stale child-list references must not delete
    /// unrelated items. UNION also bounds traversal of a previously corrupt cycle.
    pub fn delete_favors(&self, ids: &[String]) -> Result<Vec<String>, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.delete_favors(c,ids).map_err(core_error);}
        let transaction = self.conn.unchecked_transaction()?;
        let mut removed = Vec::new();
        let mut deleted = std::collections::HashSet::new();
        for id in ids {
            match self.get_favor(id) {
                Ok(_) => {}
                Err(StoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            }
            let descendants = {
                let mut stmt = self.conn.prepare(
                    "WITH RECURSIVE subtree(id) AS (SELECT id FROM favor_item WHERE id=?1 UNION SELECT f.id FROM favor_item f JOIN subtree s ON f.parent_id=s.id) SELECT id FROM subtree"
                )?;
                let rows = stmt.query_map([id], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for child in descendants.iter().rev() {
                self.conn.execute("DELETE FROM favor_item WHERE id=?1", [child])?;
            }
            deleted.extend(descendants);
            removed.push(id.clone());
        }
        // Ownership follows ParentId, but shared or moved memberships live in
        // other folders' Children. Drop every edge to a deleted row so no
        // folder keeps a "missing favorite reference".
        if !deleted.is_empty() {
            for mut folder in self.list_favors()? {
                let before = folder.children.len();
                folder.children.retain(|child| !deleted.contains(child));
                if folder.children.len() != before {
                    self.save_favor(&folder)?;
                }
            }
        }
        transaction.commit()?;
        Ok(removed)
    }

    pub fn replace_favors(&self, items: &[FavorRecord]) -> Result<(), StoreError> {
        if self.is_core_store(){return Err(StoreError::Core("A flat favorite replacement cannot preserve core payloads/extensions; an explicit versioned import is required.".into()));}
        self.conn.execute("DELETE FROM favor_item", [])?;
        for item in items {
            self.conn.execute(
                r#"INSERT INTO favor_item
                    (id, parent_id, is_folder, name, kind, body_text, file_paths, image_path, children, create_time, update_time, source_clip_id, html_data, html_source_url, qq_rich_text, source_process, sub_index, icon, execute_param)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)"#,
                params![
                    item.id,
                    item.parent_id,
                    item.is_folder as i32,
                    item.name,
                    item.kind.as_str(),
                    item.body_text,
                    serde_json::to_string(&item.file_paths)?,
                    item.image_path,
                    serde_json::to_string(&item.children)?,
                    item.create_time.timestamp_millis(),
                    item.update_time.timestamp_millis(),
                    item.source_clip_id,
                    item.html_data,
                    item.html_source_url,
                    item.qq_rich_text,
                    item.source_process_name,
                    item.sub_index,
                    item.icon,
                    item.execute_param,
                ],
            )?;
        }
        Ok(())
    }

    pub fn list_favors(&self) -> Result<Vec<FavorRecord>, StoreError> {
        if let Some((c,core))=self.conn.core(){return core.list_favors(c).map_err(core_error);}
        // One statement with `get_favor`'s columns: a per-ID lookup made every
        // tree/revision read quadratic in practice (drag refresh latency).
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, parent_id, is_folder, name, kind, body_text, file_paths, image_path, children, create_time, source_clip_id, html_data, html_source_url, qq_rich_text, update_time, source_process, sub_index, icon, execute_param, {} FROM favor_item ORDER BY create_time ASC",
            self.conn.revision_column()
        ))?;
        let items = stmt.query_map([], map_favor_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key=?1", [key], |row| row.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM kv WHERE key=?1", [key])?;
        Ok(())
    }

    /// The database receipt is authoritative if the process exits before the
    /// device sync state file is replaced. Keep it when the user deletes the
    /// associated history row so delivery retries never resurrect that row.
    pub(crate) fn commit_device_sync_receipt(&self, key: &str, receipt: &str, item: Option<&ClipRecord>) -> Result<bool, StoreError> {
        self.commit_device_sync_receipt_inner(key, receipt, item, None)
    }

    pub(crate) fn commit_device_sync_receipt_prepared(
        &self,
        key: &str,
        receipt: &str,
        item: Option<&ClipRecord>,
        prepared: &PreparedImageIdentities,
    ) -> Result<bool, StoreError> {
        self.commit_device_sync_receipt_inner(key, receipt, item, Some(prepared))
    }

    fn commit_device_sync_receipt_inner(
        &self,
        key: &str,
        receipt: &str,
        item: Option<&ClipRecord>,
        prepared: Option<&PreparedImageIdentities>,
    ) -> Result<bool, StoreError> {
        if !key.starts_with("device-sync-receipt:") {
            return Err(StoreError::Core("Invalid device sync receipt key.".into()));
        }
        let mut value: serde_json::Value = serde_json::from_str(receipt)?;
        let origin = value["origin"].as_str().filter(|value| !value.is_empty())
            .ok_or_else(|| StoreError::Core("Device sync receipt has no origin.".into()))?.to_owned();
        let sequence = value["sequence"].as_u64()
            .ok_or_else(|| StoreError::Core("Device sync receipt has no sequence.".into()))?;
        let transaction = self.conn.unchecked_transaction()?;
        if self.kv_get(key)?.is_some() {
            return Ok(false);
        }
        if let Some(item) = item {
            // A native third-party clipboard capture may commit after the
            // receiver's earlier image/text snapshot. Recheck strict equality
            // in this transaction; no file reads or image decoding occur here.
            let recent = self.list_recent_by_create_time()?;
            if let Some(prepared) = prepared {
                self.guard_prepared_image_candidates(item, &recent, prepared, &|_| false)?;
            }
            let mut existing = None;
            for candidate in recent {
                if candidate.kind != item.kind {
                    continue;
                }
                let equal = match item.kind {
                    ClipKind::TextClipItem => candidate.body_text == item.body_text,
                    ClipKind::ImageClipItem if prepared.is_some() => {
                        self.images_equal_exact(&candidate, item)?
                    }
                    ClipKind::ImageClipItem => {
                        !item.content_hash.is_empty()
                            && candidate.content_hash.eq_ignore_ascii_case(&item.content_hash)
                    }
                    _ => false,
                };
                if equal {
                    existing = Some(candidate);
                    break;
                }
            }
            if let Some(existing)=existing {
                value["ack"]["history"]=serde_json::json!("duplicate");
                value["ack"]["historyId"]=serde_json::json!(existing.id);
            } else {self.insert_clip(item, true)?;}
        }
        self.kv_set(key, &serde_json::to_string(&value)?)?;
        let high_water_key = format!("device-sync-high-water:{origin}");
        let previous = self.kv_get(&high_water_key)?.map(|value| value.parse::<u64>())
            .transpose().map_err(|error| StoreError::Core(format!("Invalid device sync high water: {error}")))?
            .unwrap_or(0);
        self.kv_set(&high_water_key, &previous.max(sequence).to_string())?;
        self.conn.execute(
            "DELETE FROM kv WHERE key IN (SELECT key FROM kv WHERE key GLOB 'device-sync-receipt:*' ORDER BY rowid DESC LIMIT -1 OFFSET 1024)",
            [],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn upsert_json(&self, table: &str, id: &str, payload: &str) -> Result<(), StoreError> {
        self.validate_json_table(table)?;
        let sql = format!(
            "INSERT INTO {table}(id, payload) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload"
        );
        self.conn.execute(&sql, params![id, payload])?;
        Ok(())
    }

    pub fn get_json(&self, table: &str, id: &str) -> Result<Option<String>, StoreError> {
        self.validate_json_table(table)?;
        let sql = format!("SELECT payload FROM {table} WHERE id=?1");
        Ok(self.conn.query_row(&sql, [id], |row| row.get(0)).optional()?)
    }

    pub fn delete_json(&self, table: &str, id: &str) -> Result<(), StoreError> {
        self.validate_json_table(table)?;
        let sql = format!("DELETE FROM {table} WHERE id=?1");
        self.conn.execute(&sql, [id])?;
        Ok(())
    }

    pub fn list_json(&self, table: &str) -> Result<Vec<String>, StoreError> {
        self.validate_json_table(table)?;
        let sql = format!("SELECT payload FROM {table}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
    fn validate_json_table(&self, table:&str)->Result<(),StoreError>{
        if self.is_core_store() && !matches!(table,"text_action"|"lan_peer"|"lan_conversation"|"lan_message"|"collect_draft") {
            return Err(StoreError::Core("Unknown auxiliary JSON domain.".into()));
        }
        Ok(())
    }

    pub fn has_lan_message(&self, id: &str) -> Result<bool, StoreError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(1) FROM lan_message WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn insert_lan_message(&self, id: &str, conversation_id: &str, payload: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO lan_message(id, conversation_id, payload, created_at) VALUES(?1,?2,?3,?4)",
            params![id, conversation_id, payload, Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn get_lan_message(&self, id: &str) -> Result<String, StoreError> {
        self.conn
            .query_row(
                "SELECT payload FROM lan_message WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn update_lan_message(&self, id: &str, payload: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE lan_message SET payload=?2 WHERE id=?1",
            params![id, payload],
        )?;
        Ok(())
    }

    /// Publish the optional history item and its durable receipt atomically.
    /// A completed receipt survives history deletion; a delivery retry must not
    /// recreate an item that the user or history cleanup already removed.
    pub(crate) fn import_lan_received_file_clip(&self, message_id: &str, clip: &ClipRecord) -> Result<bool, StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        let mut payload: serde_json::Value = serde_json::from_str(&self.get_lan_message(message_id)?)?;
        if payload["historyImportRequested"] != true || payload["historyImportCompleted"] == true {
            return Ok(false);
        }
        self.insert_clip(clip, true)?;
        payload["historyItemId"] = serde_json::json!(clip.id);
        payload["historyImportCompleted"] = serde_json::json!(true);
        if let Some(object) = payload.as_object_mut() {
            object.remove("historyImportError");
        }
        self.update_lan_message(message_id, &payload.to_string())?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn list_lan_messages(&self, conversation_id: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM lan_message WHERE conversation_id=?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([conversation_id], |row| row.get(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Read the visible tail without loading and decoding the entire conversation.
    /// Invalid JSON never consumes the caller's limit, matching the full-history path.
    pub fn list_recent_lan_messages(&self, conversation_id: &str, limit: usize) -> Result<Vec<serde_json::Value>, StoreError> {
        if limit == 0 { return Ok(Vec::new()); }
        let batch_size = limit.min(500);
        let mut items = Vec::with_capacity(batch_size);
        let mut offset = 0i64;
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM lan_message WHERE conversation_id=?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2 OFFSET ?3",
        )?;
        while items.len() < limit {
            let rows = stmt.query_map(params![conversation_id, batch_size as i64, offset], |row| row.get::<_, String>(0))?;
            let mut row_count = 0;
            for row in rows {
                row_count += 1;
                let raw = row?;
                if let Ok(payload) = serde_json::from_str(&raw) {
                    items.push(payload);
                    if items.len() == limit { break; }
                }
            }
            if row_count < batch_size || items.len() == limit { break; }
            offset += row_count as i64;
        }
        items.reverse();
        Ok(items)
    }

    pub fn upsert_explorer_record(&self, path: &str) -> Result<(), StoreError> {
        self.upsert_explorer_record_with_limit(path, crate::extra_settings::DEFAULT_REOPEN_HISTORY_LIMIT as usize).map(|_| ())
    }

    pub(crate) fn upsert_explorer_record_with_limit(&self, path: &str, limit: usize) -> Result<bool, StoreError> {
        self.upsert_explorer_record_at(path, limit, Utc::now().timestamp_millis())
    }

    fn upsert_explorer_record_at(&self, path: &str, limit: usize, now: i64) -> Result<bool, StoreError> {
        let path = path.trim();
        if path.is_empty() {
            return Ok(false);
        }
        self.import_legacy_explorer_history()?;
        let transaction = self.conn.unchecked_transaction()?;
        // Evaluate rules while holding the same SQLite snapshot used for the
        // write. A rule accepted by the manager cannot race a late capture.
        if self.list_explorer_exclusions()?.iter().any(|rule| rule.matches(path)) {
            transaction.commit()?;
            return Ok(false);
        }
        let records = self.list_explorer_history_entries_unimported()?;
        let key = crate::reopen_history::directory_key(path);
        let path = records.iter().find(|entry| entry.path == path)
            .or_else(|| records.iter().find(|entry| crate::reopen_history::directory_key(&entry.path) == key))
            .map(|entry| entry.path.as_str()).unwrap_or(path);
        // One polling snapshot can close several tabs within a millisecond.
        // Keep a strict recording order so a low cap cannot discard a newer
        // folder, including an existing path moved back to the front.
        let latest: Option<i64> = self.conn.query_row(
            "SELECT MAX(update_time) FROM explorer_record", [], |row| row.get(0),
        )?;
        let now = now.max(latest.map(|time| time.saturating_add(1)).unwrap_or(i64::MIN));
        self.conn.execute(
            "INSERT INTO explorer_record(path, update_time) VALUES(?1, ?2)
             ON CONFLICT(path) DO UPDATE SET update_time=excluded.update_time",
            params![path, now],
        )?;
        let extra: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT path FROM explorer_record ORDER BY update_time DESC",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?.into_iter()
                .skip(limit.clamp(1, crate::extra_settings::MAX_REOPEN_HISTORY_LIMIT as usize)).collect()
        };
        for path in extra {
            self.conn
                .execute("DELETE FROM explorer_record WHERE path=?1", [&path])?;
        }
        transaction.commit()?;
        Ok(true)
    }

    pub fn list_explorer_records(&self) -> Result<Vec<String>, StoreError> {
        Ok(self.list_explorer_history_entries()?.into_iter().map(|entry| entry.path).collect())
    }

    pub fn delete_explorer_record(&self, path: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM explorer_record WHERE path=?1", [path])?;
        Ok(())
    }
}

pub fn hash_file_list(paths: &[String]) -> String {
    hash_files(paths)
}

fn same_file_history_snapshot(a: &ClipRecord, b: &ClipRecord) -> Result<bool, StoreError> {
    // storage_revision is intentionally omitted by serde and must be checked
    // separately. Full payload comparison also protects native rows without a
    // document revision and prevents path edits/pins from inheriting old checks.
    Ok(a.storage_revision == b.storage_revision && serde_json::to_value(a)? == serde_json::to_value(b)?)
}

#[cfg(test)]
#[path = "file_history_store_tests.rs"]
mod file_history_store_tests;

fn apply_usage(item: &mut ClipRecord, info: &UsageInfo) {
    item.usage_count = info.usage_count;
    item.last_access = info.last_use_time;
}

fn usage_timestamp(ms: i64, count: i32) -> Option<DateTime<Utc>> {
    if ms==0 && count==0 { None } else { Utc.timestamp_millis_opt(ms).single() }
}

fn map_favor_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FavorRecord> {
    let create_time = Utc.timestamp_millis_opt(row.get(9)?).single().unwrap_or_else(Utc::now);
    let update_time = row
        .get::<_, Option<i64>>(14)?
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or(create_time);
    Ok(FavorRecord {
        storage_revision: row.get(19)?,
        id: row.get(0)?,
        parent_id: row.get(1)?,
        is_folder: row.get::<_, i32>(2)? != 0,
        name: row.get(3)?,
        kind: ClipKind::parse(&row.get::<_, String>(4)?),
        body_text: row.get(5)?,
        file_paths: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
        image_path: row.get(7)?,
        children: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default(),
        create_time,
        update_time,
        source_clip_id: row.get(10)?,
        html_data: row.get(11)?,
        html_source_url: row.get(12)?,
        qq_rich_text: row.get(13)?,
        source_process_name: row.get::<_, Option<String>>(15)?.unwrap_or_default(),
        sub_index: row.get::<_, Option<i32>>(16)?.unwrap_or(0),
        icon: row.get::<_, Option<String>>(17)?.unwrap_or_default(),
        execute_param: row.get::<_, Option<String>>(18)?.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClipRecord;
    use tempfile::tempdir;

    fn lan_performance_store() -> (tempfile::TempDir, AppStore) {
        let temporary_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/lan-perf-20261018");
        std::fs::create_dir_all(&temporary_root).unwrap();
        let dir = tempfile::Builder::new().prefix("core-store-").tempdir_in(temporary_root).unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn lan_recent_messages_preserves_order_and_skips_invalid_json_before_limiting() {
        let (_dir, store) = lan_performance_store();
        for (index, payload, created_at) in [
            (0, r#"{"id":"old"}"#, 10),
            (1, r#"{"id":"first-tie"}"#, 30),
            (2, r#"{"id":"middle"}"#, 20),
            (3, r#"{"id":"second-tie"}"#, 30),
            (4, "[invalid", 40),
            (5, "null", 40),
            (6, "1", 40),
            (7, "{invalid", 50),
            (8, "invalid", 50),
            (9, "[", 50),
        ] {
            store.conn.execute(
                "INSERT INTO lan_message(id, conversation_id, payload, created_at) VALUES(?1, 'dm:fixture', ?2, ?3)",
                params![format!("message-{index}"), payload, created_at],
            ).unwrap();
        }
        store.insert_lan_message("other", "dm:other", r#"{"id":"other"}"#).unwrap();
        let expected = vec![
            serde_json::json!({"id":"old"}), serde_json::json!({"id":"middle"}),
            serde_json::json!({"id":"first-tie"}), serde_json::json!({"id":"second-tie"}),
            serde_json::Value::Null, serde_json::json!(1),
        ];
        for limit in [0, 1, 2, 3, 5, 6, 200] {
            let start = expected.len().saturating_sub(limit);
            assert_eq!(store.list_recent_lan_messages("dm:fixture", limit).unwrap(), expected[start..], "limit={limit}");
        }
        assert!(store.list_recent_lan_messages("missing", 200).unwrap().is_empty());
    }

    #[test]
    fn lan_recent_messages_reports_sqlite_payload_read_errors() {
        let (_dir, store) = lan_performance_store();
        store.conn.execute(
            "INSERT INTO lan_message(id, conversation_id, payload, created_at) VALUES('bad-storage-type', 'dm:fixture', x'00ff', 1)", [],
        ).unwrap();
        assert!(matches!(store.list_recent_lan_messages("dm:fixture", 200), Err(StoreError::Sqlite(_))));
    }

    #[test]
    fn lan_recent_messages_large_fixture_matches_full_history_tail() {
        let (_dir, store) = lan_performance_store();
        let text = "synthetic fixture ".repeat(128);
        store.conn.execute_batch("BEGIN").unwrap();
        for index in 0..4000 {
            store.conn.execute(
                "INSERT INTO lan_message(id, conversation_id, payload, created_at) VALUES(?1, 'dm:fixture', ?2, ?3)",
                params![format!("message-{index}"), serde_json::json!({"id":index,"text":text}).to_string(), index / 4],
            ).unwrap();
        }
        store.conn.execute_batch("COMMIT").unwrap();
        let began = std::time::Instant::now();
        let full: Vec<serde_json::Value> = store.list_lan_messages("dm:fixture").unwrap().into_iter()
            .filter_map(|raw| serde_json::from_str(&raw).ok()).collect();
        let full_ms = began.elapsed().as_secs_f64() * 1000.0;
        let began = std::time::Instant::now();
        let recent = store.list_recent_lan_messages("dm:fixture", 200).unwrap();
        let recent_ms = began.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(recent, full[3800..]);
        println!("LAN_CORE_PERF {}", serde_json::json!({
            "fixtureOnly":true,"historyCount":full.len(),"returnedCount":recent.len(),
            "fullReadAndDecodeMs":full_ms,"recentReadAndDecodeMs":recent_ms,
            "schemaChanged":false,"resultEquivalent":true,
        }));
    }

    #[test]
    fn explorer_history_new_records_apply_configured_cap_and_preserve_default_200() {
        let temporary_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temporary_root).unwrap();
        let dir = tempfile::Builder::new().prefix("explorer-history-store-").tempdir_in(temporary_root).unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        for index in 0..205 {
            store.conn.execute("INSERT INTO explorer_record(path, update_time) VALUES(?1, ?2)",
                params![format!("shell:::fixture-{index}"), index]).unwrap();
        }
        store.upsert_explorer_record("shell:::default-new").unwrap();
        let records = store.list_explorer_records().unwrap();
        assert_eq!(records.len(), 200);
        assert_eq!(records[0], "shell:::default-new");
        store.upsert_explorer_record_with_limit("  ", 3).unwrap();
        assert_eq!(store.list_explorer_records().unwrap().len(), 200, "no record means no cleanup");
        store.upsert_explorer_record_with_limit("shell:::custom-new", 3).unwrap();
        let records = store.list_explorer_records().unwrap();
        assert_eq!(records.len(), 3);
        assert!(records.iter().any(|path| path == "shell:::custom-new"));
        store.upsert_explorer_record_with_limit("shell:::custom-new", 3).unwrap();
        assert_eq!(store.list_explorer_records().unwrap().len(), 3, "repeated paths are updated rather than duplicated");
    }

    #[test]
    fn explorer_history_same_millisecond_keeps_latest_record_and_moves_repeated_path_first() {
        let temporary_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temporary_root).unwrap();
        let dir = tempfile::Builder::new().prefix("explorer-history-order-").tempdir_in(temporary_root).unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        store.upsert_explorer_record_at("shell:::first", 2, 1000).unwrap();
        store.upsert_explorer_record_at("shell:::second", 2, 1000).unwrap();
        assert_eq!(store.list_explorer_records().unwrap(), ["shell:::second", "shell:::first"]);
        store.upsert_explorer_record_at("shell:::first", 2, 1000).unwrap();
        assert_eq!(store.list_explorer_records().unwrap(), ["shell:::first", "shell:::second"]);
        store.upsert_explorer_record_at("shell:::latest", 1, 1000).unwrap();
        assert_eq!(store.list_explorer_records().unwrap(), ["shell:::latest"]);
        let recorded_at: i64 = store.conn.query_row(
            "SELECT update_time FROM explorer_record WHERE path='shell:::latest'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(recorded_at, 1003);
        store.upsert_explorer_record_at("shell:::latest", 1, i64::MAX).unwrap();
        store.upsert_explorer_record_at("shell:::latest", 1, 1000).unwrap();
        let saturated: i64 = store.conn.query_row(
            "SELECT update_time FROM explorer_record WHERE path='shell:::latest'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(saturated, i64::MAX, "out-of-range persisted timestamps must saturate without panicking");
    }

    #[test]
    fn quick_paste_bounded_candidates_match_full_history_limits_ties_pins_and_usage() {
        let dir = tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 10, 3, 0, 0, 0).unwrap();
        for index in 0..88 {
            let mut item = ClipRecord::new_text(format!("quick-paste-{index}"), "fixture");
            item.id = format!("qp-bounded-{index:02}");
            item.is_top = index >= 80;
            // Multiple records share one indexed millisecond, with full date
            // and SubIndex ties crossing the requested list boundary.
            item.create_time = base + chrono::Duration::seconds((index / 8) as i64)
                + chrono::Duration::nanoseconds(((index % 8) / 3) as i64 * 100_000);
            item.update_time = item.create_time;
            item.sub_index = (index % 2) as i32;
            item.usage_count = index as i32;
            store.insert_clip(&item, true).unwrap();
        }
        // The persisted usage service must override JSON while rows without
        // a usage entry retain their original payload fields.
        for index in [0, 75, 79, 83, 87] {
            let key = clip_usage_key(&format!("qp-bounded-{index:02}"));
            store.update_usage(&key).unwrap();
            store.update_usage(&key).unwrap();
        }
        let full = store.list_clips().unwrap();
        for limit in [-1, 3, 4, 7, 25, 50, 99] {
            let expected = crate::quick_paste::select_session_items(&full, limit);
            let actual = store.list_quick_paste_candidates(limit).unwrap();
            assert_eq!(serde_json::to_value(&actual).unwrap(), serde_json::to_value(&expected).unwrap(),
                "bounded candidate semantics changed at limit {limit}");
            assert_eq!(actual.iter().filter(|item| !item.is_top).count(), crate::quick_paste::clamp_list_limit(limit) as usize);
            assert_eq!(actual.iter().filter(|item| item.is_top).count(), 8);
        }
        // Fewer normal records than the limit still return every remaining row.
        for index in 0..78 {
            store.delete_clip(&format!("qp-bounded-{index:02}")).unwrap();
        }
        let remaining = store.list_clips().unwrap();
        assert_eq!(serde_json::to_value(store.list_quick_paste_candidates(25).unwrap()).unwrap(),
            serde_json::to_value(crate::quick_paste::select_session_items(&remaining, 25)).unwrap());
    }

    #[test]
    fn quicker_drop_insert_rolls_back_if_parent_link_cannot_be_saved() {
        let dir = tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let clip = ClipRecord::new_text("fixture", "");
        let mut folder = FavorRecord::from_clip(&clip, None);
        folder.is_folder = true;
        store.insert_favor(&folder).unwrap();
        let item = FavorRecord::from_clip(&clip, Some(folder.id.clone()));
        store.conn.execute_batch("CREATE TRIGGER reject_parent BEFORE UPDATE ON favor_item WHEN OLD.is_folder=1 BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
        assert!(store.insert_favor_checked(&item).is_err());
        assert!(store.get_favor(&item.id).is_err());
        assert!(store.get_favor(&folder.id).unwrap().children.is_empty());
        store.conn.execute_batch("DROP TRIGGER reject_parent;").unwrap();
        store.insert_favor_checked(&item).unwrap();
        assert_eq!(store.get_favor(&folder.id).unwrap().children, [item.id]);
    }

    #[test]
    fn usage_survives_clip_delete_like_csharp() {
        let dir = tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let rec = ClipRecord::new_text("keep-usage", "t");
        let id = rec.id.clone();
        store.insert_clip(&rec, true).unwrap();
        store.update_usage(&clip_usage_key(&id)).unwrap();
        assert_eq!(store.get_clip(&id).unwrap().usage_count, 1);
        store.delete_clip(&id).unwrap();
        assert_eq!(store.get_usage(&clip_usage_key(&id)).unwrap().usage_count, 1);
        store.insert_clip(&rec, true).unwrap();
        assert_eq!(store.get_clip(&id).unwrap().usage_count, 1);
        assert!(store.get_clip(&id).unwrap().last_access.is_some());
    }

    #[test]
    fn persist_capture_merges_and_skips_deferred_hidden() {
        let dir = tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let first = ClipRecord::new_text("Hello\r\n", "chrome");
        let first_id = first.id.clone();
        store.insert_clip(&first, true).unwrap();
        let older = store.get_clip(&first_id).unwrap().create_time;
        std::thread::sleep(std::time::Duration::from_millis(5));
        let incoming = ClipRecord::new_text("Hello", "chrome");
        let merged = store
            .persist_capture(&incoming, false, |_| false)
            .unwrap();
        assert_eq!(merged.id, first_id);
        assert!(merged.create_time > older);
        assert_eq!(store.list_clips().unwrap().len(), 1);

        let hidden = ClipRecord::new_text("hidden-dup", "t");
        let hidden_id = hidden.id.clone();
        store.insert_clip(&hidden, true).unwrap();
        let incoming_hidden = ClipRecord::new_text("hidden-dup", "t");
        let created = store
            .persist_capture(&incoming_hidden, false, |id| id == hidden_id)
            .unwrap();
        assert_ne!(created.id, hidden_id);
        assert_eq!(store.list_clips().unwrap().len(), 3);

        let allowed = ClipRecord::new_text("Hello", "chrome");
        let allowed_id = allowed.id.clone();
        store.persist_capture(&allowed, true, |_| false).unwrap();
        assert_eq!(store.list_clips().unwrap().len(), 4);
        assert!(store.get_clip(&allowed_id).is_ok());
    }

    #[test]
    fn device_sync_database_receipt_is_atomic_bounded_and_survives_deletion() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/device-sync-receipt-tests");
        std::fs::create_dir_all(&root).unwrap();
        for documents in [false, true] {
            let directory = tempfile::tempdir_in(&root).unwrap();
            let path = directory.path().join("userdata.db");
            if documents {
                rusqlite::Connection::open(&path).unwrap().execute_batch(
                    "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
                     CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
                     CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);"
                ).unwrap();
            }
            let store = AppStore::open(&path).unwrap();
            let mut database = rusqlite::Connection::open(&path).unwrap();
            let table = if documents { "cliphost_kv" } else { "kv" };
            database.execute_batch(&format!(
                "CREATE TRIGGER fail_device_receipt BEFORE INSERT ON {table} WHEN NEW.key GLOB 'device-sync-receipt:*' BEGIN SELECT RAISE(ABORT,'receipt fixture failure'); END;"
            )).unwrap();
            let item = ClipRecord::new_text("received", "ClipHost Device Sync");
            let key = "device-sync-receipt:fixture";
            let receipt = r#"{"origin":"peer","sequence":7,"id":"fixture","hash":"test","ack":{"status":"accepted"}}"#;
            assert!(store.commit_device_sync_receipt(key, receipt, Some(&item)).is_err());
            assert!(store.list_clips().unwrap().is_empty(), "receipt failure must roll back the item on both storage formats");
            assert!(store.kv_get(key).unwrap().is_none());
            database.execute_batch("DROP TRIGGER fail_device_receipt;").unwrap();
            assert!(store.commit_device_sync_receipt(key, receipt, Some(&item)).unwrap());
            assert_eq!(store.kv_get("device-sync-high-water:peer").unwrap().as_deref(), Some("7"));
            store.delete_clip(&item.id).unwrap();
            assert!(!store.commit_device_sync_receipt(key, receipt, Some(&item)).unwrap());
            assert!(store.list_clips().unwrap().is_empty());
            store.kv_set("unrelated-setting", "preserve").unwrap();
            let transaction = database.transaction().unwrap();
            for index in 0..1024 {
                transaction.execute(&format!("INSERT INTO {table}(key,value) VALUES(?1,'{{}}')"),
                    [format!("device-sync-receipt:prefilled-{index}")]).unwrap();
            }
            transaction.commit().unwrap();
            let newest = r#"{"origin":"peer","sequence":11,"id":"newest","hash":"test","ack":{"status":"accepted"}}"#;
            assert!(store.commit_device_sync_receipt("device-sync-receipt:newest", newest, None).unwrap());
            assert!(store.kv_get(key).unwrap().is_none(), "old bounded receipts must be evicted");
            assert_eq!(store.kv_get("device-sync-high-water:peer").unwrap().as_deref(), Some("11"));
            assert_eq!(store.kv_get("unrelated-setting").unwrap().as_deref(), Some("preserve"));
            let count: i64 = database.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE key GLOB 'device-sync-receipt:*'"), [], |row| row.get(0)).unwrap();
            assert_eq!(count, 1024);
        }
    }

    #[test]
    fn device_sync_final_commit_rechecks_a_native_copy_after_the_receiver_snapshot(){
        let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/device-sync-final-dedupe-tests");
        std::fs::create_dir_all(&root).unwrap();let directory=tempfile::tempdir_in(root).unwrap();
        let store=AppStore::open(&directory.path().join("userdata.db")).unwrap();
        // The receiver prepared an insert from its earlier empty snapshot.
        assert!(store.list_recent_by_create_time().unwrap().is_empty());
        let incoming=ClipRecord::new_text(" raw\r\ntext ","ClipHost Device Sync");
        // A third-party synchronized copy lands just before the final commit.
        let native=ClipRecord::new_text(" raw\r\ntext ","native clipboard");
        store.insert_clip(&native,true).unwrap();
        let receipt=serde_json::json!({"origin":"peer","sequence":1,"id":"race","hash":"test","ack":{"status":"accepted","history":"saved","historyId":incoming.id}});
        assert!(store.commit_device_sync_receipt("device-sync-receipt:race",&receipt.to_string(),Some(&incoming)).unwrap());
        let saved:serde_json::Value=serde_json::from_str(&store.kv_get("device-sync-receipt:race").unwrap().unwrap()).unwrap();
        assert_eq!(saved["ack"]["history"],"duplicate");assert_eq!(saved["ack"]["historyId"],native.id);
        assert_eq!(store.list_clips().unwrap().len(),1);assert!(store.get_clip(&incoming.id).is_err());
        // A whitespace change is a different strict network payload.
        let changed=ClipRecord::new_text("raw\r\ntext","ClipHost Device Sync");
        let receipt=serde_json::json!({"origin":"peer","sequence":2,"id":"changed","hash":"test","ack":{"status":"accepted","history":"saved","historyId":changed.id}});
        assert!(store.commit_device_sync_receipt("device-sync-receipt:changed",&receipt.to_string(),Some(&changed)).unwrap());
        assert_eq!(store.list_clips().unwrap().len(),2);
    }

    #[test]
    fn process_icon_meta_remembers_existing_exe_like_csharp() {
        let dir = tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let exe = dir.path().join("ghost.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        store
            .remember_process_icon("ghost", exe.to_str().unwrap())
            .unwrap();
        let stored = store.get_process_icon_exe("ghost.exe").unwrap().unwrap();
        assert!(stored.ends_with("ghost.exe") || stored.to_ascii_lowercase().ends_with("ghost.exe"));
        assert!(std::path::Path::new(&stored).is_file());
        store.remember_process_icon("ghost.exe", r"C:\missing-cliphost-icon.exe").unwrap();
        assert_eq!(
            store.get_process_icon_exe("GHOST.exe").unwrap(),
            None,
            "C# Normalize keeps case: GHOST.exe is a different key"
        );
        std::fs::remove_file(&exe).unwrap();
        assert_eq!(store.get_process_icon_exe("ghost.exe").unwrap(), None);
    }
}
