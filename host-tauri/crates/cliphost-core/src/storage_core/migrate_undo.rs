//! Saved undo adaptation is deliberately separate from document conversion.
//! Only a proven effective old undo can receive a new core concurrency token.
use super::*;
use crate::model::{ClipKind, FavorRecord};
use rusqlite::OptionalExtension;

const STATE_KEY: &str = "favorite-drag.state.v1";
const UNSUPPORTED: &str = "CORE_UNDO_REQUIRES_LOSSLESS_CONVERTER";

/// `runtime` projects the complete staged core rows. Its paths are rebound
/// lexically to the eventual candidate root before calculating the new token;
/// the destination need not exist and is never probed by this adapter.
pub(super) fn migrate(
    source: &Connection,
    candidate: &Connection,
    runtime: &crate::storage_core::Runtime,
    converted: &convert::ConvertedSource,
    roots: &[String],
    aliases: &[PathBuf],
    final_root: &Path,
    settings: &MigrationSettings,
) -> Result<&'static str, String> {
    let original: Option<String> = candidate
        .query_row("SELECT value FROM kv WHERE key=?1", [STATE_KEY], |r| r.get(0))
        .optional().map_err(err)?;
    let Some(original) = original else { return Ok("none"); };
    let state = RawFields::parse(&original)?;
    let Some(raw_undo) = state.get("undo")?.filter(|v| *v != "null") else { return Ok("none"); };
    let undo = RawFields::parse(raw_undo)?;
    let after: String = read(&undo, "after_revision")?;
    // A future saved snapshot could attach restoration semantics to fields the
    // current FavorRecord does not understand. Reject them before serde can
    // silently discard them while rebuilding the current complete projection.
    let before_tokens: Vec<Box<RawValue>> = read(&undo, "before")?;
    const BEFORE_FIELDS: &[&str] = &["id","parentId","isFolder","name","kind","bodyText","filePaths","imagePath","children","createTime","updateTime","sourceClipId","htmlData","htmlSourceUrl","qqRichText","sourceProcessName","subIndex","icon","executeParam"];
    for token in &before_tokens {
        let fields=RawFields::parse(token.get())?;
        for name in BEFORE_FIELDS { fields.get(name)?; }
        if fields.0.iter().any(|(name,_)|!BEFORE_FIELDS.iter().any(|known|name.as_str()==*known)) {
            return Err(format!("{UNSUPPORTED}: unknown saved-before field requires an explicit undo adapter"));
        }
    }
    let before: Vec<FavorRecord> = read(&undo, "before")?;
    let created: Vec<String> = read(&undo, "created")?;
    let undo_roots: Vec<String> = read(&undo, "roots")?;
    let id: String = read(&undo, "id")?;
    if id.is_empty() || after.is_empty() { return Err(format!("{UNSUPPORTED}: incomplete saved undo identity")); }
    // All old fields remain raw in the sealed source table and encrypted full
    // snapshot. Unknown state/undo/receipt fields stay raw in the active key.
    let legacy = legacy_favorites(source, converted.format, settings)?;
    let contains_relative_image = legacy.iter().any(|f| f.image_path.as_deref().is_some_and(|p| !Path::new(p).is_absolute()));
    let mut effective = None;
    let mut roots_to_try = vec![PathBuf::new()];
    for alias in aliases {
        for spelling in [alias.to_string_lossy().into_owned(), alias.to_string_lossy().replace('/', "\\")] {
            let root = PathBuf::from(spelling);
            if !roots_to_try.contains(&root) { roots_to_try.push(root); }
        }
    }
    for root in roots_to_try {
        if contains_relative_image && root.as_os_str().is_empty() { continue; }
        let mut projected = legacy.clone();
        for item in &mut projected {
            if let Some(path) = item.image_path.as_mut().filter(|p| !Path::new(p).is_absolute()) {
                *path = root.join("Image").join(&*path).to_string_lossy().into_owned();
            }
        }
        if crate::host::storage_core_legacy_favorite_revision(&projected, roots)? == after {
            effective = Some(projected); break;
        }
    }
    let Some(legacy) = effective else {
        // For relative images, an omitted historical path spelling might be
        // responsible. Do not declare a potentially valid undo obsolete.
        if contains_relative_image { return Err(format!("{UNSUPPORTED}: old image-root revision could not be proven")); }
        // No old field depends on a root. The old algorithm proves this undo
        // already conflicted. Keep its original bytes and never reauthorize it.
        return Ok("obsolete_preserved");
    };
    let current: BTreeMap<_, _> = legacy.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut touched = BTreeSet::new();
    for old in &before {
        if !touched.insert(old.id.as_str()) { return Err(format!("{UNSUPPORTED}: duplicate before row")); }
        let now = current.get(old.id.as_str()).ok_or_else(|| format!("{UNSUPPORTED}: before row is absent"))?;
        // A drag changes ParentId/Children/SubIndex. Every remaining visible
        // old field must agree; otherwise a full content restore is involved.
        if nonstructural(old)? != nonstructural(now)? { return Err(format!("{UNSUPPORTED}: saved before row changes non-structural content")); }
    }
    let mut removed = BTreeSet::new();
    for key in &created {
        if !removed.insert(key.as_str()) || !current.contains_key(key.as_str()) || touched.contains(key.as_str()) {
            return Err(format!("{UNSUPPORTED}: ambiguous created rows"));
        }
    }
    // Validate the resulting graph before replacing any active state. The
    // validator operates on a disposable conversion, never writes old roots.
    let mut restored = converted.clone();
    restored.items.retain(|f| f.scope != "favorite" || !removed.contains(f.id.as_str()));
    for old in &before {
        let item = restored.items.iter_mut().find(|f| f.scope == "favorite" && f.id == old.id)
            .ok_or_else(|| format!("{UNSUPPORTED}: restored row is absent"))?;
        item.owner_parent_id = old.parent_id.clone();
        item.children = old.is_folder.then(|| old.children.clone());
        item.sub_index = old.sub_index;
    }
    convert::normalize_favorites(&mut restored, &undo_roots, "__core_undo_validation_root__")
        .map_err(|_| format!("{UNSUPPORTED}: restored graph is invalid"))?;
    let mut projected = runtime.list_favors(candidate).map_err(|_|format!("{UNSUPPORTED}: complete core projection cannot be rebuilt safely"))?;
    for item in &mut projected {
        if let Some(path) = item.image_path.as_mut() {
            if let Ok(relative) = Path::new(path).strip_prefix(&runtime.root) {
                *path = final_root.join(relative).to_string_lossy().into_owned();
            }
        }
    }
    let new_after = crate::host::storage_core_favorite_revision(&projected, roots)?;
    let current: BTreeMap<_, _> = projected.into_iter().map(|f| (f.id.clone(), f)).collect();
    let mut rebuilt = Vec::new();
    for old in &before {
        let mut complete = current.get(&old.id).cloned().ok_or_else(|| format!("{UNSUPPORTED}: core projection is absent"))?;
        complete.parent_id = old.parent_id.clone();
        complete.children = old.children.clone();
        complete.sub_index = old.sub_index;
        rebuilt.push(complete);
    }
    let undo = replace(&undo, &[("after_revision", serde_json::to_string(&new_after).map_err(err)?), ("before", serde_json::to_string(&rebuilt).map_err(err)?)])?;
    let state = replace(&state, &[("undo", undo)])?;
    let _lease = runtime.writer.enter(WriteScope::Migration)?;
    let changed = candidate.execute("UPDATE kv SET value=?1 WHERE key=?2 AND value=?3", params![state, STATE_KEY, original]).map_err(err)?;
    if changed != 1 { return Err("CORE_UNDO_STATE_CHANGED".into()); }
    Ok("converted_structure_only")
}

fn read<T: serde::de::DeserializeOwned>(fields: &RawFields, key: &str) -> Result<T, String> {
    serde_json::from_str(fields.get(key)?.ok_or_else(|| format!("{UNSUPPORTED}: missing {key}"))?)
        .map_err(|_| format!("{UNSUPPORTED}: invalid {key}"))
}
fn replace(fields: &RawFields, replacements: &[(&str, String)]) -> Result<String, String> {
    for (key, _) in replacements { fields.get(key)?; }
    Ok(format!("{{{}}}", fields.0.iter().map(|(key, value)| {
        let value = replacements.iter().find(|(name, _)| key.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).unwrap_or_else(|| value.get());
        format!("{}:{value}", serde_json::to_string(key).unwrap())
    }).collect::<Vec<_>>().join(",")))
}
fn nonstructural(record: &FavorRecord) -> Result<Value, String> {
    let mut record = record.clone();
    record.parent_id = None; record.children.clear(); record.sub_index = 0; record.storage_revision = None;
    serde_json::to_value(record).map_err(err)
}
pub(super) fn legacy_favorites(source: &Connection, format: SourceFormat, settings: &MigrationSettings) -> Result<Vec<FavorRecord>, String> {
    if !table_exists(source, "favor_item")? { return Ok(Vec::new()); }
    convert::read_rows(source, "favor_item")?.iter().map(|row| {
        let id = row.required_text("id")?;
        if format == SourceFormat::Documents {
            let data = row.required_text("Data")?;
            let parsed: Value = serde_json::from_str(&data).map_err(err)?;
            let raw = RawFields::parse(&data)?;
            let mut favor = crate::csharp_store_document::parse_favor(&id, &parsed)?;
            let created = parse_time(raw.get("CreateTime")?.ok_or("Missing old CreateTime")?, settings)?.ok_or("Missing old CreateTime")?;
            let updated = raw.get("UpdateTime")?.filter(|v| *v != "null").map(|v| parse_time(v, settings)).transpose()?.flatten().unwrap_or(created.clone());
            let milliseconds = |text: &str| -> Result<DateTime<Utc>, String> {
                let time = DateTime::parse_from_rfc3339(text).map_err(err)?.timestamp_millis();
                Utc.timestamp_millis_opt(time).single().ok_or_else(|| "Old millisecond projection out of range".into())
            };
            favor.create_time = milliseconds(&created)?; favor.update_time = milliseconds(&updated)?;
            favor.storage_revision = Some(crate::model::sha256_hex(format!("{}\0{}", row.text("RowToken")?.unwrap_or_default(), data).as_bytes()));
            Ok(favor)
        } else {
            let string_array = |column: &str| -> Result<Vec<String>, String> {
                row.text(column)?.map(|s| serde_json::from_str(&s).map_err(err)).transpose().map(Option::unwrap_or_default)
            };
            let created = row.integer("create_time")?.ok_or("Missing old native create_time")?;
            let created = Utc.timestamp_millis_opt(created).single().ok_or("Old native create_time out of range")?;
            let updated = row.integer("update_time")?.map(|v| Utc.timestamp_millis_opt(v).single().ok_or("Old native update_time out of range")).transpose()?.unwrap_or(created);
            Ok(FavorRecord {
                storage_revision: None, id, parent_id: row.text("parent_id")?, is_folder: row.integer("is_folder")?.unwrap_or(0) != 0,
                name: row.required_text("name")?, kind: ClipKind::parse(&row.required_text("kind")?), body_text: row.text("body_text")?,
                file_paths: string_array("file_paths")?, image_path: row.text("image_path")?, children: string_array("children")?, create_time: created,
                update_time: updated, source_clip_id: row.text("source_clip_id")?, html_data: row.text("html_data")?, html_source_url: row.text("html_source_url")?,
                qq_rich_text: row.text("qq_rich_text")?, source_process_name: row.text("source_process")?.unwrap_or_default(), sub_index: i32::try_from(row.integer("sub_index")?.unwrap_or(0)).map_err(err)?,
                icon: row.text("icon")?.unwrap_or_default(), execute_param: row.text("execute_param")?.unwrap_or_default(),
            })
        }
    }).collect()
}
