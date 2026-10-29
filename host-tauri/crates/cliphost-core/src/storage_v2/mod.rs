//! Offline v2 candidate construction. Never selects or changes the live store.
//! The current product deliberately rejects v2 until its reader/writer is ready.
mod assets;
#[cfg(test)]
mod benchmarks;
pub(crate) mod convert;
pub(crate) mod protected_archive;
#[cfg(test)]
mod tests;

use rusqlite::{
    backup::{Backup, StepResult},
    params, Connection, DatabaseName, OpenFlags,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const CORE_SCHEMA: &str = include_str!("../../../../docs/storage-v2.sql");
const UNSUPPORTED: &str =
    "STORAGE_V2_RUNTIME_NOT_READY: 本版本尚未支持新数据格式；未创建空库或回退旧表。";
// Conversion currently materializes original and converted rows. Fail safely
// before allocation instead of pretending this candidate tool streams any size.
const MAX_CANDIDATE_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct MigrationOptions {
    pub source_root: PathBuf,
    /// Must not exist. No merge, replacement, implicit production path or move.
    pub candidate_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationProgress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
    pub unit: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateReport {
    pub operation_id: String,
    pub dataset_id: String,
    pub source_format: String,
    pub history_count: usize,
    pub favorite_count: usize,
    pub asset_count: usize,
    pub missing_references: usize,
    pub unresolved_references: usize,
    pub orphan_usage_count: usize,
    pub source_snapshot_sha256: String,
    pub source_archive_path: String,
    pub source_archive_sha256: String,
    pub source_archive_protected: bool,
    pub candidate_database_sha256: String,
    pub schema_sha256: String,
    pub archive_files: usize,
    pub elapsed_ms: u64,
    /// This flag cannot be opted out of: v2 runtime/cutover is a separate gate.
    pub candidate_only: bool,
    pub production_ready: bool,
    pub blockers: Vec<String>,
    pub phase_timings_ms: BTreeMap<String, u64>,
}

/// Check before AppStore migrations or any host service initialization.
pub(crate) fn reject_unsupported_runtime(connection: &Connection) -> Result<(), String> {
    if has_unsupported_runtime(connection).map_err(|e| e.to_string())? {
        return Err(UNSUPPORTED.into());
    }
    Ok(())
}

fn has_unsupported_runtime(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND lower(name) IN ('store_info','storage_v2_candidate'))", [], |r| r.get(0)
    )
}

/// Runtime startup may recover SQLite transactions, but must reject future
/// schemas before business migrations or configuration compatibility writes.
/// The offline candidate builder retains its separate, read-only source policy.
pub(crate) fn check_runtime_database(path: &Path) -> Result<(), String> {
    match fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("数据库启动检查失败 [metadata] {}: {error}", path.display())),
    }
    // Even a schema SELECT needs write access to roll back a hot journal left
    // by a crashed writer. No CREATE: a disappearing file must not become an
    // empty replacement. SQLite alone performs recovery; never remove sidecars.
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).map_err(|error| runtime_database_error(path, "open", &error))?;
    connection.busy_timeout(Duration::from_secs(2))
        .map_err(|error| runtime_database_error(path, "busy_timeout", &error))?;
    if has_unsupported_runtime(&connection)
        .map_err(|error| runtime_database_error(path, "schema", &error))? {
        return Err(UNSUPPORTED.into());
    }
    Ok(())
}

pub(crate) fn runtime_database_error(path: &Path, stage: &str, error: &rusqlite::Error) -> String {
    let codes = match error {
        rusqlite::Error::SqliteFailure(code, _) => format!(
            " (SQLite code={}, extended_code={})", code.extended_code & 0xff, code.extended_code,
        ),
        _ => String::new(),
    };
    format!("数据库启动检查失败 [{stage}] {}: {error}{codes}", path.display())
}

/// Callback runs on the caller's worker thread. Returning Err cancels safely.
/// Progress is phase-local measured work, never a made-up overall percentage.
pub fn build_candidate(
    options: &MigrationOptions,
    observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>,
) -> Result<CandidateReport, String> {
    build_candidate_with_source_aliases(options, &[], observer)
}

/// Offline backups may still contain absolute paths under their former root.
/// Explicit aliases resolve only into the selected source copy; the original
/// roots are never opened, probed, or used to read missing external resources.
pub fn build_candidate_with_source_aliases(
    options: &MigrationOptions,
    source_aliases: &[PathBuf],
    observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>,
) -> Result<CandidateReport, String> {
    let started = Instant::now();
    let mut timing = BTreeMap::new();
    emit(observer, started, "preflight", 0, None, "steps")?;
    let source = checked_existing_root(&options.source_root)?;
    let target = checked_target(&source, &options.candidate_root)?;
    let parent = target.parent().ok_or("候选目录缺少父目录。")?;
    // OS lock is advisory to this migration tool only. Legacy exclusion belongs
    // to production cutover, not a falsely assumed new lock in a read-only tool.
    let lock_path = parent.join(format!(
        ".{}.migration.lock",
        target.file_name().unwrap().to_string_lossy()
    ));
    let _lock = MigrationLock::acquire(&lock_path)?;
    require_absent(&target)?;
    let stage = tempfile::Builder::new()
        .prefix(".cliphost-v2-candidate-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    let archive = stage.path().join("migration");
    fs::create_dir(&archive).map_err(|e| e.to_string())?;
    let source_db = source.join("userdata.db");
    check_local_path(&source_db, false)?;
    if fs::metadata(&source_db).map_err(|e| e.to_string())?.len() > MAX_CANDIDATE_SOURCE_BYTES {
        return Err(
            "来源数据库超过候选转换器当前 256 MiB 安全上限；需使用后续流式迁移版本，源未修改。"
                .into(),
        );
    }
    for suffix in ["-wal", "-shm"] {
        let p = source.join(format!("userdata.db{suffix}"));
        if p.exists() {
            check_local_path(&p, false)?;
        }
    }
    if source.join("userdata.db-wal").exists() && !source.join("userdata.db-shm").exists() {
        return Err("来源 WAL 缺少 SHM；请正常关闭旧宿主后重试，未创建源 sidecar。".into());
    }
    let journal = source.join("userdata.db-journal");
    match fs::symlink_metadata(&journal) {
        Ok(_) => {
            check_local_path(&journal, false)?;
            return Err(
                "来源存在 rollback journal；请用旧版本正常恢复并关闭数据库后重试，未修复源。"
                    .into(),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("无法核对来源 rollback journal，未打开来源数据库。".into()),
    }
    // On Windows these read-only handles deny write/delete sharing for the
    // entire preparation. An already-open legacy RW connection fails here.
    // This also makes immutable=1 legitimate for a fully checkpointed WAL
    // header with no sidecars: no writer can appear while the handle is held.
    let _source_guard = SourceGuard::acquire(&source_db)?;
    let no_wal = !source.join("userdata.db-wal").exists();
    let source_connection = if no_wal {
        open_immutable_guarded(&source_db)?
    } else {
        open_readonly(&source_db)?
    };
    reject_unsupported_runtime(&source_connection)?;
    let version: i64 = source_connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    source_connection
        .execute_batch("BEGIN DEFERRED; SELECT count(*) FROM sqlite_master;")
        .map_err(|e| e.to_string())?;
    let phase = Instant::now();
    // A temporary plaintext SQLite file could survive process termination.
    // Keep the entire original snapshot in memory, including freed pages, and
    // only persist its verified current-user encrypted envelope.
    let snapshot = backup_snapshot(&source_connection, started, observer)?;
    let (source_snapshot_sha256, source_archive_sha256) = {
        let bytes = snapshot
            .serialize(DatabaseName::Main)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_CANDIDATE_SOURCE_BYTES {
            return Err("WAL 快照超过候选转换器内存安全上限，未开始全量转换。".into());
        }
        let protected = protected_archive::protect(&bytes)?;
        if protected_archive::unprotect(&protected)?.as_slice() != &*bytes {
            return Err("来源凭据备份保护校验失败。".into());
        }
        write_synced(&archive.join("source.sqlite3.dpapi"), &protected)?;
        (hash_bytes(&bytes), hash_bytes(&protected))
    };
    let source_archive_path = "migration/source.sqlite3.dpapi";
    timing.insert("snapshot".into(), millis(phase));
    emit(
        observer,
        started,
        "snapshot_complete",
        1,
        Some(1),
        "snapshots",
    )?;
    let phase = Instant::now();
    emit(observer, started, "convert", 0, None, "items")?;
    let mut converted = convert::read_source(&snapshot, &source.join("Image"))?;
    drop(snapshot);
    timing.insert("convert".into(), millis(phase));
    // Copy every source configuration and local state without invoking a loader
    // that might "repair" or write configuration while reading it.
    let mut source_files = Vec::new();
    let mut configs = Vec::new();
    copy_tree(
        &source,
        Path::new("Configs"),
        &archive.join("source-files"),
        &mut source_files,
        &mut configs,
    )?;
    for name in ["states", "WindowState", "PanelWindow"] {
        copy_tree(
            &source,
            Path::new(name),
            &archive.join("source-files"),
            &mut source_files,
            &mut Vec::new(),
        )?;
    }
    copy_tree(
        &source,
        Path::new("LanShare"),
        &archive.join("source-files"),
        &mut source_files,
        &mut Vec::new(),
    )?;
    for name in [
        "clip-host-ui-preferences.json",
        "clip-host-favorites-view-state.json",
        "versioned-item-persistence.json",
    ] {
        copy_tree(
            &source,
            Path::new(name),
            &archive.join("source-files"),
            &mut source_files,
            &mut Vec::new(),
        )?;
    }
    let root_children = read_root_order(&configs)?;
    let root_id = format!("v2-root-{}", Uuid::new_v4().simple());
    let roots = convert::normalize_favorites(&mut converted, &root_children, &root_id)?;
    let phase = Instant::now();
    let mut importer = assets::AssetImporter::new(&source, stage.path())?;
    importer.set_source_aliases(source_aliases)?;
    if !source_aliases.is_empty() {
        write_synced(
            &archive.join("source-root-aliases.json"),
            &serde_json::to_vec(source_aliases).map_err(|e| e.to_string())?,
        )?;
    }
    let mut references = Vec::new();
    for (index, item) in converted.items.iter().enumerate() {
        emit(
            observer,
            started,
            "assets",
            index as u64,
            Some(converted.items.len() as u64),
            "items",
        )?;
        for reference in &item.asset_candidates {
            references.push(importer.import_reference(
                &item.scope,
                &item.id,
                &reference.slot,
                reference.ordinal,
                &reference.original_locator,
            )?);
        }
        references.extend(importer.collect_payload_references(
            &item.scope,
            &item.id,
            &item.kind,
            &item.payload_json,
            &item.source_process,
        )?);
    }
    // Preserve files not fully understood by this parser as well. These archives
    // are not claimed as portable resolved references. Unknown/non-image files
    // use protected archives rather than ordinary plaintext asset paths.
    let mut legacy_archives = Vec::new();
    for relative in ["Image", "blobs", "WindowBackground", "Resource"] {
        legacy_archives.push(importer.archive_legacy_tree(Path::new(relative))?);
    }
    write_synced(
        &archive.join("legacy-assets-manifest.json"),
        &serde_json::to_vec(&legacy_archives).map_err(|e| e.to_string())?,
    )?;
    timing.insert("assets".into(), millis(phase));
    emit(
        observer,
        started,
        "assets_complete",
        converted.items.len() as u64,
        Some(converted.items.len() as u64),
        "items",
    )?;
    let candidate_db = stage.path().join("userdata.db");
    let phase = Instant::now();
    let mut destination = Connection::open(&candidate_db).map_err(|e| e.to_string())?;
    destination
        .execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;",
        )
        .map_err(|e| e.to_string())?;
    let tx = destination
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    tx.execute_batch(CORE_SCHEMA).map_err(|e| e.to_string())?;
    tx.execute_batch("CREATE TABLE storage_v2_candidate (singleton INTEGER PRIMARY KEY CHECK(singleton=1), operation_id TEXT NOT NULL, production_ready INTEGER NOT NULL CHECK(production_ready=0));").map_err(|e| e.to_string())?;
    let now = timestamp();
    let dataset_id = Uuid::new_v4().simple().to_string();
    let operation_id = Uuid::new_v4().simple().to_string();
    for (index, item) in converted.items.iter().enumerate() {
        emit(
            observer,
            started,
            "write",
            index as u64,
            Some(converted.items.len() as u64),
            "items",
        )?;
        insert_item(&tx, item)?;
    }
    let root_revision = token();
    let root_json = serde_json::to_string(&roots).map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO items(scope,id,kind,title,payload_json,created_at,updated_at,revision,children_json,system_role) VALUES('favorite',?1,'folder',NULL,'{}',?2,?2,?3,?4,'favorite_root')", params![root_id,now,root_revision,root_json]).map_err(|e|e.to_string())?;
    let mut unique_assets = BTreeMap::new();
    for reference in &references {
        if let Some(asset) = &reference.asset {
            unique_assets.insert(asset.id.clone(), asset.clone());
        }
    }
    if importer.assets().count() != unique_assets.len() {
        return Err("资产登记与引用集合不一致，未发布。".into());
    }
    for asset in unique_assets.values() {
        tx.execute(
            "INSERT INTO assets(id,byte_length,media_type,created_at) VALUES(?1,?2,?3,?4)",
            params![asset.id, asset.byte_length, asset.media_type, now],
        )
        .map_err(|e| e.to_string())?;
    }
    for reference in &references {
        tx.execute("INSERT INTO item_assets(scope,item_id,slot,ordinal,asset_id,original_locator,status) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![reference.scope,reference.item_id,reference.slot,reference.ordinal,reference.asset.as_ref().map(|a|a.id.as_str()),reference.original_locator,reference.status.as_str()]).map_err(|e|e.to_string())?;
    }
    let mut expected_settings = BTreeMap::new();
    for (relative, value) in &configs {
        if protected_archive::is_sensitive_path(relative)
            || protected_archive::json_contains_credentials(value)
        {
            continue;
        }
        let mut value = value.clone();
        if relative.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .eq_ignore_ascii_case("FavorConfig.json")
        }) {
            if let Some(object) = value.as_object_mut() {
                object.retain(|key, _| !key.eq_ignore_ascii_case("RootChildren"));
            }
        }
        // Opaque compatibility documents are lossless candidate material. The
        // canonical settings ownership migration is an explicit release blocker.
        let key = format!("source/{}", relative.to_string_lossy().replace('\\', "/"));
        let encoded = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let revision = token();
        tx.execute(
            "INSERT INTO settings(key,value_json,revision) VALUES(?1,?2,?3)",
            params![key, encoded, revision],
        )
        .map_err(|e| e.to_string())?;
        expected_settings.insert(key, (encoded, revision));
    }
    tx.execute("INSERT INTO store_info(singleton,dataset_id,format_version,favorite_root_id) VALUES(1,?1,2,?2)",params![dataset_id,root_id]).map_err(|e|e.to_string())?;
    tx.execute(
        "INSERT INTO schema_migrations VALUES(1,?1,?2)",
        params![hash_bytes(CORE_SCHEMA.as_bytes()), now],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO storage_v2_candidate VALUES(1,?1,0)",
        [&operation_id],
    )
    .map_err(|e| e.to_string())?;
    emit(
        observer,
        started,
        "before_commit",
        converted.items.len() as u64,
        Some(converted.items.len() as u64),
        "items",
    )?;
    tx.commit().map_err(|e| e.to_string())?;
    // Bind the report to the transaction we just produced, before yielding to
    // observers. A later legal SQL update must not establish a new baseline.
    let committed_database_sha256 = hash_file(&candidate_db)?;
    emit(
        observer,
        started,
        "after_commit",
        1,
        Some(1),
        "transactions",
    )?;
    timing.insert("write".into(), millis(phase));
    verify_sqlite(&destination)?;
    verify_items(&destination, &converted.items)?;
    verify_metadata(
        &destination,
        &root_id,
        &root_json,
        &root_revision,
        &dataset_id,
        &operation_id,
        &now,
        &expected_settings,
    )?;
    if hash_file(&candidate_db)? != committed_database_sha256 {
        return Err("候选事务提交后数据库发生变化，未发布。".into());
    }
    let issues: Vec<_> = references.iter().filter_map(|r|r.reason.as_ref().map(|reason|json!({"scope":r.scope,"id":r.item_id,"slot":r.slot,"ordinal":r.ordinal,"status":r.status.as_str(),"originalLocator":r.original_locator,"reason":reason}))).collect();
    write_synced(
        &archive.join("reference-issues.json"),
        &serde_json::to_vec_pretty(&issues).map_err(|e| e.to_string())?,
    )?;
    let rows = serde_json::to_vec(&converted.archive).map_err(|e| e.to_string())?;
    let protected_rows = protected_archive::protect(&rows)?;
    if protected_archive::unprotect(&protected_rows)? != rows {
        return Err("原始行归档保护校验失败。".into());
    }
    write_synced(&archive.join("source-rows.json.dpapi"), &protected_rows)?;
    write_synced(
        &archive.join("conversion-warnings.json"),
        &serde_json::to_vec(&converted.warnings).map_err(|e| e.to_string())?,
    )?;
    drop(destination);
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&candidate_db)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    let phase = Instant::now();
    emit(
        observer,
        started,
        "verify",
        0,
        Some(unique_assets.len() as u64),
        "assets",
    )?;
    for (index, asset) in unique_assets.values().enumerate() {
        let path = stage.path().join(&asset.relative_path);
        if hash_file(&path)? != asset.id {
            return Err("候选资产校验失败，未发布。".into());
        }
        emit(
            observer,
            started,
            "verify",
            (index + 1) as u64,
            Some(unique_assets.len() as u64),
            "assets",
        )?;
    }
    // Revalidate all source config/state snapshots, including secret files that
    // were copied as bytes, without putting their content in logs or reports.
    for entry in &source_files {
        check_local_path(&entry.source, false)?;
        if hash_file(&entry.source)? != entry.sha256 {
            return Err("迁移期间来源配置/状态已变化，未发布。".into());
        }
    }
    let expected: BTreeSet<_> = source_files
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    if source_file_inventory(&source)? != expected {
        return Err("迁移期间来源配置/状态文件集合已变化，未发布。".into());
    }
    importer.revalidate_sources()?;
    source_connection
        .execute_batch("ROLLBACK;")
        .map_err(|e| e.to_string())?;
    let after: i64 = source_connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version != after {
        return Err("迁移期间来源数据库被其他程序修改，未发布；请关闭旧写入者后重试。".into());
    }
    timing.insert("verify".into(), millis(phase));
    let missing = references
        .iter()
        .filter(|r| r.status.as_str() == "missing")
        .count();
    let unresolved = references
        .iter()
        .filter(|r| r.status.as_str() == "unresolved")
        .count();
    let mut report = CandidateReport {
        operation_id,
        dataset_id,
        source_format: format!("{:?}", converted.format),
        history_count: converted
            .items
            .iter()
            .filter(|i| i.scope == "history")
            .count(),
        favorite_count: converted
            .items
            .iter()
            .filter(|i| i.scope == "favorite")
            .count(),
        asset_count: unique_assets.len(),
        missing_references: missing,
        unresolved_references: unresolved,
        orphan_usage_count: converted.orphan_usage.len(),
        source_snapshot_sha256,
        source_archive_path: source_archive_path.into(),
        source_archive_sha256,
        source_archive_protected: true,
        candidate_database_sha256: committed_database_sha256,
        schema_sha256: hash_bytes(CORE_SCHEMA.as_bytes()),
        archive_files: source_files.len(),
        elapsed_ms: millis(started),
        candidate_only: true,
        production_ready: false,
        blockers: vec![
            "v2_runtime_and_atomic_cutover_not_implemented".into(),
            "canonical_settings_and_auxiliary_domains_require_semantic_migration".into(),
        ],
        phase_timings_ms: timing,
    };
    if missing > 0 {
        report.blockers.push("missing_assets".into());
    }
    if unresolved > 0 {
        report.blockers.push("unresolved_references".into());
    }
    if importer.has_incomplete_references() && missing == 0 && unresolved == 0 {
        return Err("未完整解析的附件缺少持久问题记录，未发布。".into());
    }
    if !converted.warnings.is_empty() {
        report
            .blockers
            .push("source_conversion_warnings_require_review".into());
    }
    write_synced(
        &archive.join("source-files.json"),
        &serde_json::to_vec_pretty(&source_files).map_err(|e| e.to_string())?,
    )?;
    let sealed_files = seal_tree(stage.path())?;
    emit(
        observer,
        started,
        "before_publish",
        1,
        Some(1),
        "candidates",
    )?;
    // An observer/UI scheduling gap can be arbitrarily long. Do not trust the
    // source checks from before this last cancellable boundary.
    let current: i64 = source_connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if current != version {
        return Err("发布前来源数据库已变化，未发布。".into());
    }
    for entry in &source_files {
        check_local_path(&entry.source, false)?;
        if hash_file(&entry.source)? != entry.sha256 {
            return Err("发布前来源配置/状态已变化，未发布。".into());
        }
    }
    if source_file_inventory(&source)? != expected {
        return Err("发布前来源配置/状态文件集合已变化，未发布。".into());
    }
    importer.revalidate_sources()?;
    for asset in unique_assets.values() {
        if hash_file(&stage.path().join(&asset.relative_path))? != asset.id {
            return Err("发布前候选资产摘要核对失败，未发布。".into());
        }
    }
    for summary in &legacy_archives {
        for entry in &summary.files {
            if hash_file(&stage.path().join(&entry.archive_relative_path))?
                != entry.actual_archive_sha256
            {
                return Err("发布前旧资源归档摘要核对失败，未发布。".into());
            }
        }
    }
    for entry in &source_files {
        if hash_file(&archive.join("source-files").join(&entry.archive_relative))?
            != entry.archive_sha256
        {
            return Err("发布前配置归档摘要核对失败，未发布。".into());
        }
    }
    if hash_file(&candidate_db)? != report.candidate_database_sha256 {
        return Err("候选数据库在验证后被修改，未发布。".into());
    }
    if hash_file(&stage.path().join(&report.source_archive_path))? != report.source_archive_sha256 {
        return Err("来源保护归档在验证后被修改，未发布。".into());
    }
    if seal_tree(stage.path())? != sealed_files {
        return Err("候选资产或归档在最终验证后被修改，未发布。".into());
    }
    report.elapsed_ms = millis(started);
    report.archive_files += legacy_archives
        .iter()
        .map(|summary| summary.files.len())
        .sum::<usize>()
        + 1;
    write_synced(
        &archive.join("candidate-report.json"),
        &serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )?;
    require_absent(&target)?;
    publish_new_directory(stage.path(), &target)?;
    // No fallible callback after the commit point: its failure must never make a
    // successfully published candidate look absent or safe to overwrite.
    Ok(report)
}

fn insert_item(c: &Connection, i: &convert::ConvertedItem) -> Result<(), String> {
    c.execute("INSERT INTO items(scope,id,kind,title,icon_override,description,payload_json,search_text,dedupe_key,source_process,created_at,updated_at,sub_index,is_pinned,usage_count,last_used_at,revision,owner_parent_id,children_json,source_history_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",params![i.scope,i.id,i.kind,i.title,i.icon_override,i.description,serde_json::to_string(&i.payload_json).map_err(|e|e.to_string())?,i.search_text,i.dedupe_key,i.source_process,i.created_at,i.updated_at,i.sub_index,i.is_pinned,i.usage_count,i.last_used_at,token(),i.owner_parent_id,i.children.as_ref().map(serde_json::to_string).transpose().map_err(|e|e.to_string())?,i.source_history_id]).map_err(|e|e.to_string())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_metadata(
    c: &Connection,
    root_id: &str,
    root_json: &str,
    root_revision: &str,
    dataset_id: &str,
    operation_id: &str,
    now: &str,
    settings: &BTreeMap<String, (String, String)>,
) -> Result<(), String> {
    let root: (String, String, String, String, String, String, String, String, Option<String>, Option<String>) = c.query_row(
        "SELECT scope,id,kind,payload_json,children_json,created_at,updated_at,revision,title,owner_parent_id FROM items WHERE system_role='favorite_root'",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?))
    ).map_err(|e| e.to_string())?;
    if root
        != (
            "favorite".into(),
            root_id.into(),
            "folder".into(),
            "{}".into(),
            root_json.into(),
            now.into(),
            now.into(),
            root_revision.into(),
            None,
            None,
        )
    {
        return Err("候选系统根及顺序核对失败，未发布。".into());
    }
    let store: (i64, String, i64, String) = c
        .query_row(
            "SELECT singleton,dataset_id,format_version,favorite_root_id FROM store_info",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| e.to_string())?;
    let migration: (i64, String, String) = c
        .query_row(
            "SELECT version,checksum,applied_at FROM schema_migrations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| e.to_string())?;
    let candidate: (i64, String, i64) = c
        .query_row(
            "SELECT singleton,operation_id,production_ready FROM storage_v2_candidate",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| e.to_string())?;
    if store != (1, dataset_id.into(), 2, root_id.into())
        || migration != (1, hash_bytes(CORE_SCHEMA.as_bytes()), now.into())
        || candidate != (1, operation_id.into(), 0)
    {
        return Err("候选数据集身份或版本记录核对失败，未发布。".into());
    }
    let mut actual = BTreeMap::new();
    let mut statement = c
        .prepare("SELECT key,value_json,revision,value_version FROM settings")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if row.get::<_, i64>(3).map_err(|e| e.to_string())? != 1 {
            return Err("候选设置版本核对失败，未发布。".into());
        }
        actual.insert(
            row.get::<_, String>(0).map_err(|e| e.to_string())?,
            (
                row.get::<_, String>(1).map_err(|e| e.to_string())?,
                row.get::<_, String>(2).map_err(|e| e.to_string())?,
            ),
        );
    }
    if &actual != settings {
        return Err("候选设置内容核对失败，未发布。".into());
    }
    Ok(())
}

fn verify_items(c: &Connection, items: &[convert::ConvertedItem]) -> Result<(), String> {
    let count: i64 = c
        .query_row(
            "SELECT count(*) FROM items WHERE system_role IS NULL",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if count != items.len() as i64 {
        return Err("候选条目数量不一致。".into());
    }
    for i in items {
        let row=c.query_row("SELECT kind,title,icon_override,description,payload_json,search_text,dedupe_key,source_process,created_at,updated_at,sub_index,is_pinned,usage_count,last_used_at,owner_parent_id,children_json,source_history_id FROM items WHERE scope=?1 AND id=?2",params![i.scope,i.id],|r| {
            Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,String>(8)?,r.get::<_,String>(9)?,r.get::<_,i32>(10)?,r.get::<_,bool>(11)?,r.get::<_,i64>(12)?,r.get::<_,Option<String>>(13)?,r.get::<_,Option<String>>(14)?,r.get::<_,Option<String>>(15)?,r.get::<_,Option<String>>(16)?))
        }).map_err(|e|e.to_string())?;
        let payload: Value = serde_json::from_str(&row.4).map_err(|e| e.to_string())?;
        let children: Option<Vec<String>> = row
            .15
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| e.to_string())?;
        if row.0 != i.kind
            || row.1 != i.title
            || row.2 != i.icon_override
            || row.3 != i.description
            || payload != i.payload_json
            || row.5 != i.search_text
            || row.6 != i.dedupe_key
            || row.7 != i.source_process
            || row.8 != i.created_at
            || row.9 != i.updated_at
            || row.10 != i.sub_index
            || row.11 != i.is_pinned
            || row.12 != i.usage_count
            || row.13 != i.last_used_at
            || row.14 != i.owner_parent_id
            || children != i.children
            || row.16 != i.source_history_id
        {
            return Err("候选条目语义校验失败。".into());
        }
    }
    Ok(())
}

pub(crate) fn backup_snapshot(
    source: &Connection,
    started: Instant,
    observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>,
) -> Result<Connection, String> {
    backup_snapshot_bounded(source, started, observer, MAX_CANDIDATE_SOURCE_BYTES)
}
fn backup_snapshot_bounded(
    source: &Connection,
    started: Instant,
    observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>,
    max_bytes: u64,
) -> Result<Connection, String> {
    let page_size: u64 = source
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let mut destination = Connection::open_in_memory().map_err(|e| e.to_string())?;
    destination
        .execute_batch("PRAGMA temp_store=MEMORY; PRAGMA journal_mode=MEMORY;")
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    {
        let backup = Backup::new(source, &mut destination).map_err(|e| e.to_string())?;
        loop {
            let result = backup.step(128).map_err(|e| e.to_string())?;
            let p = backup.progress();
            if (p.pagecount.max(0) as u64)
                .checked_mul(page_size)
                .is_none_or(|bytes| bytes > max_bytes)
            {
                return Err("SQLite 有效快照（含 WAL）超过候选转换器安全上限。".into());
            }
            emit(
                observer,
                started,
                "snapshot",
                (p.pagecount - p.remaining).max(0) as u64,
                Some(p.pagecount.max(0) as u64),
                "pages",
            )?;
            match result {
                StepResult::Done => break,
                StepResult::More => {}
                _ => {
                    if Instant::now() > deadline {
                        return Err("来源数据库持续繁忙，未发布候选。".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    verify_sqlite(&destination)?;
    destination
        .execute_batch("PRAGMA query_only=ON;")
        .map_err(|e| e.to_string())?;
    Ok(destination)
}

pub(crate) fn verify_sqlite(c: &Connection) -> Result<(), String> {
    let integrity: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if integrity != "ok" {
        return Err("SQLite 完整性检查失败，未发布候选。".into());
    }
    let mut check = c
        .prepare("PRAGMA foreign_key_check")
        .map_err(|e| e.to_string())?;
    if check
        .query([])
        .map_err(|e| e.to_string())?
        .next()
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("SQLite 外键检查失败。".into());
    }
    Ok(())
}

pub(crate) fn open_readonly(path: &Path) -> Result<Connection, String> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    c.busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    c.execute_batch("PRAGMA query_only=ON;")
        .map_err(|e| e.to_string())?;
    Ok(c)
}

pub(crate) fn open_immutable_guarded(path: &Path) -> Result<Connection, String> {
    // Called only while SourceGuard excludes foreign write/delete handles, and
    // only after checking that there is no WAL to replay. Never use this for a
    // live database or to bypass a missing/unreadable WAL.
    #[cfg(not(windows))]
    {
        let _ = path;
        return Err("此平台尚未实现可证明来源静止的迁移文件锁。".into());
    }
    #[cfg(windows)]
    {
        let mut uri =
            url::Url::from_file_path(path).map_err(|_| "来源无法转换为 SQLite 文件 URI。")?;
        uri.set_query(Some("mode=ro&immutable=1"));
        let c = Connection::open_with_flags(
            uri.as_str(),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;
        c.execute_batch("PRAGMA query_only=ON;")
            .map_err(|e| e.to_string())?;
        Ok(c)
    }
}

pub(crate) struct SourceGuard {
    _handles: Vec<File>,
}
impl SourceGuard {
    pub(crate) fn acquire(database: &Path) -> Result<Self, String> {
        #[cfg(not(windows))]
        {
            let _ = database;
            return Err("此平台尚未实现来源写入排他保护。".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let mut handles = Vec::new();
            for suffix in ["", "-wal", "-shm"] {
                let mut name = database.as_os_str().to_os_string();
                name.push(suffix);
                let path = PathBuf::from(name);
                match fs::symlink_metadata(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {
                        continue
                    }
                    Err(e) => return Err(e.to_string()),
                    Ok(_) => {}
                }
                check_local_path(&path, false)?;
                handles.push(
                    OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(&path)
                        .map_err(|_| {
                            "来源数据库或 sidecar 仍被写入者占用；请正常关闭全部旧宿主后重试。"
                                .to_string()
                        })?,
                );
            }
            Ok(Self { _handles: handles })
        }
    }
}

#[derive(Debug, Serialize)]
struct SourceFile {
    source: PathBuf,
    relative: PathBuf,
    sha256: String,
    archive_relative: PathBuf,
    archive_sha256: String,
    protected: bool,
}

pub(crate) fn seal_tree(root: &Path) -> Result<BTreeMap<PathBuf, String>, String> {
    let mut result = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        check_local_path(&directory, true)?;
        for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            check_local_path(&path, true)?;
            if path.is_dir() {
                pending.push(path);
            } else {
                result.insert(
                    path.strip_prefix(root)
                        .map_err(|e| e.to_string())?
                        .to_path_buf(),
                    hash_file(&path)?,
                );
            }
        }
    }
    Ok(result)
}

fn source_file_inventory(root: &Path) -> Result<BTreeSet<PathBuf>, String> {
    fn walk(root: &Path, relative: &Path, out: &mut BTreeSet<PathBuf>) -> Result<(), String> {
        let path = root.join(relative);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
            Ok(_) => {}
        }
        check_local_path(&path, true)?;
        if path.is_dir() {
            for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
                walk(
                    root,
                    &relative.join(entry.map_err(|e| e.to_string())?.file_name()),
                    out,
                )?;
            }
        } else {
            out.insert(relative.to_path_buf());
        }
        Ok(())
    }
    let mut out = BTreeSet::new();
    for name in [
        "Configs",
        "states",
        "WindowState",
        "PanelWindow",
        "LanShare",
        "clip-host-ui-preferences.json",
        "clip-host-favorites-view-state.json",
        "versioned-item-persistence.json",
    ] {
        walk(root, Path::new(name), &mut out)?;
    }
    Ok(out)
}

fn copy_tree(
    root: &Path,
    relative: &Path,
    destination: &Path,
    manifest: &mut Vec<SourceFile>,
    configs: &mut Vec<(PathBuf, Value)>,
) -> Result<(), String> {
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    }
    check_local_path(&path, true)?;
    if path.is_dir() {
        let mut entries = fs::read_dir(&path)
            .map_err(|e| e.to_string())?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort();
        for name in entries {
            copy_tree(root, &relative.join(name), destination, manifest, configs)?;
        }
    } else {
        let direct_json = relative.starts_with("Configs")
            && relative
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            && relative.components().count() == 2;
        // Unknown plugin configuration can contain credentials under arbitrary
        // field names. Preserve nested Configs bytes protected by default.
        let nested_config = relative.starts_with("Configs") && relative.components().count() > 2;
        let json_file = relative
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        let known_sensitive = protected_archive::is_sensitive_path(relative) || nested_config;
        if !json_file && !known_sensitive {
            let target = destination.join(relative);
            fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
            let hash = copy_synced_stream(&path, &target)?;
            manifest.push(SourceFile {
                source: path,
                relative: relative.to_path_buf(),
                sha256: hash,
                archive_relative: relative.to_path_buf(),
                archive_sha256: hash_file(&target)?,
                protected: false,
            });
            return Ok(());
        }
        let length = fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if length > 512 * 1024 * 1024 || (json_file && length > 16 * 1024 * 1024) {
            return Err("来源配置或受保护归档超过当前有界处理上限，未发布。".into());
        }
        let limit = if json_file {
            16 * 1024 * 1024
        } else {
            protected_archive::MAX_ARCHIVE_BYTES
        };
        let bytes = read_bounded(&path, limit)?;
        let hash = hash_bytes(&bytes);
        let mut sensitive = known_sensitive;
        let mut config_value = None;
        if json_file {
            let raw = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
            match serde_json::from_slice::<Value>(raw) {
                Ok(value) => {
                    sensitive |= protected_archive::json_contains_credentials(&value);
                    if direct_json {
                        config_value = Some(value);
                    }
                }
                Err(_) if direct_json => {
                    return Err(format!(
                        "配置 {} JSON 损坏；未使用默认值。",
                        relative.display()
                    ))
                }
                // Unknown state may use a non-JSON extension convention. Keep
                // its exact bytes, but never assume malformed JSON is harmless.
                Err(_) => sensitive = true,
            }
        }
        let mut archived = relative.as_os_str().to_os_string();
        if sensitive {
            archived.push(".dpapi");
        }
        let archived = PathBuf::from(archived);
        let target = destination.join(&archived);
        fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
        if sensitive {
            let encrypted = protected_archive::protect(&bytes)?;
            if protected_archive::unprotect(&encrypted)? != bytes {
                return Err("配置凭据保护校验失败。".into());
            }
            write_synced(&target, &encrypted)?;
        } else {
            write_synced(&target, &bytes)?;
        }
        // Parsing for graph extraction and eligibility for plaintext settings
        // are separate decisions. A future credential extension must not erase
        // the already understood RootChildren ordering.
        if let Some(value) = config_value {
            configs.push((relative.to_path_buf(), value));
        }
        manifest.push(SourceFile {
            source: path,
            relative: relative.to_path_buf(),
            sha256: hash,
            archive_relative: archived,
            archive_sha256: hash_file(&target)?,
            protected: sensitive,
        });
    }
    Ok(())
}

fn copy_synced_stream(source: &Path, target: &Path) -> Result<String, String> {
    let mut input = File::open(source).map_err(|e| e.to_string())?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
    }
    output.sync_all().map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hash.finalize()))
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let count = file.read(&mut chunk).map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes
            .len()
            .checked_add(count)
            .is_none_or(|size| size > limit)
        {
            return Err("来源文件在读取期间超过有界处理上限，未发布。".into());
        }
        bytes
            .try_reserve(count)
            .map_err(|_| "无法分配来源文件有界缓冲区。")?;
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn read_root_order(configs: &[(PathBuf, Value)]) -> Result<Vec<String>, String> {
    let Some((_, value)) = configs.iter().find(|(p, _)| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("FavorConfig.json"))
    }) else {
        return Ok(Vec::new());
    };
    let object = value.as_object().ok_or("FavorConfig 必须为对象。")?;
    let matching: Vec<_> = object
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("RootChildren"))
        .collect();
    if matching.len() > 1 {
        return Err("收藏根顺序存在重复大小写字段。".into());
    }
    matching
        .first()
        .map(|(_, v)| {
            serde_json::from_value((*v).clone()).map_err(|_| "收藏根顺序必须是字符串数组。".into())
        })
        .unwrap_or(Ok(Vec::new()))
}

pub(crate) fn checked_existing_root(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("迁移来源必须是无父级跳转的绝对路径。".into());
    }
    check_local_path(path, true)?;
    if !path.is_dir() {
        return Err("来源必须是目录。".into());
    }
    fs::canonicalize(path).map_err(|e| e.to_string())
}

pub(crate) fn checked_target(source: &Path, path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("候选目录必须是无父级跳转的绝对路径。".into());
    }
    require_absent(path)?;
    let parent = path.parent().ok_or("候选目录没有父目录。")?;
    check_local_path(parent, true)?;
    let parent = fs::canonicalize(parent).map_err(|e| e.to_string())?;
    let target = parent.join(path.file_name().ok_or("候选目录无名称。")?);
    if target.starts_with(source) || source.starts_with(&target) {
        return Err("来源与候选目录不得相同、嵌套或互为祖先。".into());
    }
    Ok(target)
}

pub(crate) fn check_local_path(path: &Path, allow_directory: bool) -> Result<(), String> {
    let mut partial = PathBuf::new();
    for component in path.components() {
        partial.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("onedrive") || name == "dropbox" || name.starts_with("icloud") {
            return Err("迁移路径位于云同步目录，已停止读取。".into());
        }
        let meta = fs::symlink_metadata(&partial).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            return Err("迁移路径含链接，已停止读取。".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
                return Err("迁移路径含联接点或云占位属性，已停止读取。".into());
            }
        }
    }
    if !allow_directory && !path.is_file() {
        return Err("迁移来源不是普通文件。".into());
    }
    Ok(())
}

pub(crate) fn require_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err("候选路径已存在；禁止覆盖、合并或自动删除。".into()),
        Err(e) => Err(e.to_string()),
    }
}

pub(crate) fn publish_new_directory(source: &Path, target: &Path) -> Result<(), String> {
    // Windows directory rename fails if the target exists, including an empty
    // directory. Do not emulate publication by deleting/replacing the target.
    #[cfg(windows)]
    {
        fs::rename(source, target).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = (source, target);
        Err("候选目录的无覆盖原子发布尚未在此平台实现。".into())
    }
}

pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())
}
pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn hash_file(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}
fn token() -> String {
    Uuid::new_v4().simple().to_string()
}
fn millis(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
fn emit(
    observer: &mut impl FnMut(&MigrationProgress) -> Result<(), String>,
    started: Instant,
    phase: &str,
    completed: u64,
    total: Option<u64>,
    unit: &str,
) -> Result<(), String> {
    observer(&MigrationProgress {
        phase: phase.into(),
        completed,
        total,
        unit: unit.into(),
        elapsed_ms: millis(started),
    })
}

pub(crate) struct MigrationLock(File);
impl MigrationLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self, String> {
        check_local_path(path.parent().unwrap(), true)?;
        if path.exists() {
            check_local_path(path, false)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| e.to_string())?;
        file.try_lock()
            .map_err(|_| "另一个迁移任务持有同一候选目录锁。".to_string())?;
        Ok(Self(file))
    }
}
impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
