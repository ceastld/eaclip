//! Portable favorite snapshots. Network IO belongs to the WebDAV worker.
//!
//! Canonical source documents and local materializations are kept separately:
//! downloading an attachment must never manufacture another remote edit.
use crate::{
    model::{ClipRecord, FavorRecord},
    paths::AppPaths,
    store::AppStore,
};
use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const SHADOW_KEY: &str = "webdav-favorites:portable-shadow:v1";
const APPLY_RECEIPT: &str = "webdav-favorites:last-apply:v1";
const JOURNAL: &str = "webdav-favorites-apply-journal.json";

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_items: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_items: 10_000,
            max_file_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncIssue {
    pub item_id: String,
    pub resource: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportBundle {
    /// Only favor/<id> entries. The caller encodes roots in its own dataset key.
    pub entries: BTreeMap<String, Value>,
    pub roots: Vec<String>,
    pub blobs: BTreeMap<String, Vec<u8>>,
    pub revision: String,
    pub issues: Vec<SyncIssue>,
}
#[derive(Debug, Clone)]
pub struct ApplyPlan {
    pub expected_revision: String,
    pub entries: BTreeMap<String, Value>,
    pub roots: Vec<String>,
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// Caller-owned KV key/value, committed atomically with rows and shadow.
    /// Query AppStore::kv_get(key) after a crash, then recover_apply before use.
    pub receipt: Option<(String, String)>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    sha256: String,
    byte_len: u64,
    extension: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableFavorite {
    format: u32,
    record: Value,
    /// Original complete C# document, including fields not modeled by Rust.
    document: Option<Value>,
    resources: BTreeMap<String, Resource>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Shadow {
    local: Value,
    portable: PortableFavorite,
    #[serde(default)]
    resource_locations: BTreeMap<String, String>,
}
/// Local-only optimistic token. Resource paths never enter the remote snapshot.
#[derive(Serialize, Deserialize)]
struct LocalRevision {
    rows: String,
    resources: BTreeMap<String, Resource>,
}
#[derive(Serialize, Deserialize)]
struct ApplyJournal {
    operation_id: String,
    before: Option<Value>,
    after: Value,
}

fn err(_error: impl std::fmt::Display) -> String {
    // These errors reach ordinary status.json. serde's Display may include the
    // offending clipboard string, so keep diagnostic categories content-free.
    "Favorite synchronization storage or data validation failed.".into()
}
fn digest(bytes: &[u8]) -> String {
    crate::model::sha256_hex(bytes).to_ascii_lowercase()
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn config_path(paths: &AppPaths) -> PathBuf {
    paths.configs.join("FavorConfig.json")
}
fn read_config(paths: &AppPaths) -> Result<Option<Value>, String> {
    let path = config_path(paths);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            safe_metadata(&meta)?;
            let bytes = fs::read(&path).map_err(err)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(err)?
                .trim_start_matches('\u{feff}');
            let value: Value = serde_json::from_str(text).map_err(err)?;
            if !value.is_object() {
                return Err("Favorite config must be an object.".into());
            }
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(e)),
    }
}
fn roots_from(config: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = config.and_then(Value::as_object).and_then(|m| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("RootChildren"))
            .map(|(_, v)| v)
    }) else {
        return Ok(Vec::new());
    };
    serde_json::from_value(value.clone()).map_err(err)
}
fn set_roots(mut config: Value, roots: &[String]) -> Result<Value, String> {
    let object = config
        .as_object_mut()
        .ok_or("Favorite config must be an object.")?;
    let key = object
        .keys()
        .find(|k| k.eq_ignore_ascii_case("RootChildren"))
        .cloned()
        .unwrap_or_else(|| "RootChildren".into());
    object.insert(key, json!(roots));
    Ok(config)
}
fn local_snapshot(
    store: &AppStore,
    tx: &Transaction<'_>,
    roots: &[String],
) -> Result<(BTreeMap<String, Value>, String), String> {
    let records = store.list_favors().map_err(err)?;
    let documents: BTreeMap<String, Value> = if store.is_document_store() {
        let mut stmt = tx
            .prepare("SELECT Id,Data FROM main.\"favor_item\" ORDER BY Id")
            .map_err(err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(err)?;
        let mut docs = BTreeMap::new();
        for row in rows {
            let (id, raw) = row.map_err(err)?;
            docs.insert(id, serde_json::from_str(&raw).map_err(err)?);
        }
        docs
    } else {
        BTreeMap::new()
    };
    let mut local = BTreeMap::new();
    let mut revisions = BTreeMap::new();
    for record in records {
        revisions.insert(record.id.clone(), record.storage_revision.clone());
        local.insert(record.id.clone(), json!({"record":serde_json::to_value(&record).map_err(err)?,"document":documents.get(&record.id)}));
    }
    let revision = digest(
        &serde_json::to_vec(&json!({"local":local,"tokens":revisions,"roots":roots}))
            .map_err(err)?,
    );
    Ok((local, revision))
}
/// Reuse untouched canonical fields, including original rich-text byte strings.
fn preserve_unchanged(current: &Value, old_local: &Value, canonical: &Value) -> Value {
    if current == old_local {
        return canonical.clone();
    }
    match (current, old_local, canonical) {
        (Value::Object(now), Value::Object(old), Value::Object(source)) => {
            let mut merged: serde_json::Map<String, Value> = now
                .iter()
                .map(|(k, v)| {
                    let merged = match (old.get(k), source.get(k)) {
                        (Some(o), Some(s)) => preserve_unchanged(v, o, s),
                        _ => v.clone(),
                    };
                    (k.clone(), merged)
                })
                .collect();
            // An older local projection never observed these fields, so its
            // absence is not a deletion of a newer canonical field.
            for (key, value) in source {
                if !old.contains_key(key) && !now.contains_key(key) {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Value::Object(merged)
        }
        (Value::Array(now), Value::Array(old), Value::Array(source))
            if now.len() == old.len() && old.len() == source.len() =>
        {
            Value::Array(
                now.iter()
                    .zip(old)
                    .zip(source)
                    .map(|((n, o), s)| preserve_unchanged(n, o, s))
                    .collect(),
            )
        }
        _ => current.clone(),
    }
}

pub fn export_favorites(
    store: &AppStore,
    paths: &AppPaths,
    limits: &Limits,
) -> Result<ExportBundle, String> {
    recover_apply(store, paths)?;
    if store.is_core_store() {
        return Err("WebDAV adapter does not activate isolated core2 storage.".into());
    }
    let tx = store.connection().unchecked_transaction().map_err(err)?;
    let roots = roots_from(read_config(paths)?.as_ref())?;
    let (local, revision) = local_snapshot(store, &tx, &roots)?;
    if local.len() > limits.max_items {
        return Err("Favorite count exceeds synchronization budget.".into());
    }
    let shadows: BTreeMap<String, Shadow> = store
        .kv_get(SHADOW_KEY)
        .map_err(err)?
        .map(|s| serde_json::from_str(&s).map_err(err))
        .transpose()?
        .unwrap_or_default();
    let mut result = ExportBundle {
        entries: BTreeMap::new(),
        roots,
        blobs: BTreeMap::new(),
        revision,
        issues: Vec::new(),
    };
    let records: BTreeMap<String, FavorRecord> = local
        .iter()
        .map(|(id, value)| {
            serde_json::from_value(value["record"].clone())
                .map(|record| (id.clone(), record))
                .map_err(err)
        })
        .collect::<Result<_, _>>()?;
    // The production root-order cache deliberately tolerates stale IDs after
    // deleting a root folder. Export its effective membership without editing
    // the source config; the optimistic revision above still covers raw roots.
    let mut seen_roots = BTreeSet::new();
    result.roots.retain(|id| {
        records
            .get(id)
            .is_some_and(|r| r.is_folder && r.parent_id.is_none())
            && seen_roots.insert(id.clone())
    });
    if let Err(reason) = validate_graph(&records, &result.roots) {
        result.issues.push(SyncIssue {
            item_id: String::new(),
            resource: "favorite-graph".into(),
            reason,
        });
    }
    let mut fingerprints = BTreeMap::new();
    for (id, current) in local {
        let old = shadows.get(&id);
        let canonical =
            old.map(|s| json!({"record":s.portable.record,"document":s.portable.document}));
        let source = match (old, canonical) {
            (Some(s), Some(c)) => preserve_unchanged(&current, &s.local, &c),
            _ => current,
        };
        let mut portable = PortableFavorite {
            format: 1,
            record: source["record"].clone(),
            document: source.get("document").filter(|v| !v.is_null()).cloned(),
            resources: BTreeMap::new(),
        };
        let record: FavorRecord = serde_json::from_value(portable.record.clone()).map_err(err)?;
        let mut references = BTreeSet::new();
        if let Some(path) = &record.image_path {
            if !path.is_empty() {
                references.insert(path.clone());
            }
        }
        references.extend(record.file_paths.iter().filter(|s| !s.is_empty()).cloned());
        if is_path_icon(&record.icon) {
            references.insert(record.icon.clone());
        }
        if let Some(raw) = &portable.document {
            let mut images = std::collections::HashSet::new();
            crate::paths::collect_image_path_tokens(raw, &mut images);
            references.extend(images);
        }
        let mut rich = to_clip(&record);
        if let Err(_reason) =
            crate::csharp_import_assets::rewrite_embedded_images(&mut rich, &mut |path| {
                references.insert(path.into());
                Ok(resolve_source(paths, path)?.to_string_lossy().into_owned())
            })
        {
            result.issues.push(SyncIssue {
                item_id: id.clone(),
                resource: "rich-text".into(),
                reason: "rich_text_resource_not_portable".into(),
            });
        }
        for reference in references {
            let previous = old.and_then(|s| s.portable.resources.get(&reference));
            let attempt = (|| {
                let path = local_resource_path(paths, &reference, old)?;
                let bytes = read_bounded_local(&path, limits.max_file_bytes)?;
                let hash = digest(&bytes);
                if previous.is_some_and(|r| {
                    managed_path(paths, r, &reference).is_ok_and(|managed| managed == path)
                        && (r.sha256 != hash || r.byte_len != bytes.len() as u64)
                }) {
                    return Err("Managed attachment differs from its immutable identity.".into());
                }
                let total: u64 = result.blobs.values().map(|b| b.len() as u64).sum();
                if !result.blobs.contains_key(&hash)
                    && total.saturating_add(bytes.len() as u64) > limits.max_total_bytes
                {
                    return Err("Attachment total exceeds synchronization budget.".into());
                }
                let resource = Resource {
                    sha256: hash.clone(),
                    byte_len: bytes.len() as u64,
                    extension: previous
                        .map(|r| r.extension.clone())
                        .unwrap_or_else(|| extension(&path)),
                };
                fingerprints.insert(path.to_string_lossy().into_owned(), resource.clone());
                result.blobs.entry(hash).or_insert(bytes);
                Ok(resource)
            })();
            match attempt {
                Ok(resource) => {
                    portable.resources.insert(reference, resource);
                }
                Err(reason) => result.issues.push(SyncIssue {
                    item_id: id.clone(),
                    resource: reference,
                    reason,
                }),
            }
        }
        result.entries.insert(
            format!("favor/{id}"),
            serde_json::to_value(portable).map_err(err)?,
        );
    }
    result.revision = serde_json::to_string(&LocalRevision {
        rows: result.revision,
        resources: fingerprints,
    })
    .map_err(err)?;
    tx.commit().map_err(err)?;
    Ok(result)
}

fn to_clip(record: &FavorRecord) -> ClipRecord {
    let mut clip = ClipRecord::new_text("", &record.source_process_name);
    clip.kind = record.kind;
    clip.html_data = record.html_data.clone();
    clip.html_source_url = record.html_source_url.clone();
    clip.qq_rich_text = record.qq_rich_text.clone();
    clip
}
fn is_path_icon(text: &str) -> bool {
    !text.is_empty() && (Path::new(text).is_absolute() || text.starts_with("file:"))
}
fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_ascii_lowercase()
}
fn resolve_source(paths: &AppPaths, source: &str) -> Result<PathBuf, String> {
    if source.starts_with("file:") {
        return crate::explorer::file_url_to_local_path(source)
            .map(PathBuf::from)
            .ok_or_else(|| "Invalid file URL.".into());
    }
    let path = Path::new(source);
    if path.is_absolute() {
        Ok(path.into())
    } else {
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("Unsafe relative resource path.".into());
        }
        Ok(paths.image_library.join(path))
    }
}
fn local_resource_path(
    paths: &AppPaths,
    reference: &str,
    shadow: Option<&Shadow>,
) -> Result<PathBuf, String> {
    if let Some(path) = shadow.and_then(|s| s.resource_locations.get(reference)) {
        return Ok(PathBuf::from(path));
    }
    match shadow.and_then(|s| s.portable.resources.get(reference)) {
        Some(resource) => managed_path(paths, resource, reference),
        None => resolve_source(paths, reference),
    }
}
fn safe_metadata(meta: &fs::Metadata) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        return Err("Links are not uploaded or followed.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return Err("Reparse points and cloud placeholders are excluded.".into());
        }
    }
    Ok(())
}
fn inspect_components(path: &Path, allow_missing: bool) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("An absolute local resource path is required.".into());
    }
    let raw = path.to_string_lossy();
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return Err("Network resource paths are excluded.".into());
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        if matches!(part, Component::ParentDir | Component::CurDir) {
            return Err("Traversal in resource path is excluded.".into());
        }
        if let Component::Normal(name) = part {
            let name = name.to_string_lossy().to_ascii_lowercase();
            if name.starts_with("onedrive") || name == "dropbox" || name.starts_with("icloud") {
                return Err("Cloud sync directories are excluded.".into());
            }
        }
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) => safe_metadata(&meta)?,
            Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
    }
    Ok(())
}
fn read_bounded_local(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    inspect_components(path, false)?;
    let meta = fs::symlink_metadata(path).map_err(err)?;
    if !meta.is_file() {
        return Err("Directories are not recursively uploaded.".into());
    }
    if meta.len() > limit {
        return Err("Attachment exceeds single-file budget.".into());
    }
    let mut file = fs::OpenOptions::new();
    file.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        file.share_mode(1);
        file.custom_flags(0x00200000);
    }
    let file = file.open(path).map_err(err)?;
    safe_metadata(&file.metadata().map_err(err)?)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() as u64 > limit {
        return Err("Attachment grew beyond single-file budget.".into());
    }
    Ok(bytes)
}
fn safe_attachment_name(source: &str, resource: &Resource) -> String {
    let decoded = if source.starts_with("file:") {
        crate::explorer::file_url_to_local_path(source).map(PathBuf::from)
    } else {
        None
    };
    let candidate = decoded.as_deref().unwrap_or_else(|| Path::new(source));
    let name = candidate.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let base = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let device = matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 240
        && name.encode_utf16().count() <= 180
        && !name.ends_with([' ', '.'])
        && !device
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        });
    if safe {
        name.to_owned()
    } else {
        format!("{}.{}", resource.sha256, resource.extension)
    }
}
fn validate_resource_identity(resource: &Resource) -> Result<(), String> {
    if !valid_hash(&resource.sha256)
        || resource.extension.is_empty()
        || resource.extension.len() > 10
        || !resource
            .extension
            .bytes()
            .all(|b| b.is_ascii_alphanumeric())
    {
        return Err("Invalid attachment identity.".into());
    }
    Ok(())
}
fn managed_path(paths: &AppPaths, resource: &Resource, source: &str) -> Result<PathBuf, String> {
    validate_resource_identity(resource)?;
    Ok(paths
        .data_root
        .join("WebDavSync")
        .join("attachments")
        .join(&resource.sha256)
        .join(safe_attachment_name(source, resource)))
}
fn ensure_directory(path: &Path) -> Result<(), String> {
    inspect_components(path, true)?;
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                safe_metadata(&meta)?;
                if !meta.is_dir() {
                    return Err("Managed path component is not a directory.".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(err)?;
                safe_metadata(&fs::symlink_metadata(&current).map_err(err)?)?;
            }
            Err(e) => return Err(err(e)),
        }
    }
    Ok(())
}
fn publish_attachment(
    paths: &AppPaths,
    resource: &Resource,
    source: &str,
    bytes: &[u8],
    limits: &Limits,
) -> Result<String, String> {
    if resource.byte_len > limits.max_file_bytes
        || resource.byte_len != bytes.len() as u64
        || digest(bytes) != resource.sha256
    {
        return Err("Attachment length or SHA-256 validation failed.".into());
    }
    let path = managed_path(paths, resource, source)?;
    let parent = path.parent().ok_or("Missing managed attachment parent.")?;
    ensure_directory(parent)?;
    if path.exists() {
        if read_bounded_local(&path, limits.max_file_bytes)? != bytes {
            return Err("Existing immutable attachment is corrupt.".into());
        }
    } else {
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(err)?;
        temp.write_all(bytes).map_err(err)?;
        temp.as_file().sync_all().map_err(err)?;
        if let Err(error) = temp.persist_noclobber(&path) {
            if error.error.kind() != std::io::ErrorKind::AlreadyExists
                || read_bounded_local(&path, limits.max_file_bytes)? != bytes
            {
                return Err(err(error.error));
            }
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

fn validate_graph(records: &BTreeMap<String, FavorRecord>, roots: &[String]) -> Result<(), String> {
    let mut root_set = BTreeSet::new();
    for root in roots {
        if !root_set.insert(root)
            || !records
                .get(root)
                .is_some_and(|r| r.is_folder && r.parent_id.is_none())
        {
            return Err("Invalid favorite root reference.".into());
        }
    }
    for item in records.values() {
        if let Some(parent) = &item.parent_id {
            if parent == &item.id || !records.get(parent).is_some_and(|r| r.is_folder) {
                return Err("Invalid favorite owner.".into());
            }
        }
        let mut seen = BTreeSet::new();
        for child in &item.children {
            if !item.is_folder || !seen.insert(child) || !records.contains_key(child) {
                return Err("Invalid favorite ordered reference.".into());
            }
        }
    }
    // Ownership-only compatibility edges also participate in cycle detection.
    // Use an explicit stack: a remote chain must not exhaust the thread stack.
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = records
        .iter()
        .map(|(id, item)| {
            (
                id.as_str(),
                item.children.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    for item in records.values() {
        if let Some(parent) = &item.parent_id {
            edges
                .get_mut(parent.as_str())
                .unwrap()
                .insert(item.id.as_str());
        }
    }
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    for id in records.keys() {
        let mut stack = vec![(id.as_str(), false)];
        while let Some((current, leaving)) = stack.pop() {
            if leaving {
                active.remove(current);
                done.insert(current);
                continue;
            }
            if done.contains(current) {
                continue;
            }
            if !active.insert(current) {
                return Err("Favorite reference graph contains a cycle.".into());
            }
            stack.push((current, true));
            for child in edges[current].iter().rev() {
                stack.push((child, false));
            }
        }
    }
    Ok(())
}
fn map_raw_paths(value: &mut Value, mapping: &BTreeMap<String, String>) {
    match value {
        Value::Object(m) => {
            for v in m.values_mut() {
                map_raw_paths(v, mapping)
            }
        }
        Value::Array(a) => {
            for v in a {
                map_raw_paths(v, mapping)
            }
        }
        Value::String(s) => {
            if let Some(path) = mapping.get(s) {
                *s = path.clone()
            }
        }
        _ => {}
    }
}
fn restore_document(tx: &Transaction<'_>, record: &FavorRecord, raw: &Value) -> Result<(), String> {
    // Modeled rows have already been written through the normal cache triggers.
    // Restore complete unmodeled fields, with the same versioned-writer gate.
    let current: String = tx
        .query_row(
            "SELECT Data FROM main.\"favor_item\" WHERE Id=?1",
            [&record.id],
            |row| row.get(0),
        )
        .map_err(err)?;
    let current: Value = serde_json::from_str(&current).map_err(err)?;
    if &current == raw {
        // In particular, do not rotate RowToken while legacy_data stays equal:
        // that would re-enter the ordinary cache trigger with its old CAS token.
        return Ok(());
    }
    let columns: Vec<String> = {
        let mut s = tx
            .prepare("PRAGMA main.table_info('favor_item')")
            .map_err(err)?;
        let rows = s.query_map([], |r| r.get(1)).map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)?
    };
    let token = columns.iter().any(|c| c.eq_ignore_ascii_case("RowToken"));
    let gate:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='__clip_versioned_write_gate')",[],|r|r.get(0)).map_err(err)?;
    if gate {
        tx.execute(
            "UPDATE __clip_versioned_write_gate SET Permit=1 WHERE TableName='favor_item'",
            [],
        )
        .map_err(err)?;
    }
    let token_update = if token {
        ",RowToken=lower(hex(randomblob(16)))"
    } else {
        ""
    };
    let result = tx.execute(
        &format!("UPDATE main.\"favor_item\" SET Data=?2{token_update} WHERE Id=?1"),
        params![record.id, serde_json::to_string(raw).map_err(err)?],
    );
    if gate {
        tx.execute(
            "UPDATE __clip_versioned_write_gate SET Permit=0 WHERE TableName='favor_item'",
            [],
        )
        .map_err(err)?;
    }
    if result.map_err(err)? != 1 {
        return Err("Imported favorite document is missing.".into());
    }
    let token_expr = if token { "RowToken" } else { "NULL" };
    tx.execute(&format!("UPDATE rust.cliphost_cache_favor_item SET legacy_data=(SELECT Data FROM main.\"favor_item\" WHERE Id=?1),legacy_token=(SELECT {token_expr} FROM main.\"favor_item\" WHERE Id=?1),legacy_revision=(SELECT cliphost_document_revision(Data,{token_expr}) FROM main.\"favor_item\" WHERE Id=?1) WHERE id=?1"),[&record.id]).map_err(err)?;
    Ok(())
}

/// Validate a complete candidate before the engine publishes its head. This is
/// pure: no source/managed paths are opened and no temporary files are created.
pub fn validate_portable(
    entries: &BTreeMap<String, Value>,
    roots: &[String],
    blobs: &BTreeMap<String, Vec<u8>>,
    limits: &Limits,
) -> Result<(), String> {
    if entries.len() > limits.max_items {
        return Err("Favorite count exceeds synchronization budget.".into());
    }
    let mut total = 0u64;
    for (hash, bytes) in blobs {
        if !valid_hash(hash) || digest(bytes) != *hash {
            return Err("Attachment SHA-256 validation failed.".into());
        }
        if bytes.len() as u64 > limits.max_file_bytes {
            return Err("Attachment exceeds single-file budget.".into());
        }
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("Attachment byte budget overflow.")?;
        if total > limits.max_total_bytes {
            return Err("Attachment total exceeds synchronization budget.".into());
        }
    }
    let mut records = BTreeMap::new();
    for (key, value) in entries {
        let source: PortableFavorite = serde_json::from_value(value.clone()).map_err(err)?;
        if source.format != 1 {
            return Err("Unsupported favorite portable format.".into());
        }
        let record: FavorRecord = serde_json::from_value(source.record.clone()).map_err(err)?;
        if record.id.is_empty() || key != &format!("favor/{}", record.id) {
            return Err("Favorite entry identity mismatch.".into());
        }
        for resource in source.resources.values() {
            validate_resource_identity(resource)?;
            let bytes = blobs
                .get(&resource.sha256)
                .ok_or("Attachment bytes are incomplete; favorites were not applied.")?;
            if resource.byte_len != bytes.len() as u64 {
                return Err("Attachment length validation failed.".into());
            }
        }
        let require = |path: &str| -> Result<(), String> {
            if source.resources.contains_key(path) {
                Ok(())
            } else {
                Err("Attachment reference is not portable.".into())
            }
        };
        if let Some(path) = record.image_path.as_deref().filter(|p| !p.is_empty()) {
            require(path)?;
        }
        for path in &record.file_paths {
            require(path)?;
        }
        if is_path_icon(&record.icon) {
            require(&record.icon)?;
        }
        if let Some(raw) = &source.document {
            let mut references = std::collections::HashSet::new();
            crate::paths::collect_image_path_tokens(raw, &mut references);
            for path in references {
                require(&path)?;
            }
        }
        let mut rich = to_clip(&record);
        crate::csharp_import_assets::rewrite_embedded_images(&mut rich, &mut |path| {
            require(path)?;
            // Only exercise the markup codec. This syntactically absolute path
            // is never opened and is not retained in any snapshot.
            Ok(if cfg!(windows) {
                "C:\\cliphost-validation\\resource.bin"
            } else {
                "/cliphost-validation/resource.bin"
            }
            .into())
        })
        .map_err(|_| "rich_text_resource_not_portable".to_owned())?;
        crate::csharp_store_document::write_document(
            "favor_item",
            &source.record,
            source.document.as_ref(),
        )
        .map_err(err)?;
        records.insert(record.id.clone(), record);
    }
    validate_graph(&records, roots)
}

fn upsert_exact(store: &AppStore, record: &FavorRecord) -> Result<(), String> {
    match store.get_favor(&record.id) {
        Ok(current) => store
            .connection()
            .verify_revision(
                "favor_item",
                &record.id,
                current.storage_revision.as_deref(),
            )
            .map_err(err)?,
        Err(crate::store::StoreError::NotFound(_)) => {}
        Err(error) => return Err(err(error)),
    }
    store.connection().execute(
        "INSERT INTO favor_item (id,parent_id,is_folder,name,kind,body_text,file_paths,image_path,children,create_time,update_time,source_clip_id,html_data,html_source_url,qq_rich_text,source_process,sub_index,icon,execute_param)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(id) DO UPDATE SET parent_id=excluded.parent_id,is_folder=excluded.is_folder,name=excluded.name,kind=excluded.kind,body_text=excluded.body_text,file_paths=excluded.file_paths,image_path=excluded.image_path,children=excluded.children,create_time=excluded.create_time,update_time=excluded.update_time,source_clip_id=excluded.source_clip_id,html_data=excluded.html_data,html_source_url=excluded.html_source_url,qq_rich_text=excluded.qq_rich_text,source_process=excluded.source_process,sub_index=excluded.sub_index,icon=excluded.icon,execute_param=excluded.execute_param",
        params![record.id,record.parent_id,record.is_folder as i32,record.name,record.kind.as_str(),record.body_text,serde_json::to_string(&record.file_paths).map_err(err)?,record.image_path,serde_json::to_string(&record.children).map_err(err)?,record.create_time.timestamp_millis(),record.update_time.timestamp_millis(),record.source_clip_id,record.html_data,record.html_source_url,record.qq_rich_text,record.source_process_name,record.sub_index,record.icon,record.execute_param]).map_err(err)?;
    Ok(())
}

pub fn apply_favorites(
    store: &AppStore,
    paths: &AppPaths,
    plan: &ApplyPlan,
    limits: &Limits,
) -> Result<(), String> {
    recover_apply(store, paths)?;
    if store.is_core_store() {
        return Err("WebDAV adapter does not activate isolated core2 storage.".into());
    }
    validate_portable(&plan.entries, &plan.roots, &plan.blobs, limits)?;
    if plan.entries.len() > limits.max_items {
        return Err("Favorite count exceeds synchronization budget.".into());
    }
    if plan
        .blobs
        .values()
        .try_fold(0u64, |n, b| n.checked_add(b.len() as u64))
        .ok_or("Attachment byte budget overflow.")?
        > limits.max_total_bytes
    {
        return Err("Attachment total exceeds synchronization budget.".into());
    }
    if plan
        .receipt
        .as_ref()
        .is_some_and(|(k, _)| k == SHADOW_KEY || k == APPLY_RECEIPT)
    {
        return Err("Reserved synchronization receipt key.".into());
    }
    let before_bundle = export_favorites(store, paths, limits)?;
    if before_bundle.revision != plan.expected_revision {
        let expected: LocalRevision = serde_json::from_str(&plan.expected_revision).map_err(err)?;
        let actual: LocalRevision = serde_json::from_str(&before_bundle.revision).map_err(err)?;
        return Err(if expected.rows == actual.rows {
            "WRITE_CONFLICT: a source attachment changed during synchronization."
        } else {
            "WRITE_CONFLICT: favorites changed during synchronization."
        }
        .into());
    }
    if !before_bundle.issues.is_empty() {
        return Err("Local favorite resources are incomplete; favorites were not applied.".into());
    }
    let previous_shadows: BTreeMap<String, Shadow> = store
        .kv_get(SHADOW_KEY)
        .map_err(err)?
        .map(|raw| serde_json::from_str(&raw).map_err(err))
        .transpose()?
        .unwrap_or_default();
    let mut portable = BTreeMap::new();
    let mut materialized = BTreeMap::new();
    let mut documents = BTreeMap::new();
    let mut local_resources = BTreeMap::new();
    let mut unchanged = BTreeSet::new();
    for (key, value) in &plan.entries {
        let source: PortableFavorite = serde_json::from_value(value.clone()).map_err(err)?;
        if source.format != 1 {
            return Err("Unsupported favorite portable format.".into());
        }
        let mut record: FavorRecord = serde_json::from_value(source.record.clone()).map_err(err)?;
        if key != &format!("favor/{}", record.id) || record.id.is_empty() {
            return Err("Favorite entry identity mismatch.".into());
        }
        let known = before_bundle
            .entries
            .get(key)
            .map(|value| serde_json::from_value::<PortableFavorite>(value.clone()).map_err(err))
            .transpose()?;
        if before_bundle.entries.get(key) == Some(value) {
            let current = store.get_favor(&record.id).map_err(err)?;
            let locations = source
                .resources
                .keys()
                .map(|reference| {
                    Ok((
                        reference.clone(),
                        local_resource_path(paths, reference, previous_shadows.get(&record.id))?
                            .to_string_lossy()
                            .into_owned(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>, String>>()?;
            unchanged.insert(record.id.clone());
            local_resources.insert(record.id.clone(), locations);
            portable.insert(record.id.clone(), source);
            materialized.insert(record.id.clone(), current);
            continue;
        }
        let mut mapping = BTreeMap::new();
        for (original, resource) in &source.resources {
            let bytes = plan
                .blobs
                .get(&resource.sha256)
                .ok_or("Attachment bytes are incomplete; favorites were not applied.")?;
            let path = if known.as_ref().and_then(|p| p.resources.get(original)) == Some(resource) {
                local_resource_path(paths, original, previous_shadows.get(&record.id))?
                    .to_string_lossy()
                    .into_owned()
            } else {
                publish_attachment(paths, resource, original, bytes, limits)?
            };
            mapping.insert(original.clone(), path);
        }
        let resolve = |path: &str| {
            mapping
                .get(path)
                .cloned()
                .ok_or_else(|| "Attachment reference is not portable.".to_owned())
        };
        if let Some(path) = &record.image_path {
            if !path.is_empty() {
                record.image_path = Some(resolve(path)?);
            }
        }
        record.file_paths = record
            .file_paths
            .iter()
            .map(|p| resolve(p))
            .collect::<Result<_, _>>()?;
        if is_path_icon(&record.icon) {
            record.icon = resolve(&record.icon)?;
        }
        let mut rich = to_clip(&record);
        crate::csharp_import_assets::rewrite_embedded_images(&mut rich, &mut |p| resolve(p))
            .map_err(|_| "rich_text_resource_not_portable".to_owned())?;
        record.html_data = rich.html_data;
        record.qq_rich_text = rich.qq_rich_text;
        record.storage_revision = None;
        let raw = source
            .document
            .clone()
            .map(|mut raw| {
                map_raw_paths(&mut raw, &mapping);
                crate::csharp_store_document::write_document(
                    "favor_item",
                    &serde_json::to_value(&record).map_err(err)?,
                    Some(&raw),
                )
                .map_err(err)
            })
            .transpose()?;
        if let Some(raw) = raw {
            documents.insert(record.id.clone(), raw);
        }
        local_resources.insert(record.id.clone(), mapping);
        portable.insert(record.id.clone(), source);
        materialized.insert(record.id.clone(), record);
    }
    validate_graph(&materialized, &plan.roots)?;
    let journal_path = paths.data_root.join(JOURNAL);
    let operation_id = uuid::Uuid::new_v4().simple().to_string();
    let outcome = (|| {
        let tx = store.connection().unchecked_transaction().map_err(err)?;
        let before = read_config(paths)?;
        let roots = roots_from(before.as_ref())?;
        let (before_local, actual) = local_snapshot(store, &tx, &roots)?;
        let expected: LocalRevision = serde_json::from_str(&plan.expected_revision)
            .map_err(|_| "Invalid local synchronization revision.")?;
        if actual != expected.rows {
            return Err("WRITE_CONFLICT: favorites changed during synchronization.".into());
        }
        for (source, identity) in expected.resources {
            let bytes =
                read_bounded_local(Path::new(&source), limits.max_file_bytes).map_err(|_| {
                    "WRITE_CONFLICT: a source attachment became unavailable during synchronization."
                })?;
            if bytes.len() as u64 != identity.byte_len || digest(&bytes) != identity.sha256 {
                return Err(
                    "WRITE_CONFLICT: a source attachment changed during synchronization.".into(),
                );
            }
        }
        let after = set_roots(before.clone().unwrap_or_else(|| json!({})), &plan.roots)?;
        crate::config_file::save(
            &journal_path,
            &ApplyJournal {
                operation_id: operation_id.clone(),
                before: before.clone(),
                after: after.clone(),
            },
        )?;
        let removed: Vec<String> = before_local
            .keys()
            .filter(|id| !materialized.contains_key(*id))
            .cloned()
            .collect();
        store.delete_favors_exact(&removed).map_err(err)?;
        for (id, record) in &materialized {
            if unchanged.contains(id) {
                continue;
            }
            let projection = serde_json::to_value(record).map_err(err)?;
            let projection_same = before_local
                .get(id)
                .is_some_and(|old| old["record"] == projection);
            let raw_same = !store.is_document_store()
                || documents.get(id).is_none_or(|raw| {
                    before_local.get(id).and_then(|old| old.get("document")) == Some(raw)
                });
            if projection_same && raw_same {
                continue;
            }
            upsert_exact(store, record)?;
            if store.is_document_store() {
                if let Some(raw) = documents.get(id) {
                    restore_document(&tx, record, raw)?;
                }
            }
        }
        let (local, _) = local_snapshot(store, &tx, &plan.roots)?;
        let shadows: BTreeMap<String, Shadow> = portable
            .into_iter()
            .map(|(id, p)| {
                let shadow = Shadow {
                    local: local[&id].clone(),
                    portable: p,
                    resource_locations: local_resources.remove(&id).unwrap_or_default(),
                };
                (id, shadow)
            })
            .collect();
        store
            .kv_set(SHADOW_KEY, &serde_json::to_string(&shadows).map_err(err)?)
            .map_err(err)?;
        store.kv_set(APPLY_RECEIPT, &operation_id).map_err(err)?;
        if let Some((key, value)) = &plan.receipt {
            store.kv_set(key, value).map_err(err)?;
        }
        // Check the config again after potentially lengthy DB work.
        if read_config(paths)? != before {
            return Err("WRITE_CONFLICT: favorite configuration changed during apply.".into());
        }
        if before.as_ref() != Some(&after) {
            crate::config_file::save(&config_path(paths), &after)?;
        }
        tx.commit().map_err(err)
    })();
    match outcome {
        Ok(()) => {
            recover_apply(store, paths)?;
            Ok(())
        }
        Err(error) => match recover_apply(store, paths) {
            Ok(()) => Err(error),
            Err(recovery) => Err(format!("{error}; recovery is pending: {recovery}")),
        },
    }
}

/// Run before reading or writing the synchronized collection. A DB receipt
/// chooses forward/rollback; an intervening external config write stops recovery.
pub fn recover_apply(store: &AppStore, paths: &AppPaths) -> Result<(), String> {
    let path = paths.data_root.join(JOURNAL);
    let bytes = match fs::symlink_metadata(&path) {
        Ok(meta) => {
            safe_metadata(&meta)?;
            fs::read(&path).map_err(err)?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(err(e)),
    };
    let journal: ApplyJournal = serde_json::from_slice(&bytes).map_err(err)?;
    let current = read_config(paths)?;
    if current != journal.before && current.as_ref() != Some(&journal.after) {
        return Err(
            "Favorite config changed outside interrupted apply; recovery journal was preserved."
                .into(),
        );
    }
    let committed =
        store.kv_get(APPLY_RECEIPT).map_err(err)?.as_deref() == Some(journal.operation_id.as_str());
    let wanted = if committed {
        Some(journal.after)
    } else {
        journal.before
    };
    if current != wanted {
        match wanted {
            Some(value) => crate::config_file::save(&config_path(paths), &value)?,
            None => {
                fs::remove_file(config_path(paths)).map_err(err)?;
            }
        }
    }
    fs::remove_file(path).map_err(err)?;
    Ok(())
}

#[cfg(test)]
#[path = "webdav_favorites_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "webdav_favorites_integration_tests.rs"]
mod integration_tests;
