//! Read-only conversion of supported v1 item stores into the v2 content model.
//! This module never opens a path, creates a table, rewrites source markup, or
//! mutates the supplied connection. The coordinator supplies a fixed snapshot.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, SecondsFormat, TimeZone, Utc};
use rusqlite::{types::ValueRef, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::model::{ClipKind, ClipRecord, FavorRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Documents,
    Native,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertedSource {
    pub format: SourceFormat,
    pub items: Vec<ConvertedItem>,
    pub archive: Vec<ArchivedRow>,
    pub orphan_usage: Vec<String>,
    /// Metadata only: original values remain in the complete row archive.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertedItem {
    pub scope: String,
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub icon_override: Option<String>,
    pub description: String,
    pub payload_json: Value,
    pub search_text: String,
    pub dedupe_key: String,
    pub source_process: String,
    pub created_at: String,
    pub updated_at: String,
    pub sub_index: i32,
    pub is_pinned: bool,
    pub usage_count: i64,
    pub last_used_at: Option<String>,
    pub owner_parent_id: Option<String>,
    pub children: Option<Vec<String>>,
    pub source_history_id: Option<String>,
    pub asset_candidates: Vec<AssetCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetCandidate {
    pub slot: String,
    pub ordinal: u32,
    pub original_locator: String,
    /// Resolution hint only. The asset importer validates containment and file
    /// identity; conversion does not read the file or decide that it exists.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedRow {
    pub table: String,
    pub rowid: Option<i64>,
    pub cells: BTreeMap<String, ArchivedCell>,
}

/// Typed SQL cells preserve NULL, text, blobs and even non-finite SQLite REALs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ArchivedCell {
    Null,
    Integer(i64),
    RealBits(String),
    Text(String),
    Blob(Vec<u8>),
}

impl ArchivedRow {
    pub(crate) fn cell(&self, name: &str) -> Option<&ArchivedCell> {
        self.cells
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }
    pub(crate) fn text(&self, name: &str) -> Result<Option<String>, String> {
        match self.cell(name) {
            None | Some(ArchivedCell::Null) => Ok(None),
            Some(ArchivedCell::Text(value)) => Ok(Some(value.clone())),
            _ => Err(format!("{}.{name} must be TEXT or NULL", self.table)),
        }
    }
    pub(crate) fn required_text(&self, name: &str) -> Result<String, String> {
        self.text(name)?
            .ok_or_else(|| format!("{}.{name} is missing or NULL", self.table))
    }
    pub(crate) fn integer(&self, name: &str) -> Result<Option<i64>, String> {
        match self.cell(name) {
            None | Some(ArchivedCell::Null) => Ok(None),
            Some(ArchivedCell::Integer(value)) => Ok(Some(*value)),
            _ => Err(format!("{}.{name} must be INTEGER or NULL", self.table)),
        }
    }
}

pub fn read_source(connection: &Connection, image_root: &Path) -> Result<ConvertedSource, String> {
    read_source_impl(connection, image_root, true)
}

thread_local! { static PURE_CORE_READER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// CORE owns its nullable/unknown usage semantics and never probes external
/// files while building a disposable derived index. The full prototype keeps
/// its original behavior through read_source above.
pub(crate) fn read_source_core(connection: &Connection, image_root: &Path) -> Result<ConvertedSource, String> {
    struct Restore(bool);
    impl Drop for Restore { fn drop(&mut self) { PURE_CORE_READER.with(|flag| flag.set(self.0)); } }
    let _restore = Restore(PURE_CORE_READER.with(|flag| flag.replace(true)));
    read_source_impl(connection, image_root, false)
}

fn read_source_impl(connection: &Connection, image_root: &Path, include_usage: bool) -> Result<ConvertedSource, String> {
    let tables = table_names(connection)?;
    if tables
        .iter()
        .any(|name| name.eq_ignore_ascii_case("store_info"))
    {
        return Err(
            "Source already has store_info; refuse to reinterpret a v2 or interrupted target as v1"
                .into(),
        );
    }
    let clip_columns = columns(connection, "clip_item")?;
    let favor_columns = columns(connection, "favor_item")?;
    let classify = |names: &[String]| -> Result<Option<SourceFormat>, String> {
        if names.is_empty() {
            return Ok(None);
        }
        let has = |name: &str| names.iter().any(|v| v.eq_ignore_ascii_case(name));
        if has("Id") && has("Data") && !has("kind") {
            Ok(Some(SourceFormat::Documents))
        } else if has("id") && has("kind") && !has("Data") {
            Ok(Some(SourceFormat::Native))
        } else {
            Err(format!("Unsupported source item schema: {names:?}"))
        }
    };
    let clip_format = classify(&clip_columns)?;
    let favor_format = classify(&favor_columns)?;
    let format = match (clip_format, favor_format) {
        (Some(a), Some(b)) if a != b => return Err("Mixed native/document item tables cannot be migrated safely".into()),
        (Some(a), _) | (_, Some(a)) => a,
        _ => return Err("No supported v1 item tables; ancient ClipboardItem/FavorItem or an empty/unknown database requires explicit initialization or a dedicated migration".into()),
    };
    for (modern, old) in [("clip_item", "ClipboardItem"), ("favor_item", "FavorItem")] {
        let modern_exists = !columns(connection, modern)?.is_empty();
        let modern_count: i64 = if modern_exists {
            connection
                .query_row(
                    &format!("SELECT count(*) FROM {}", quote(modern)),
                    [],
                    |r| r.get(0),
                )
                .map_err(sql_error)?
        } else {
            0
        };
        if !modern_exists || modern_count == 0 {
            if let Some(old) = tables.iter().find(|name| name.eq_ignore_ascii_case(old)) {
                let count: i64 = connection
                    .query_row(&format!("SELECT count(*) FROM {}", quote(old)), [], |r| {
                        r.get(0)
                    })
                    .map_err(sql_error)?;
                if count != 0 {
                    return Err(format!("Ancient table {old} contains records but {modern} is absent or empty; migration completion is ambiguous"));
                }
            }
        }
    }
    let mut converted = ConvertedSource {
        format,
        items: Vec::new(),
        archive: Vec::new(),
        orphan_usage: Vec::new(),
        warnings: Vec::new(),
    };
    let mut identities = HashSet::new();
    for (table, scope, exists) in [
        ("clip_item", "history", !clip_columns.is_empty()),
        ("favor_item", "favorite", !favor_columns.is_empty()),
    ] {
        if !exists {
            continue;
        }
        for row in read_rows(connection, table)? {
            let id = row.required_text("id")?;
            if id.trim().is_empty() || !identities.insert((scope.to_owned(), id.clone())) {
                return Err(format!("Empty or duplicate {scope} source item ID"));
            }
            let item = match format {
                SourceFormat::Documents => {
                    convert_document(&row, scope, image_root, &mut converted.warnings)
                }
                SourceFormat::Native => convert_native(&row, scope, image_root),
            }
            .map_err(|error| format!("Cannot convert {scope}/{id}: {error}"))?;
            converted.items.push(item);
            converted.archive.push(row);
        }
    }
    if include_usage { merge_usage(connection, &tables, &mut converted)?; }
    Ok(converted)
}

/// Normalize only the already-supported ParentId-only fallback. Invalid edges
/// and cycles are reported, never silently removed. On error `source` is intact.
/// The returned root members belong to the system root inserted by the caller.
pub fn normalize_favorites(
    source: &mut ConvertedSource,
    root_children: &[String],
    root_id: &str,
) -> Result<Vec<String>, String> {
    if root_id.trim().is_empty() {
        return Err("System root ID is empty".into());
    }
    let mut items = source.items.clone();
    let index: HashMap<String, usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.scope == "favorite")
        .map(|(n, item)| (item.id.clone(), n))
        .collect();
    if index.contains_key(root_id) {
        return Err("System root ID collides with an existing favorite".into());
    }
    for (id, &n) in &index {
        let item = &items[n];
        if let Some(parent) = item.owner_parent_id.as_deref() {
            let parent_index = index
                .get(parent)
                .ok_or_else(|| format!("Favorite {id} has missing owner {parent}"))?;
            if items[*parent_index].kind != "folder" {
                return Err(format!("Favorite {id} owner {parent} is not a folder"));
            }
            if parent == id {
                return Err(format!("Favorite {id} owns itself"));
            }
        }
        let mut seen = HashSet::new();
        for child in item.children.iter().flatten() {
            if !index.contains_key(child) {
                return Err(format!("Favorite {id} contains missing child {child}"));
            }
            if !seen.insert(child) {
                return Err(format!("Favorite {id} contains duplicate child {child}"));
            }
            if child == id {
                return Err(format!("Favorite {id} references itself"));
            }
        }
    }
    let mut fallback: Vec<usize> = index.values().copied().collect();
    fallback.sort_by(|a, b| {
        items[*a]
            .sub_index
            .cmp(&items[*b].sub_index)
            .then(items[*a].created_at.cmp(&items[*b].created_at))
            .then(items[*a].id.cmp(&items[*b].id))
    });
    for &n in &fallback {
        if let Some(parent) = items[n].owner_parent_id.clone() {
            let id = items[n].id.clone();
            let children = items[*index.get(&parent).unwrap()]
                .children
                .as_mut()
                .ok_or_else(|| format!("Owner {parent} has no folder Children"))?;
            if !children.contains(&id) {
                children.push(id);
            }
        }
    }
    validate_dag(&items, &index, false)?;
    validate_dag(&items, &index, true)?;
    let referenced: HashSet<String> = index
        .values()
        .flat_map(|n| items[*n].children.iter().flatten().cloned())
        .collect();
    let mut roots = Vec::new();
    let mut seen_roots = HashSet::new();
    for id in root_children {
        let &n = index
            .get(id)
            .ok_or_else(|| format!("Root Children contains missing favorite {id}"))?;
        if !seen_roots.insert(id) {
            return Err(format!("Root Children contains duplicate favorite {id}"));
        }
        if items[n].owner_parent_id.is_some() {
            return Err(format!(
                "Root Children contains owned non-root favorite {id}"
            ));
        }
        // Existing graph projection omits roots already occurring in a folder.
        if !referenced.contains(id) {
            roots.push(id.clone());
        }
    }
    let mut missing_folders: Vec<usize> = fallback
        .iter()
        .copied()
        .filter(|n| {
            items[*n].kind == "folder"
                && items[*n].owner_parent_id.is_none()
                && !referenced.contains(&items[*n].id)
                && !roots.contains(&items[*n].id)
        })
        .collect();
    // normalized_root_order sorted names stably over list_favors' ascending
    // projected milliseconds, retaining source row order for equal times. IDs
    // must not become a new tie-breaker for identically named root folders.
    missing_folders.sort_by(|a, b| {
        items[*a]
            .title
            .as_deref()
            .unwrap_or("【收藏夹】")
            .to_lowercase()
            .cmp(
                &items[*b]
                    .title
                    .as_deref()
                    .unwrap_or("【收藏夹】")
                    .to_lowercase(),
            )
            .then(
                items[*a]
                    .created_at
                    .get(..23)
                    .cmp(&items[*b].created_at.get(..23)),
            )
            .then(a.cmp(b))
    });
    roots.extend(missing_folders.iter().map(|n| items[*n].id.clone()));
    for &n in &fallback {
        if items[n].owner_parent_id.is_none()
            && !referenced.contains(&items[n].id)
            && !roots.contains(&items[n].id)
        {
            roots.push(items[n].id.clone());
        }
    }
    for &n in index.values() {
        if items[n].owner_parent_id.is_none() {
            items[n].owner_parent_id = Some(root_id.into());
        }
    }
    source.items = items;
    Ok(roots)
}

fn validate_dag(
    items: &[ConvertedItem],
    index: &HashMap<String, usize>,
    ownership: bool,
) -> Result<(), String> {
    let mut degree: HashMap<String, usize> = index.keys().map(|id| (id.clone(), 0)).collect();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for (id, &n) in index {
        if ownership {
            if let Some(parent) = &items[n].owner_parent_id {
                edges.entry(parent.clone()).or_default().push(id.clone());
                *degree.get_mut(id).unwrap() += 1;
            }
        } else {
            for child in items[n].children.iter().flatten() {
                edges.entry(id.clone()).or_default().push(child.clone());
                *degree.get_mut(child).unwrap() += 1;
            }
        }
    }
    let mut pending: VecDeque<String> = degree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut visited = 0;
    while let Some(id) = pending.pop_front() {
        visited += 1;
        for child in edges.get(&id).into_iter().flatten() {
            let count = degree.get_mut(child).unwrap();
            *count -= 1;
            if *count == 0 {
                pending.push_back(child.clone());
            }
        }
    }
    if visited != index.len() {
        return Err(format!(
            "Favorite {} graph contains a cycle",
            if ownership { "ownership" } else { "Children" }
        ));
    }
    Ok(())
}

fn convert_document(
    row: &ArchivedRow,
    scope: &str,
    image_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<ConvertedItem, String> {
    let id = row.required_text("Id")?;
    let raw = row.required_text("Data")?;
    let document: Value =
        serde_json::from_str(&raw).map_err(|error| format!("Invalid document JSON: {error}"))?;
    let clip = crate::csharp_store_document::parse_clip(&id, &document)?;
    note_local_time(
        warnings,
        scope,
        &id,
        "CreateTime",
        field(&document, "CreateTime"),
        clip.create_time,
    );
    note_local_time(
        warnings,
        scope,
        &id,
        "UpdateTime",
        field(&document, "UpdateTime"),
        clip.update_time,
    );
    let favorite = if scope == "favorite" {
        Some(crate::csharp_store_document::parse_favor(&id, &document)?)
    } else {
        None
    };
    let original = field(&document, "Item").ok_or("Missing Item")?;
    let mut item = from_clip(&clip, scope, image_root)?;
    // The compatibility parser computes effective values. Migration preserves
    // overrides separately from Link.Item's own source metadata instead.
    item.title = optional_text(&document, "Title")?;
    item.icon_override = optional_text(&document, "Icon")?;
    item.payload_json = document_payload(&clip, original)?;
    if let Some(favorite) = favorite {
        item.owner_parent_id = favorite.parent_id;
        item.children = if favorite.is_folder {
            Some(favorite.children)
        } else {
            None
        };
        item.source_history_id = favorite.source_clip_id;
        item.is_pinned = false;
    }
    item.asset_candidates
        .retain(|candidate| candidate.slot == "image");
    add_icon_candidate(
        &mut item,
        "override_icon",
        optional_text(&document, "Icon")?.as_deref(),
        image_root,
    );
    add_icon_candidate(
        &mut item,
        "content_icon",
        optional_text(original, "Icon")?.as_deref(),
        image_root,
    );
    let known_document = [
        "Id",
        "Item",
        "Title",
        "Description",
        "Icon",
        "CreateTime",
        "UpdateTime",
        "SourceProcessName",
        "IsTop",
        "SubIndex",
        "ParentId",
        "Children",
        "SourceClipId",
    ];
    if let Some(object) = document.as_object() {
        for key in object.keys().filter(|key| {
            !known_document
                .iter()
                .any(|known| key.eq_ignore_ascii_case(known))
        }) {
            warnings.push(format!(
                "{scope}/{id}: archived unmodeled document field {key}"
            ));
        }
    }
    Ok(item)
}

fn document_payload(clip: &ClipRecord, raw: &Value) -> Result<Value, String> {
    let mut payload = Map::new();
    let mut consumed = vec!["ObjType", "Type"];
    let mut copy = |old: &'static str, new: &str| {
        consumed.push(old);
        if let Some(value) = field(raw, old) {
            payload.insert(new.into(), value.clone());
        }
    };
    match clip.kind {
        ClipKind::TextClipItem => copy("Text", "text"),
        ClipKind::HtmlClipItem => {
            copy("Text", "text");
            copy("HtmlData", "html");
            copy("HtmlSourceUrl", "sourceUrl");
        }
        ClipKind::QQRichTextItem => {
            copy("Text", "text");
            copy("QQRichText", "xml");
            copy("HtmlData", "html");
            copy("HtmlSourceUrl", "sourceUrl");
        }
        ClipKind::ImageClipItem => {
            copy("PerceptualHash", "perceptualHash");
            copy("ImageHash", "sourceHash");
            copy("ImageMD5", "sourceMd5");
        }
        ClipKind::FileClipItem => copy("FileList", "paths"),
        ClipKind::LinkClipItem => {
            copy("Url", "url");
            copy("Title", "sourceTitle");
            copy("Icon", "sourceIcon");
        }
        ClipKind::CmdClipItem => copy("Command", "command"),
        ClipKind::StepClipItem => {
            copy("StepData", "data");
            copy("StepSummary", "sourceSummary");
        }
        ClipKind::ActionClipItem => {
            copy("ActionItem", "data");
            copy("ExecuteParam", "executeParam");
        }
        ClipKind::FavoriteClipItem => {}
    }
    if clip.kind == ClipKind::ActionClipItem {
        if let Some(action) = field(raw, "ActionItem") {
            payload.insert(
                "data".into(),
                Value::String(serde_json::to_string(action).map_err(|e| e.to_string())?),
            );
        }
    }
    if clip.kind == ClipKind::QQRichTextItem {
        payload.insert(
            "origin".into(),
            json!(crate::im_rich::resolve_origin(
                &clip.source_process_name,
                clip.qq_rich_text.as_deref().unwrap_or("")
            )
            .as_api_token()),
        );
    }
    // Unknown type-specific payload data remains available in v2 as an opaque
    // extension, in addition to the exact source row archive.
    if let Some(object) = raw.as_object() {
        let extensions: Map<String, Value> = object
            .iter()
            .filter(|(key, _)| !consumed.iter().any(|known| key.eq_ignore_ascii_case(known)))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !extensions.is_empty() {
            payload.insert("sourceExtensions".into(), Value::Object(extensions));
        }
    }
    Ok(Value::Object(payload))
}

fn convert_native(
    row: &ArchivedRow,
    scope: &str,
    image_root: &Path,
) -> Result<ConvertedItem, String> {
    if scope == "history" {
        let record: ClipRecord = serde_json::from_str(&row.required_text("extra_json")?)
            .map_err(|error| format!("Invalid native extra_json: {error}"))?;
        if record.id != row.required_text("id")? {
            return Err("Native SQL ID differs from extra_json ID".into());
        }
        if native_kind(&row.required_text("kind")?)? != record.kind {
            return Err("Native SQL kind differs from extra_json kind".into());
        }
        for (column, expected) in [
            ("create_time", record.create_time.timestamp_millis()),
            ("update_time", record.update_time.timestamp_millis()),
        ] {
            if let Some(actual) = row.integer(column)? {
                if actual != expected {
                    return Err(format!("Native SQL {column} differs from extra_json"));
                }
            }
        }
        if row.cell("is_top").is_some() && boolean_column(row, "is_top", false)? != record.is_top {
            return Err("Native SQL is_top differs from extra_json".into());
        }
        for (column, expected) in [
            ("title", &record.title),
            ("body_text", &record.body_text),
            ("html_data", &record.html_data),
            ("image_path", &record.image_path),
        ] {
            if row.cell(column).is_some() && row.text(column)? != *expected {
                return Err(format!("Native SQL {column} differs from extra_json"));
            }
        }
        if row.cell("file_paths").is_some()
            && string_array_column(row, "file_paths")? != record.file_paths
        {
            return Err("Native SQL file_paths differs from extra_json".into());
        }
        if row.cell("source_process").is_some()
            && row.text("source_process")?.unwrap_or_default() != record.source_process_name
        {
            return Err("Native SQL source_process differs from extra_json".into());
        }
        let mut item = from_clip(&record, scope, image_root)?;
        if let Some(icon) = row.text("icon")? {
            item.icon_override = Some(icon);
        }
        return Ok(item);
    }
    let id = row.required_text("id")?;
    let kind = native_kind(&row.required_text("kind")?)?;
    let is_folder = boolean_column(row, "is_folder", false)?;
    if is_folder != (kind == ClipKind::FavoriteClipItem) {
        return Err("Native favorite kind and is_folder disagree".into());
    }
    let created = millis(
        row.integer("create_time")?
            .ok_or("Missing native create_time")?,
    )?;
    let updated = row
        .integer("update_time")?
        .map(millis)
        .transpose()?
        .unwrap_or(created);
    let favorite = FavorRecord {
        storage_revision: None,
        id: id.clone(),
        parent_id: row.text("parent_id")?.filter(|v| !v.is_empty()),
        is_folder,
        name: row.required_text("name")?,
        kind,
        body_text: row.text("body_text")?,
        file_paths: string_array_column(row, "file_paths")?,
        image_path: row.text("image_path")?,
        children: string_array_column(row, "children")?,
        create_time: created,
        update_time: updated,
        source_clip_id: row.text("source_clip_id")?,
        html_data: row.text("html_data")?,
        html_source_url: row.text("html_source_url")?,
        qq_rich_text: row.text("qq_rich_text")?,
        source_process_name: row.text("source_process")?.unwrap_or_default(),
        sub_index: i32::try_from(row.integer("sub_index")?.unwrap_or(0))
            .map_err(|_| "Native SubIndex is out of range")?,
        icon: row.text("icon")?.unwrap_or_default(),
        execute_param: row.text("execute_param")?.unwrap_or_default(),
    };
    if !favorite.is_folder && !favorite.children.is_empty() {
        return Err("Native non-folder favorite contains Children".into());
    }
    let mut record = ClipRecord::new_text("", "");
    record.id = id;
    record.kind = kind;
    record.title = Some(favorite.name.clone());
    record.body_text = favorite.body_text.clone();
    record.html_data = favorite.html_data.clone();
    record.html_source_url = favorite.html_source_url.clone();
    record.qq_rich_text = favorite.qq_rich_text.clone();
    record.file_paths = favorite.file_paths.clone();
    record.image_path = favorite.image_path.clone();
    record.create_time = created;
    record.update_time = updated;
    record.sub_index = favorite.sub_index;
    record.source_process_name = favorite.source_process_name.clone();
    record.execute_param = favorite.execute_param.clone();
    record.description = row.text("description")?.unwrap_or_default();
    record.content_hash.clear();
    let mut item = from_clip(&record, scope, image_root)?;
    item.owner_parent_id = favorite.parent_id;
    item.children = if is_folder {
        Some(favorite.children)
    } else {
        None
    };
    item.source_history_id = favorite.source_clip_id;
    item.icon_override = row.text("icon")?;
    let icon = item.icon_override.clone();
    add_icon_candidate(&mut item, "override_icon", icon.as_deref(), image_root);
    Ok(item)
}

fn from_clip(clip: &ClipRecord, scope: &str, image_root: &Path) -> Result<ConvertedItem, String> {
    if scope == "history" && clip.kind == ClipKind::FavoriteClipItem {
        return Err(
            "A folder in history has no defined v2 representation; source preserved".into(),
        );
    }
    if clip.usage_count < 0 {
        return Err("Negative source usage count".into());
    }
    let kind = match clip.kind {
        ClipKind::TextClipItem => "text",
        ClipKind::HtmlClipItem => "html",
        ClipKind::QQRichTextItem => "im_rich",
        ClipKind::ImageClipItem => "image",
        ClipKind::FileClipItem => "files",
        ClipKind::LinkClipItem => "link",
        ClipKind::CmdClipItem => "command",
        ClipKind::StepClipItem => "steps",
        ClipKind::ActionClipItem => "action",
        ClipKind::FavoriteClipItem => "folder",
    };
    let payload = match clip.kind {
        ClipKind::TextClipItem => json!({"text":clip.body_text}),
        ClipKind::HtmlClipItem => {
            json!({"text":clip.body_text,"html":clip.html_data,"sourceUrl":clip.html_source_url})
        }
        ClipKind::QQRichTextItem => {
            json!({"origin":crate::im_rich::resolve_origin(&clip.source_process_name, clip.qq_rich_text.as_deref().unwrap_or("")).as_api_token(),"text":clip.body_text,"xml":clip.qq_rich_text,"html":clip.html_data,"sourceUrl":clip.html_source_url})
        }
        ClipKind::ImageClipItem => json!({"perceptualHash":clip.perceptual_hash}),
        ClipKind::FileClipItem => json!({"paths":clip.file_paths}),
        ClipKind::LinkClipItem => json!({"url":clip.body_text}),
        ClipKind::CmdClipItem => json!({"command":clip.body_text}),
        ClipKind::StepClipItem => json!({"data":clip.body_text}),
        ClipKind::ActionClipItem => {
            json!({"data":clip.body_text,"executeParam":clip.execute_param})
        }
        ClipKind::FavoriteClipItem => json!({}),
    };
    let mut item = ConvertedItem {
        scope: scope.into(),
        id: clip.id.clone(),
        kind: kind.into(),
        title: clip.title.clone(),
        icon_override: None,
        description: clip.description.clone(),
        payload_json: payload,
        search_text: [
            clip.title.as_deref().unwrap_or(""),
            clip.body_text.as_deref().unwrap_or(""),
            &clip.description,
        ]
        .join("\n"),
        dedupe_key: core_safe_distinct_key(clip),
        source_process: clip.source_process_name.clone(),
        created_at: canonical_time(clip.create_time)?,
        updated_at: canonical_time(clip.update_time)?,
        sub_index: clip.sub_index,
        is_pinned: scope == "history" && clip.is_top,
        usage_count: i64::from(clip.usage_count),
        last_used_at: clip.last_access.map(canonical_time).transpose()?,
        owner_parent_id: None,
        children: if clip.kind == ClipKind::FavoriteClipItem {
            Some(Vec::new())
        } else {
            None
        },
        source_history_id: None,
        asset_candidates: Vec::new(),
    };
    if clip.kind == ClipKind::ImageClipItem {
        let original = clip
            .image_path
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or("Image item has no source locator")?;
        item.asset_candidates.push(AssetCandidate {
            slot: "image".into(),
            ordinal: 0,
            original_locator: original.into(),
            path: resolve_path(original, image_root),
        });
    }
    Ok(item)
}

fn core_safe_distinct_key(clip: &ClipRecord) -> String {
    if !PURE_CORE_READER.with(std::cell::Cell::get) { return clip.distinct_key(); }
    let lexical = |path: &str| path.trim().replace('/', "\\").trim_end_matches('\\').to_owned();
    match clip.kind {
        ClipKind::FileClipItem => {
            let mut paths: Vec<_> = clip.file_paths.iter().filter(|p| !p.trim().is_empty()).map(|p| lexical(p)).collect();
            paths.sort_by_key(|p| p.to_ascii_lowercase());
            format!("file:FileClipItem:{}", paths.join("|"))
        }
        ClipKind::ImageClipItem if clip.perceptual_hash.trim().is_empty() && clip.content_hash.is_empty() => {
            format!("image:path:{}", lexical(clip.image_path.as_deref().unwrap_or("")))
        }
        _ => clip.distinct_key(),
    }
}

fn merge_usage(
    connection: &Connection,
    tables: &[String],
    source: &mut ConvertedSource,
) -> Result<(), String> {
    let table = match source.format {
        SourceFormat::Documents => "UsageInfo",
        SourceFormat::Native => "usage_info",
    };
    let Some(table) = tables.iter().find(|name| name.eq_ignore_ascii_case(table)) else {
        return Ok(());
    };
    let index: HashMap<(String, String), usize> = source
        .items
        .iter()
        .enumerate()
        .map(|(n, item)| ((item.scope.clone(), item.id.clone()), n))
        .collect();
    let mut seen = HashSet::new();
    for row in read_rows(connection, table)? {
        let id = row.required_text("id")?;
        if !seen.insert(id.clone()) {
            return Err("Duplicate source usage ID".into());
        }
        let (count, time) = match source.format {
            SourceFormat::Documents => {
                let document: Value = serde_json::from_str(&row.required_text("Data")?)
                    .map_err(|e| format!("Invalid UsageInfo JSON: {e}"))?;
                let info = crate::csharp_store_document::parse_usage(&id, &document)?;
                if let Some(time) = info.last_use_time {
                    note_local_time(
                        &mut source.warnings,
                        "usage",
                        &id,
                        "LastUseTime",
                        field(&document, "LastUseTime"),
                        time,
                    );
                }
                (
                    i64::from(info.usage_count),
                    info.last_use_time.map(canonical_time).transpose()?,
                )
            }
            SourceFormat::Native => {
                let count = row
                    .integer("usage_count")?
                    .ok_or("Native usage count missing")?;
                if count < 0 {
                    return Err("Negative native usage count".into());
                }
                let ms = row
                    .integer("last_use_time")?
                    .ok_or("Native usage time missing")?;
                (
                    count,
                    if ms == 0 && count == 0 {
                        None
                    } else {
                        Some(canonical_time(millis(ms)?)?)
                    },
                )
            }
        };
        let key = id
            .strip_prefix("clip_item_")
            .map(|id| ("history", id))
            .or_else(|| id.strip_prefix("favor_item_").map(|id| ("favorite", id)));
        if let Some(&n) = key.and_then(|(scope, id)| index.get(&(scope.into(), id.into()))) {
            source.items[n].usage_count = count;
            source.items[n].last_used_at = time;
        } else {
            source.orphan_usage.push(id);
        }
        source.archive.push(row);
    }
    Ok(())
}

pub fn canonical_time(time: DateTime<Utc>) -> Result<String, String> {
    if !(0..=9999).contains(&time.year()) {
        return Err("Source timestamp is outside the supported four-digit UTC year range".into());
    }
    let value = time.to_rfc3339_opts(SecondsFormat::Nanos, true);
    if value.len() != 30 {
        return Err("Source timestamp cannot be represented losslessly in v2".into());
    }
    Ok(value)
}

fn table_names(connection: &Connection) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare("SELECT name FROM main.sqlite_master WHERE type='table' ORDER BY name")
        .map_err(sql_error)?;
    let result = statement
        .query_map([], |row| row.get(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error);
    result
}
fn columns(connection: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(sql_error)?;
    let result = statement
        .query_map([table], |row| row.get(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error);
    result
}
pub(crate) fn read_rows(connection: &Connection, table: &str) -> Result<Vec<ArchivedRow>, String> {
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE)", [table], |r| r.get(0)).map_err(sql_error)?;
    if !exists {
        return Err(format!("Source {table} is not a real table"));
    }
    let sql = format!("SELECT _rowid_,* FROM {} ORDER BY _rowid_", quote(table));
    let (mut statement, has_rowid) = match connection.prepare(&sql) {
        Ok(statement) => (statement, true),
        Err(_) => (
            connection
                .prepare(&format!("SELECT * FROM {} ORDER BY Id", quote(table)))
                .map_err(sql_error)?,
            false,
        ),
    };
    let names = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let mut cursor = statement.query([]).map_err(sql_error)?;
    let mut output = Vec::new();
    while let Some(row) = cursor.next().map_err(sql_error)? {
        let rowid = if has_rowid {
            Some(row.get::<_, i64>(0).map_err(sql_error)?)
        } else {
            None
        };
        let mut cells = BTreeMap::new();
        for (column, name) in names.iter().enumerate().skip(usize::from(has_rowid)) {
            let value = match row.get_ref(column).map_err(sql_error)? {
                ValueRef::Null => ArchivedCell::Null,
                ValueRef::Integer(value) => ArchivedCell::Integer(value),
                ValueRef::Real(value) => {
                    ArchivedCell::RealBits(format!("{:016x}", value.to_bits()))
                }
                ValueRef::Text(value) => ArchivedCell::Text(
                    std::str::from_utf8(value)
                        .map_err(|_| format!("Source {table}.{name} is not valid UTF-8"))?
                        .into(),
                ),
                ValueRef::Blob(value) => ArchivedCell::Blob(value.to_vec()),
            };
            cells.insert(name.clone(), value);
        }
        output.push(ArchivedRow {
            table: table.into(),
            rowid,
            cells,
        });
    }
    Ok(output)
}
fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
fn sql_error(error: rusqlite::Error) -> String {
    format!("SQLite source read: {error}")
}
fn note_local_time(
    warnings: &mut Vec<String>,
    scope: &str,
    id: &str,
    field_name: &str,
    raw: Option<&Value>,
    converted: DateTime<Utc>,
) {
    if raw
        .and_then(Value::as_str)
        .is_some_and(|text| DateTime::parse_from_rfc3339(text).is_err())
    {
        let offset = converted.with_timezone(&Local).offset().local_minus_utc();
        warnings.push(format!("{scope}/{id}: offset-free {field_name} interpreted using source-host local UTC offset {offset} seconds; confirm this matches the original data machine"));
    }
}
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value.get(name).or_else(|| {
        value
            .as_object()?
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    })
}
fn optional_text(value: &Value, name: &str) -> Result<Option<String>, String> {
    match field(value, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(format!("{name} must be a string or null")),
    }
}
fn millis(value: i64) -> Result<DateTime<Utc>, String> {
    Utc.timestamp_millis_opt(value)
        .single()
        .ok_or_else(|| "Native timestamp is out of range".into())
}
fn boolean_column(row: &ArchivedRow, name: &str, default: bool) -> Result<bool, String> {
    match row.integer(name)? {
        None => Ok(default),
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!("Native {name} must be 0 or 1")),
    }
}
fn string_array_column(row: &ArchivedRow, name: &str) -> Result<Vec<String>, String> {
    let value = row.required_text(name)?;
    serde_json::from_str(&value)
        .map_err(|error| format!("Native {name} is not a string array: {error}"))
}
fn native_kind(value: &str) -> Result<ClipKind, String> {
    match value {
        "TextClipItem" | "text" => Ok(ClipKind::TextClipItem),
        "HtmlClipItem" | "html" => Ok(ClipKind::HtmlClipItem),
        "QQRichTextItem" => Ok(ClipKind::QQRichTextItem),
        "ImageClipItem" | "image" => Ok(ClipKind::ImageClipItem),
        "FileClipItem" | "file" => Ok(ClipKind::FileClipItem),
        "LinkClipItem" => Ok(ClipKind::LinkClipItem),
        "CmdClipItem" => Ok(ClipKind::CmdClipItem),
        "StepClipItem" | "step" | "other" => Ok(ClipKind::StepClipItem),
        "ActionClipItem" => Ok(ClipKind::ActionClipItem),
        "FavoriteClipItem" => Ok(ClipKind::FavoriteClipItem),
        _ => Err(format!("Unknown native kind {value}")),
    }
}
fn resolve_path(locator: &str, image_root: &Path) -> PathBuf {
    if locator.to_ascii_lowercase().starts_with("file:") {
        if let Some(path) = crate::explorer::file_url_to_local_path(locator) {
            return PathBuf::from(path);
        }
    }
    let path = Path::new(locator);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        image_root.join(path)
    }
}
fn add_icon_candidate(
    item: &mut ConvertedItem,
    slot: &str,
    locator: Option<&str>,
    image_root: &Path,
) {
    let Some(locator) = locator.filter(|value| !value.is_empty()) else {
        return;
    };
    let lower = locator.to_ascii_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("data:") {
        return;
    }
    let path = Path::new(locator);
    let image_extension = path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|ext| {
            ["png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp"]
                .iter()
                .any(|v| ext.eq_ignore_ascii_case(v))
        });
    if path.is_absolute() || lower.starts_with("file:") || image_extension {
        item.asset_candidates.push(AssetCandidate {
            slot: slot.into(),
            ordinal: 0,
            original_locator: locator.into(),
            path: resolve_path(locator, image_root),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    const DATE: &str = "2026-10-17T03:12:34.123456700Z";

    fn documents() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT,Future BLOB);
            CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT);
            CREATE TABLE UsageInfo(Id TEXT PRIMARY KEY,Data TEXT);",
        )
        .unwrap();
        db
    }
    fn document(id: &str, payload: Value) -> Value {
        json!({"Id":id,"Title":null,"Icon":null,"CreateTime":DATE,"UpdateTime":DATE,
            "SourceProcessName":"fixture","Item":payload})
    }
    fn insert(db: &Connection, table: &str, id: &str, value: Value) {
        db.execute(
            &format!("INSERT INTO {}(Id,Data) VALUES(?1,?2)", quote(table)),
            params![id, value.to_string()],
        )
        .unwrap();
    }
    fn read(db: &Connection) -> ConvertedSource {
        read_source(db, Path::new("fixture/Image")).unwrap()
    }
    fn favorite(db: &Connection, id: &str, parent: Option<&str>, folder: bool, children: &[&str]) {
        let mut value = document(
            id,
            if folder {
                json!({"ObjType":100})
            } else {
                json!({"ObjType":1,"Text":id})
            },
        );
        value["Title"] = json!(id);
        value["ParentId"] = json!(parent);
        value["Children"] = json!(children);
        insert(db, "favor_item", id, value);
    }
    fn native() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE clip_item(id TEXT PRIMARY KEY,kind TEXT,extra_json TEXT,create_time INTEGER,update_time INTEGER,is_top INTEGER);
            CREATE TABLE favor_item(id TEXT PRIMARY KEY,kind TEXT,is_folder INTEGER,name TEXT,parent_id TEXT,body_text TEXT,file_paths TEXT,image_path TEXT,children TEXT,create_time INTEGER,update_time INTEGER);
            CREATE TABLE usage_info(id TEXT PRIMARY KEY,last_use_time INTEGER,usage_count INTEGER);").unwrap();
        db
    }
    fn native_clip(db: &Connection, record: &ClipRecord) {
        db.execute(
            "INSERT INTO clip_item VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                record.id,
                record.kind.as_str(),
                serde_json::to_string(record).unwrap(),
                record.create_time.timestamp_millis(),
                record.update_time.timestamp_millis(),
                record.is_top as i32
            ],
        )
        .unwrap();
    }

    #[test]
    fn document_link_keeps_override_and_source_metadata_separate() {
        let db = documents();
        let mut value = document(
            "same",
            json!({"ObjType":20,"Url":"https://example.test/","Title":"source title","Icon":"source.png","Future":{"enabled":true}}),
        );
        value["Title"] = json!("");
        value["Icon"] = json!("override.png");
        insert(&db, "favor_item", "same", value);
        let source = read(&db);
        let item = &source.items[0];
        assert_eq!(item.title.as_deref(), Some(""));
        assert_eq!(item.icon_override.as_deref(), Some("override.png"));
        assert_eq!(item.payload_json["sourceTitle"], "source title");
        assert_eq!(item.payload_json["sourceIcon"], "source.png");
        assert_eq!(
            item.payload_json["sourceExtensions"]["Future"]["enabled"],
            true
        );
        assert_eq!(
            item.asset_candidates
                .iter()
                .map(|v| v.slot.as_str())
                .collect::<Vec<_>>(),
            vec!["override_icon", "content_icon"]
        );
    }

    #[test]
    fn document_exact_keys_match_legacy_reader_and_keep_case_sensitive_extensions() {
        let db = documents();
        let mut value = document(
            "case-priority",
            json!({
                "ObjType": 1,
                "Text": "canonical original\r\n",
                "TEXT": "wrong uppercase payload",
                "Future": {"x": 1, "X": 2}
            }),
        );
        value["Title"] = json!("canonical title");
        value["TITLE"] = json!("wrong uppercase title");
        value["ITEM"] = json!({"ObjType": 1, "Text": "wrong uppercase document item"});
        let legacy = crate::csharp_store_document::parse_clip("case-priority", &value).unwrap();
        insert(&db, "clip_item", "case-priority", value);
        let source = read(&db);
        let item = &source.items[0];
        assert_eq!(item.title, legacy.title);
        assert_eq!(item.title.as_deref(), Some("canonical title"));
        assert_eq!(
            item.payload_json["text"].as_str(),
            legacy.body_text.as_deref()
        );
        assert_eq!(item.payload_json["text"], "canonical original\r\n");
        assert_eq!(
            item.search_text,
            [
                legacy.title.as_deref().unwrap_or(""),
                legacy.body_text.as_deref().unwrap_or(""),
                &legacy.description,
            ]
            .join("\n")
        );
        assert_eq!(item.dedupe_key, legacy.distinct_key());
        assert!(!item.search_text.contains("wrong uppercase"));
        assert_eq!(item.payload_json["sourceExtensions"]["Future"]["x"], 1);
        assert_eq!(item.payload_json["sourceExtensions"]["Future"]["X"], 2);
    }

    #[test]
    fn document_html_and_explicit_empty_source_are_byte_preserved() {
        let db = documents();
        let original = "Version:1.0\r\nSourceURL:https://source.test/\r\n<html><head><!--原始--></head><body>\r\n<img src='Temp/a.png'>\r\n</body></html>";
        insert(
            &db,
            "clip_item",
            "html",
            document(
                "html",
                json!({"ObjType":10,"Text":"原文\r\n","HtmlData":original,"HtmlSourceUrl":""}),
            ),
        );
        let source = read(&db);
        let item = &source.items[0];
        assert_eq!(
            item.payload_json["html"].as_str().unwrap().as_bytes(),
            original.as_bytes()
        );
        assert_eq!(item.payload_json["sourceUrl"], "");
        assert_eq!(item.payload_json["text"], "原文\r\n");
        assert_eq!(item.created_at, DATE);
        assert_eq!(item.title, None);
    }

    #[test]
    fn document_im_keeps_xml_html_null_and_unknown_fields() {
        let db = documents();
        let xml="<QQRichEditFormat>\r\n<EditElement type='3' filepath='Temp/a.png'/></QQRichEditFormat>";
        insert(
            &db,
            "clip_item",
            "im",
            document(
                "im",
                json!({"ObjType":11,"Text":"\r\n","QQRichText":xml,"HtmlData":null,"HtmlSourceUrl":null,"FutureFlag":17}),
            ),
        );
        let source = read(&db);
        assert_eq!(source.items[0].payload_json["xml"], xml);
        assert!(source.items[0].payload_json.get("html").unwrap().is_null());
        assert_eq!(
            source.items[0].payload_json["sourceExtensions"]["FutureFlag"],
            17
        );
    }

    #[test]
    fn source_archive_keeps_exact_json_and_unknown_sql_cells() {
        let db = documents();
        let raw=format!("{{ \"Id\":\"raw\", \"CreateTime\":\"{DATE}\", \"Item\":{{\"ObjType\":1,\"Text\":\"same\\r\\n\"}}, \"FutureDocument\": {{\"x\":1}} }}");
        db.execute(
            "INSERT INTO clip_item(Id,Data,Future) VALUES('raw',?1,?2)",
            params![raw, vec![0_u8, 255, 1]],
        )
        .unwrap();
        let source = read(&db);
        assert!(
            matches!(source.archive[0].cell("Data"),Some(ArchivedCell::Text(value)) if value==&raw)
        );
        assert!(
            matches!(source.archive[0].cell("Future"),Some(ArchivedCell::Blob(value)) if value==&[0,255,1])
        );
        assert!(source.warnings.iter().any(|v| v.contains("FutureDocument")));
        let serialized = serde_json::to_string(&source.archive).unwrap();
        let recovered: Vec<ArchivedRow> = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(recovered[0].cell("Data"),Some(ArchivedCell::Text(value)) if value==&raw));
    }

    #[test]
    fn usage_merges_scoped_ids_and_archives_orphans() {
        let db = documents();
        insert(
            &db,
            "clip_item",
            "same",
            document("same", json!({"ObjType":1,"Text":"history"})),
        );
        favorite(&db, "same", None, false, &[]);
        for (id, count) in [
            ("clip_item_same", 3),
            ("favor_item_same", 7),
            ("clip_item_gone", 11),
        ] {
            insert(
                &db,
                "UsageInfo",
                id,
                json!({"Id":id,"UsageCount":count,"LastUseTime":DATE}),
            );
        }
        let source = read(&db);
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.scope == "history")
                .unwrap()
                .usage_count,
            3
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.scope == "favorite")
                .unwrap()
                .usage_count,
            7
        );
        assert_eq!(source.orphan_usage, vec!["clip_item_gone"]);
        assert_eq!(source.archive.len(), 5);
    }

    #[test]
    fn native_history_uses_full_precision_json_and_usage_overlay() {
        let db = native();
        let mut item = ClipRecord::new_text("native\r\n", "");
        item.id = "native".into();
        item.create_time = DateTime::parse_from_rfc3339("2026-10-17T03:12:34.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        item.update_time = item.create_time;
        item.usage_count = 19;
        native_clip(&db, &item);
        db.execute("INSERT INTO usage_info VALUES('clip_item_native',0,0)", [])
            .unwrap();
        let source = read(&db);
        let result = &source.items[0];
        assert_eq!(source.format, SourceFormat::Native);
        assert_eq!(result.created_at, "2026-10-17T03:12:34.123456789Z");
        assert_eq!(result.payload_json["text"], "native\r\n");
        assert_eq!(result.usage_count, 0);
        assert_eq!(result.last_used_at, None);
    }

    #[test]
    fn native_inconsistent_identity_or_time_is_rejected() {
        let db = native();
        let mut item = ClipRecord::new_text("native", "");
        item.id = "native".into();
        native_clip(&db, &item);
        db.execute("UPDATE clip_item SET create_time=create_time+1", [])
            .unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("differs from extra_json"));
        db.execute(
            "UPDATE clip_item SET create_time=?1,id='other'",
            [item.create_time.timestamp_millis()],
        )
        .unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("SQL ID"));
    }

    #[test]
    fn native_bad_children_does_not_silently_become_empty() {
        let db = native();
        db.execute("INSERT INTO favor_item(id,kind,is_folder,name,file_paths,children,create_time) VALUES('f','FavoriteClipItem',1,'f','[]','not-json',0)",[]).unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("string array"));
    }

    #[test]
    fn ancient_empty_unknown_mixed_and_v2_sources_are_rejected() {
        let db = Connection::open_in_memory().unwrap();
        assert!(read_source(&db, Path::new("Image")).is_err());
        db.execute_batch("CREATE TABLE ClipboardItem(Id INTEGER,Item TEXT); INSERT INTO ClipboardItem VALUES(1,'old');").unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("No supported"));
        db.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT); CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT);").unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("ambiguous"));
        db.execute_batch("DROP TABLE ClipboardItem; DROP TABLE favor_item; CREATE TABLE favor_item(id TEXT PRIMARY KEY,kind TEXT);").unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("Mixed"));
        db.execute_batch(
            "CREATE TABLE store_info(format_version INTEGER); INSERT INTO store_info VALUES(3);",
        )
        .unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("store_info"));
    }

    #[test]
    fn missing_lazy_document_table_is_supported_unless_ancient_rows_exist() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT);")
            .unwrap();
        insert(
            &db,
            "clip_item",
            "one",
            document("one", json!({"ObjType":1,"Text":"one"})),
        );
        assert_eq!(read(&db).items.len(), 1);
        db.execute_batch("CREATE TABLE FavorItem(Id INTEGER); INSERT INTO FavorItem VALUES(1);")
            .unwrap();
        assert!(read_source(&db, Path::new("Image"))
            .unwrap_err()
            .contains("ambiguous"));
    }

    #[test]
    fn conversion_works_with_query_only_and_never_writes() {
        let db = documents();
        favorite(&db, "folder", None, true, &[]);
        let before = db.total_changes();
        let schema: i64 = db
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        db.execute_batch("PRAGMA query_only=ON;").unwrap();
        let _ = read(&db);
        assert_eq!(db.total_changes(), before);
        assert_eq!(
            db.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            schema
        );
        assert!(db.is_autocommit());
    }

    #[test]
    fn graph_keeps_shared_members_and_appends_parent_only_edges_in_current_order() {
        let db = documents();
        favorite(&db, "A", None, true, &["X"]);
        favorite(&db, "B", None, true, &["X"]);
        favorite(&db, "X", Some("A"), false, &[]);
        favorite(&db, "Z", Some("A"), false, &[]);
        favorite(&db, "Y", Some("A"), false, &[]);
        let mut source = read(&db);
        let roots =
            normalize_favorites(&mut source, &["B".into(), "A".into()], "system-root").unwrap();
        assert_eq!(roots, vec!["B", "A"]);
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "A")
                .unwrap()
                .children
                .as_ref()
                .unwrap(),
            &vec!["X", "Y", "Z"]
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "B")
                .unwrap()
                .children
                .as_ref()
                .unwrap(),
            &vec!["X"]
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "X")
                .unwrap()
                .owner_parent_id
                .as_deref(),
            Some("A")
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "A")
                .unwrap()
                .owner_parent_id
                .as_deref(),
            Some("system-root")
        );
    }

    #[test]
    fn graph_errors_preserve_input_and_report_missing_duplicates_cycles_and_owner() {
        for case in ["missing", "duplicate", "cycle", "owner"] {
            let db = documents();
            match case {
                "missing" => favorite(&db, "A", None, true, &["gone"]),
                "duplicate" => {
                    favorite(&db, "A", None, true, &["X", "X"]);
                    favorite(&db, "X", Some("A"), false, &[]);
                }
                "cycle" => {
                    favorite(&db, "A", None, true, &["B"]);
                    favorite(&db, "B", None, true, &["A"]);
                }
                _ => favorite(&db, "X", Some("gone"), false, &[]),
            }
            let mut source = read(&db);
            let before = serde_json::to_string(&source).unwrap();
            assert!(
                normalize_favorites(&mut source, &[], "system-root").is_err(),
                "{case}"
            );
            assert_eq!(serde_json::to_string(&source).unwrap(), before, "{case}");
        }
    }

    #[test]
    fn graph_validates_root_references_and_collision() {
        let db = documents();
        favorite(&db, "A", None, true, &[]);
        let mut source = read(&db);
        assert!(normalize_favorites(&mut source, &[], "A").is_err());
        assert!(normalize_favorites(&mut source, &["gone".into()], "root").is_err());
        assert!(normalize_favorites(&mut source, &["A".into(), "A".into()], "root").is_err());
    }

    #[test]
    fn same_named_missing_root_preferences_keep_existing_stable_order() {
        let db = documents();
        // ID ordering is deliberately opposite insertion order, while both
        // dates share the same projected millisecond in the current UI.
        for (id, time) in [
            ("Z", "2026-10-17T03:12:34.123456789Z"),
            ("A", "2026-10-17T03:12:34.123456700Z"),
        ] {
            let mut value = document(id, json!({"ObjType":100}));
            value["Title"] = json!("same");
            value["Children"] = json!([]);
            value["CreateTime"] = json!(time);
            insert(&db, "favor_item", id, value);
        }
        let mut source = read(&db);
        assert_eq!(
            normalize_favorites(&mut source, &[], "root").unwrap(),
            vec!["Z", "A"]
        );
    }

    #[test]
    fn image_candidates_keep_missing_locator_without_reading_the_filesystem() {
        let db = documents();
        insert(
            &db,
            "clip_item",
            "image",
            document(
                "image",
                json!({"ObjType":3,"RSource":{"IsInImageDir":true,"RPath":"Temp/missing.png"},"ImageMD5":"legacy"}),
            ),
        );
        let source = read(&db);
        let candidate = &source.items[0].asset_candidates[0];
        assert_eq!(candidate.original_locator, "Temp/missing.png");
        assert_eq!(
            candidate.path,
            Path::new("fixture/Image").join("Temp/missing.png")
        );
        assert_eq!(source.items[0].payload_json["sourceMd5"], "legacy");
    }

    #[test]
    fn malformed_source_json_identity_or_usage_blocks_conversion() {
        let db = documents();
        db.execute("INSERT INTO clip_item(Id,Data) VALUES('bad','{')", [])
            .unwrap();
        assert!(read_source(&db, Path::new("Image")).is_err());
        db.execute("DELETE FROM clip_item", []).unwrap();
        insert(
            &db,
            "clip_item",
            "bad",
            document("wrong", json!({"ObjType":1,"Text":"x"})),
        );
        assert!(read_source(&db, Path::new("Image")).is_err());
        db.execute("DELETE FROM clip_item", []).unwrap();
        insert(
            &db,
            "UsageInfo",
            "clip_item_bad",
            json!({"Id":"clip_item_bad","UsageCount":-1,"LastUseTime":DATE}),
        );
        assert!(read_source(&db, Path::new("Image")).is_err());
    }

    #[test]
    fn every_supported_content_type_is_converted_without_text_normalization() {
        let db = documents();
        for (id, payload) in [
            ("t", json!({"ObjType":1,"Text":" \r\n"})),
            ("f", json!({"ObjType":2,"FileList":["D:\\external.txt"]})),
            ("c", json!({"ObjType":21,"Command":"echo 原文\r\n"})),
            ("s", json!({"ObjType":64,"StepData":"{ raw steps }\r\n"})),
        ] {
            insert(&db, "clip_item", id, document(id, payload));
        }
        let source = read(&db);
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "t")
                .unwrap()
                .payload_json["text"],
            " \r\n"
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "f")
                .unwrap()
                .payload_json["paths"],
            json!(["D:\\external.txt"])
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "c")
                .unwrap()
                .payload_json["command"],
            "echo 原文\r\n"
        );
        assert_eq!(
            source
                .items
                .iter()
                .find(|v| v.id == "s")
                .unwrap()
                .payload_json["data"],
            "{ raw steps }\r\n"
        );
    }

    #[test]
    fn document_action_retains_nested_original_fields_and_execute_parameter() {
        let db = documents();
        let action = json!({"Id":"action","Title":"source action","SharedActionId":"shared","FutureAction":{"n":19}});
        insert(
            &db,
            "favor_item",
            "a",
            document(
                "a",
                json!({"ObjType":65,"ActionItem":action,"ExecuteParam":"原始参数\r\n"}),
            ),
        );
        let source = read(&db);
        let item = &source.items[0];
        assert_eq!(item.kind, "action");
        assert_eq!(
            serde_json::from_str::<Value>(item.payload_json["data"].as_str().unwrap()).unwrap(),
            action
        );
        assert_eq!(item.payload_json["executeParam"], "原始参数\r\n");
    }

    #[test]
    fn native_favorite_preserves_content_fields_and_its_separate_identity() {
        let db = native();
        db.execute_batch(
            "ALTER TABLE favor_item ADD COLUMN html_data TEXT;
            ALTER TABLE favor_item ADD COLUMN html_source_url TEXT;
            ALTER TABLE favor_item ADD COLUMN qq_rich_text TEXT;
            ALTER TABLE favor_item ADD COLUMN icon TEXT;
            ALTER TABLE favor_item ADD COLUMN source_clip_id TEXT;",
        )
        .unwrap();
        let mut history = ClipRecord::new_text("history", "");
        history.id = "same".into();
        native_clip(&db, &history);
        db.execute("INSERT INTO favor_item(id,kind,is_folder,name,body_text,file_paths,children,create_time,html_data,html_source_url,qq_rich_text,icon,source_clip_id)
            VALUES('same','HtmlClipItem',0,'favorite','favorite text','[]','[]',0,?1,'',NULL,'','same')",["<html>original\r\n</html>"]).unwrap();
        let source = read(&db);
        let item = source.items.iter().find(|v| v.scope == "favorite").unwrap();
        assert_eq!(source.items.len(), 2);
        assert_eq!(item.payload_json["html"], "<html>original\r\n</html>");
        assert_eq!(item.payload_json["sourceUrl"], "");
        assert_eq!(item.icon_override.as_deref(), Some(""));
        assert_eq!(item.source_history_id.as_deref(), Some("same"));
        assert_eq!(item.updated_at, item.created_at);
    }

    #[test]
    fn offset_free_source_time_is_explicitly_reported() {
        let db = documents();
        let mut value = document("local", json!({"ObjType":1,"Text":"local"}));
        value["CreateTime"] = json!("2026-10-17T12:12:34.1234567");
        insert(&db, "clip_item", "local", value);
        let source = read(&db);
        assert!(source
            .warnings
            .iter()
            .any(|v| v.contains("offset-free CreateTime") && v.contains("UTC offset")));
        assert_eq!(source.items[0].created_at.len(), 30);
    }

    #[test]
    fn canonical_time_keeps_nanoseconds_minimum_year_and_sort_order() {
        let a = DateTime::parse_from_rfc3339("0001-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(canonical_time(a).unwrap(), "0001-01-01T00:00:00.000000000Z");
        let a = DateTime::parse_from_rfc3339("2026-10-17T03:12:34.123456700Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = DateTime::parse_from_rfc3339("2026-10-17T03:12:34.123456701Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(canonical_time(a).unwrap() < canonical_time(b).unwrap());
        assert!(canonical_time(Utc.with_ymd_and_hms(10000, 1, 1, 0, 0, 0).unwrap()).is_err());
    }
}
