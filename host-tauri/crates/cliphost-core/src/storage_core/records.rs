use super::{write, Runtime, WriteScope};
use crate::model::{hash_files, hash_text, ClipKind, ClipRecord, FavorRecord};
use crate::store::UsageInfo;
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use rusqlite::{params, types::Value as SqlValue, Connection, OptionalExtension, Row};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, HashMap, HashSet};

const COLUMNS:&str="seq,scope,id,kind,title,icon_override,description,payload_version,payload_json,source_extensions_json,dedupe_key,source_process,created_at,updated_at,sub_index,is_pinned,revision,owner_parent_id,children_json,source_history_id";
type RawMap = BTreeMap<String, Box<RawValue>>;
pub(crate) fn token() -> String {
    format!("core2:{}", uuid::Uuid::new_v4())
}
pub(crate) fn time(value: DateTime<Utc>) -> Result<String, String> {
    if !(0..=9999).contains(&value.year()) {
        return Err("Time year is outside the persisted range.".into());
    }
    Ok(value.to_rfc3339_opts(SecondsFormat::Nanos, true))
}
fn parse_time(value: &str) -> Result<DateTime<Utc>, String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| "Invalid persisted UTC time.")?
        .with_timezone(&Utc);
    if time(parsed)? != value {
        return Err("Persisted time is not canonical nanosecond UTC.".into());
    }
    Ok(parsed)
}
fn map(raw: &str) -> Result<RawMap, String> {
    struct UniqueObject(RawMap);
    impl<'de> serde::Deserialize<'de> for UniqueObject {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ObjectVisitor;
            impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
                type Value = UniqueObject;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("an object with unique payload keys")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut fields: M,
                ) -> Result<Self::Value, M::Error> {
                    let mut values = RawMap::new();
                    while let Some((key, value)) = fields.next_entry::<String, Box<RawValue>>()? {
                        if values.insert(key, value).is_some() {
                            return Err(serde::de::Error::custom("Duplicate payload key."));
                        }
                    }
                    Ok(UniqueObject(values))
                }
            }
            deserializer.deserialize_map(ObjectVisitor)
        }
    }
    // BTreeMap's normal deserializer silently keeps the last duplicate key.
    // Reject that ambiguity before an edit can serialize away an earlier token;
    // nested unknown values stay opaque RawValue bytes, including large numbers.
    serde_json::from_str::<UniqueObject>(raw)
        .map(|object| object.0)
        .map_err(|_| "Invalid payload object or duplicate key.".to_owned())
}
fn field<T: serde::de::DeserializeOwned>(payload: &RawMap, key: &str) -> Result<Option<T>, String> {
    payload
        .get(key)
        .map(|v| {
            serde_json::from_str::<Option<T>>(v.get())
                .map_err(|_| format!("Invalid payload field {key}."))
        })
        .transpose()
        .map(Option::flatten)
}
fn set<T: serde::Serialize>(payload: &mut RawMap, key: &str, value: &T) -> Result<(), String> {
    payload.insert(
        key.into(),
        serde_json::value::to_raw_value(value).map_err(|e| e.to_string())?,
    );
    Ok(())
}
fn kind(value: &str) -> Result<ClipKind, String> {
    Ok(match value {
        "text" => ClipKind::TextClipItem,
        "html" => ClipKind::HtmlClipItem,
        "im_rich" => ClipKind::QQRichTextItem,
        "image" => ClipKind::ImageClipItem,
        "files" => ClipKind::FileClipItem,
        "link" => ClipKind::LinkClipItem,
        "command" => ClipKind::CmdClipItem,
        "steps" => ClipKind::StepClipItem,
        "action" => ClipKind::ActionClipItem,
        "folder" => ClipKind::FavoriteClipItem,
        _ => return Err("Unsupported core item type; no text fallback is allowed.".into()),
    })
}
fn kind_token(value: ClipKind) -> &'static str {
    match value {
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
    }
}

/// Persisted dedupe must never probe an old absolute file path. Hash-backed
/// image identities and the other types keep the existing business semantics;
/// path fallbacks are deliberately lexical inside this isolated repository.
pub(crate) fn dedupe(clip: &ClipRecord) -> String {
    let path = |s: &str| {
        s.trim()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_owned()
    };
    match clip.kind {
        ClipKind::FileClipItem => {
            let mut paths: Vec<_> = clip
                .file_paths
                .iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| path(s))
                .collect();
            paths.sort_by_key(|s| s.to_ascii_lowercase());
            format!("file:FileClipItem:{}", paths.join("|"))
        }
        ClipKind::ImageClipItem
            if clip.content_hash.is_empty() && clip.perceptual_hash.trim().is_empty() =>
        {
            format!(
                "image:path:{}",
                path(clip.image_path.as_deref().unwrap_or(""))
            )
        }
        _ => clip.distinct_key(),
    }
}

/// Shared migration/runtime image identity. Only the selected root is inspected
/// by resolver construction; resolving an old locator never probes that path.
pub(crate) fn image_dedupe_key(
    payload_json: &str,
    active_root: &std::path::Path,
    aliases: &[std::path::PathBuf],
) -> Result<String, String> {
    let payload = map(payload_json)?;
    let phash = field::<String>(&payload, "perceptualHash")?.unwrap_or_default();
    if !phash.trim().is_empty() {
        return Ok(format!("image:phash:{}", phash.trim()));
    }
    let hash = field::<String>(&payload, "sourceContentHash")?
        .or(field(&payload, "sourceHash")?)
        .or(field(&payload, "sourceMd5")?)
        .unwrap_or_default();
    if !hash.is_empty() {
        return Ok(format!("image:hash:{}", hash.trim()));
    }
    let locator = field::<String>(&payload, "sourceLocator")?
        .filter(|s| !s.is_empty())
        .ok_or("Image has no source locator.")?;
    let base_token =
        field::<String>(&payload, "locatorBase")?.ok_or("Image locator has no base.")?;
    let base = super::resources::LocatorBase::from_token(&base_token)?;
    let resolver = super::resources::ResourceResolver::new(active_root, aliases)?;
    let normalize = |value: &str| {
        let value = value.replace('/', "\\");
        let value = if value
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case(r"\\?\UNC\"))
        {
            format!(r"\\{}", &value[8..])
        } else {
            value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
        };
        value.trim_end_matches('\\').to_ascii_lowercase()
    };
    if let Ok(resolved) = resolver.resolve(&locator, base) {
        let relative = resolved
            .strip_prefix(resolver.active_root())
            .map_err(|_| "Resolved image identity escaped its selected root.")?;
        return Ok(format!(
            "image:path:data_root:{}",
            normalize(&relative.to_string_lossy())
        ));
    }
    // Unmapped/unsupported references remain separate identities. They are
    // retained verbatim in payload; this derived fallback never opens them.
    let decoded = if locator
        .get(..5)
        .is_some_and(|s| s.eq_ignore_ascii_case("file:"))
    {
        url::Url::parse(&locator)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| locator.clone())
    } else {
        locator.clone()
    };
    Ok(format!(
        "image:path:unresolved:{base_token}:{}",
        normalize(&decoded)
    ))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredItem {
    pub seq: i64,
    pub scope: String,
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub icon_override: Option<String>,
    pub description: String,
    pub payload_version: i64,
    pub payload_json: String,
    pub source_extensions_json: String,
    pub dedupe_key: String,
    pub source_process: String,
    pub created_at: String,
    pub updated_at: String,
    pub sub_index: i32,
    pub is_pinned: bool,
    pub revision: String,
    pub owner_parent_id: Option<String>,
    pub children: Option<Vec<String>>,
    pub source_history_id: Option<String>,
}
fn row(row: &Row<'_>) -> rusqlite::Result<StoredItem> {
    let children: Option<String> = row.get(18)?;
    let children = children
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    18,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(StoredItem {
        seq: row.get(0)?,
        scope: row.get(1)?,
        id: row.get(2)?,
        kind: row.get(3)?,
        title: row.get(4)?,
        icon_override: row.get(5)?,
        description: row.get(6)?,
        payload_version: row.get(7)?,
        payload_json: row.get(8)?,
        source_extensions_json: row.get(9)?,
        dedupe_key: row.get(10)?,
        source_process: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        sub_index: row.get(14)?,
        is_pinned: row.get(15)?,
        revision: row.get(16)?,
        owner_parent_id: row.get(17)?,
        children,
        source_history_id: row.get(19)?,
    })
}
impl Runtime {
    fn item_dedupe(&self, item: &StoredItem) -> Result<String, String> {
        if item.kind == "image" {
            image_dedupe_key(&item.payload_json, &self.root, &self.historical_roots)
        } else {
            Ok(dedupe(&self.clip(item)?))
        }
    }
    pub fn item(&self, c: &Connection, scope: &str, id: &str) -> Result<StoredItem, String> {
        c.query_row(
            &format!("SELECT {COLUMNS} FROM items WHERE scope=?1 AND id=?2"),
            params![scope, id],
            row,
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("NOT_FOUND:{id}"))
    }
    fn items(&self, c: &Connection, scope: &str) -> Result<Vec<StoredItem>, String> {
        let order = if scope == "history" {
            "is_pinned DESC,created_at DESC,sub_index ASC,seq ASC"
        } else {
            "created_at ASC,seq ASC"
        };
        let mut q = c
            .prepare(&format!(
                "SELECT {COLUMNS} FROM items WHERE scope=?1 ORDER BY {order}"
            ))
            .map_err(|e| e.to_string())?;
        let result = q
            .query_map([scope], row)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(result)
    }
    pub fn validate_all(&self, c: &Connection) -> Result<(), String> {
        for scope in ["history", "favorite"] {
            for item in self.items(c, scope)? {
                if item.payload_version != 1 {
                    return Err(
                        "Unsupported core payload version; writable activation was refused.".into(),
                    );
                }
                if !item.revision.starts_with("core2:") {
                    return Err("Invalid core content revision.".into());
                }
                let _: RawMap = map(&item.source_extensions_json)?;
                if scope == "favorite" {
                    self.favor(&item)?;
                } else {
                    self.clip(&item)?;
                }
            }
        }
        let mut q = c
            .prepare("SELECT last_use_time,usage_count FROM usage_info")
            .map_err(|e| e.to_string())?;
        for r in q
            .query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| e.to_string())?
        {
            let (t, n) = r.map_err(|e| e.to_string())?;
            if let Some(t) = t {
                parse_time(&t)?;
            }
            if !(0..=i64::from(i32::MAX)).contains(&n) {
                return Err("Usage count is outside the supported range.".into());
            }
        }
        self.validate_graph(c)
    }
    pub fn clip(&self, item: &StoredItem) -> Result<ClipRecord, String> {
        if item.payload_version != 1 {
            return Err("Unsupported payload version.".into());
        }
        let k = kind(&item.kind)?;
        let p = map(&item.payload_json)?;
        let mut clip = ClipRecord::new_text("", &item.source_process);
        clip.storage_revision = Some(item.revision.clone());
        clip.id = item.id.clone();
        clip.kind = k;
        clip.title = item.title.clone().or(if k == ClipKind::LinkClipItem {
            field::<String>(&p, "sourceTitle")?
        } else {
            None
        });
        clip.description = item.description.clone();
        clip.is_top = item.is_pinned;
        clip.sub_index = item.sub_index;
        clip.create_time = parse_time(&item.created_at)?;
        clip.update_time = parse_time(&item.updated_at)?;
        clip.body_text = None;
        clip.content_hash.clear();
        match k {
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                clip.body_text = Some(field::<String>(&p, "text")?.unwrap_or_default());
                clip.html_data = field(&p, "html")?;
                clip.html_source_url = field(&p, "sourceUrl")?;
                clip.qq_rich_text = field(&p, "xml")?;
                clip.content_hash = if k == ClipKind::QQRichTextItem {
                    hash_text(&format!(
                        "qq:{}",
                        clip.qq_rich_text.as_deref().unwrap_or("")
                    ))
                } else {
                    hash_text(clip.body_text.as_deref().unwrap_or(""))
                };
            }
            ClipKind::ImageClipItem => {
                let locator = field::<String>(&p, "sourceLocator")?
                    .filter(|s| !s.is_empty())
                    .ok_or("Image has no source locator.")?;
                let base = super::resources::LocatorBase::from_token(
                    &field::<String>(&p, "locatorBase")?.ok_or("Image locator has no base.")?,
                )?;
                // Never expose an unmapped production path to legacy consumers.
                clip.image_path =
                    super::resources::ResourceResolver::new(&self.root, &self.historical_roots)?
                        .resolve(&locator, base)
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned());
                clip.perceptual_hash = field::<String>(&p, "perceptualHash")?.unwrap_or_default();
                clip.content_hash = field::<String>(&p, "sourceContentHash")?
                    .or(field(&p, "sourceHash")?)
                    .or(field(&p, "sourceMd5")?)
                    .unwrap_or_default();
            }
            ClipKind::FileClipItem => {
                clip.file_paths = field::<Vec<String>>(&p, "paths")?.unwrap_or_default();
                clip.content_hash = hash_files(&clip.file_paths);
            }
            ClipKind::LinkClipItem => {
                clip.body_text = Some(field::<String>(&p, "url")?.unwrap_or_default());
                clip.content_hash = hash_text(clip.body_text.as_deref().unwrap_or(""));
            }
            ClipKind::CmdClipItem => {
                clip.body_text = Some(field::<String>(&p, "command")?.unwrap_or_default());
                clip.content_hash = hash_text(clip.body_text.as_deref().unwrap_or(""));
            }
            ClipKind::StepClipItem => {
                clip.body_text = Some(field::<String>(&p, "data")?.unwrap_or_default());
                clip.content_hash = hash_text(clip.body_text.as_deref().unwrap_or(""));
            }
            ClipKind::ActionClipItem => {
                let data =
                    field::<String>(&p, "data")?.ok_or("Action has no serialized payload.")?;
                crate::quicker_action::ActionReference::parse(&data)?;
                clip.content_hash = hash_text(&data);
                clip.body_text = Some(data);
                clip.execute_param = field::<String>(&p, "executeParam")?.unwrap_or_default();
            }
            ClipKind::FavoriteClipItem => {}
        }
        Ok(clip)
    }
    fn favor(&self, item: &StoredItem) -> Result<FavorRecord, String> {
        let clip = self.clip(item)?;
        let p = map(&item.payload_json)?;
        let mut record = FavorRecord::from_clip(&clip, item.owner_parent_id.clone());
        record.id = item.id.clone();
        record.storage_revision = Some(item.revision.clone());
        record.is_folder = item.kind == "folder";
        record.name = item
            .title
            .clone()
            .unwrap_or_else(|| clip.csharp_visual_name());
        record.icon = item
            .icon_override
            .clone()
            .filter(|s| !s.is_empty())
            .or(field::<String>(&p, "sourceIcon")?)
            .unwrap_or_default();
        record.children = item.children.clone().unwrap_or_default();
        record.create_time = clip.create_time;
        record.update_time = clip.update_time;
        record.source_clip_id = item.source_history_id.clone();
        record.sub_index = item.sub_index;
        Ok(record)
    }
    pub fn get_clip(&self, c: &Connection, id: &str) -> Result<ClipRecord, String> {
        let mut clip = self.clip(&self.item(c, "history", id)?)?;
        let usage = self.usage(c, &crate::store::clip_usage_key(id))?;
        clip.usage_count = usage.usage_count;
        clip.last_access = usage.last_use_time;
        Ok(clip)
    }
    pub fn list_clips(&self, c: &Connection) -> Result<Vec<ClipRecord>, String> {
        self.items(c, "history")?
            .into_iter()
            .map(|i| {
                let mut clip = self.clip(&i)?;
                let u = self.usage(c, &crate::store::clip_usage_key(&i.id))?;
                clip.usage_count = u.usage_count;
                clip.last_access = u.last_use_time;
                Ok(clip)
            })
            .collect()
    }
    pub fn list_keys(
        &self,
        c: &Connection,
        filter: &str,
    ) -> Result<Vec<crate::store::ClipListKey>, String> {
        let needs_text = crate::clip_filter::requires_text(filter);
        let projection = if needs_text {
            ",COALESCE(json_extract(i.payload_json,'$.text'),json_extract(i.payload_json,'$.url'),json_extract(i.payload_json,'$.command'),json_extract(i.payload_json,'$.data')),COALESCE(i.title,CASE WHEN i.kind='link' THEN json_extract(i.payload_json,'$.sourceTitle') END)"
        } else {
            ""
        };
        let mut query=c.prepare(&format!("SELECT i.id,i.kind,i.is_pinned,i.source_process,i.created_at,u.last_use_time,COALESCE(u.usage_count,0){projection} FROM items i LEFT JOIN usage_info u ON u.id='clip_item_'||i.id WHERE i.scope='history' ORDER BY i.is_pinned DESC,i.created_at DESC,i.sub_index ASC,i.seq ASC")).map_err(|e|e.to_string())?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, i64>(6)?,
                    if needs_text {
                        r.get::<_, Option<String>>(7)?
                    } else {
                        None
                    },
                    if needs_text {
                        r.get::<_, Option<String>>(8)?
                    } else {
                        None
                    },
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut result = Vec::new();
        for row in rows {
            let (id, token, pinned, process, created, last, count, text, title) =
                row.map_err(|e| e.to_string())?;
            let kind = kind(&token)?;
            if !crate::clip_filter::matches_with_text(kind, pinned, text.as_deref(), filter)
                && !(needs_text
                    && crate::clip_filter::matches_with_text(
                        kind,
                        pinned,
                        title.as_deref(),
                        filter,
                    ))
            {
                continue;
            }
            result.push(crate::store::ClipListKey {
                id,
                kind,
                is_top: pinned,
                source_process_name: process,
                create_ms: parse_time(&created)?.timestamp_millis(),
                last_access_ms: last
                    .as_deref()
                    .map(parse_time)
                    .transpose()?
                    .map(|t| t.timestamp_millis()),
                usage_count: i32::try_from(count)
                    .map_err(|_| "Usage count exceeds the supported counter range.")?,
            });
        }
        Ok(result)
    }
    pub fn clips_by_ids(
        &self,
        c: &Connection,
        ids: &[&str],
    ) -> Result<HashMap<String, ClipRecord>, String> {
        let mut result = HashMap::with_capacity(ids.len());
        for id in ids {
            match self.get_clip(c, id) {
                Ok(item) => {
                    result.insert((*id).into(), item);
                }
                Err(e) if e.starts_with("NOT_FOUND:") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(result)
    }
    pub fn recent_clips(&self, c: &Connection, limit: usize) -> Result<Vec<ClipRecord>, String> {
        let mut query=c.prepare(&format!("SELECT {COLUMNS} FROM items WHERE scope='history' ORDER BY created_at DESC,sub_index ASC,seq ASC LIMIT ?1")).map_err(|e|e.to_string())?;
        let items = query
            .query_map([limit as i64], row)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        items.iter().map(|i| self.clip(i)).collect()
    }
    pub fn get_favor(&self, c: &Connection, id: &str) -> Result<FavorRecord, String> {
        self.favor(&self.item(c, "favorite", id)?)
    }
    pub fn list_favors(&self, c: &Connection) -> Result<Vec<FavorRecord>, String> {
        self.items(c, "favorite")?
            .iter()
            .map(|i| self.favor(i))
            .collect()
    }
    pub fn usage(&self, c: &Connection, key: &str) -> Result<UsageInfo, String> {
        let r: Option<(Option<String>, i64)> = c
            .query_row(
                "SELECT last_use_time,usage_count FROM usage_info WHERE id=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match r {
            None => Ok(UsageInfo::default()),
            Some((t, count)) => Ok(UsageInfo {
                last_use_time: t.as_deref().map(parse_time).transpose()?,
                usage_count: i32::try_from(count)
                    .map_err(|_| "Usage count exceeds the supported counter range.")?,
            }),
        }
    }
    pub fn update_usage(&self, c: &Connection, key: &str) -> Result<UsageInfo, String> {
        let scope = if key
            .strip_prefix("clip_item_")
            .is_some_and(|s| !s.is_empty())
        {
            WriteScope::History
        } else if key
            .strip_prefix("favor_item_")
            .is_some_and(|s| !s.is_empty())
        {
            WriteScope::Favorite
        } else {
            return Err("Unknown usage key cannot be written by a normal repository.".into());
        };
        write(c, &self.writer, scope, || {
            let now = time(Utc::now())?;
            let changed = c.execute("INSERT INTO usage_info(id,last_use_time,usage_count) VALUES(?1,?2,1) ON CONFLICT(id) DO UPDATE SET last_use_time=excluded.last_use_time,usage_count=usage_info.usage_count+1 WHERE usage_info.usage_count<2147483647",params![key,now]).map_err(|e|e.to_string())?;
            if changed != 1 {
                return Err("Usage counter limit reached; update rolled back.".into());
            }
            self.usage(c, key)
        })
    }
    fn new_item(&self, clip: &ClipRecord, scope: &str) -> Result<StoredItem, String> {
        let mut p = RawMap::new();
        match clip.kind {
            ClipKind::TextClipItem => set(&mut p, "text", &clip.body_text)?,
            ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                set(&mut p, "text", &clip.body_text)?;
                set(&mut p, "html", &clip.html_data)?;
                set(&mut p, "sourceUrl", &clip.html_source_url)?;
                if clip.kind == ClipKind::QQRichTextItem {
                    set(&mut p, "xml", &clip.qq_rich_text)?;
                    set(
                        &mut p,
                        "origin",
                        &crate::im_rich::resolve_origin(
                            &clip.source_process_name,
                            clip.qq_rich_text.as_deref().unwrap_or(""),
                        )
                        .as_api_token(),
                    )?;
                }
            }
            ClipKind::ImageClipItem => {
                let path = clip
                    .image_path
                    .as_deref()
                    .ok_or("A new image has no locator.")?;
                set(&mut p, "sourceLocator", &path)?;
                set(
                    &mut p,
                    "locatorBase",
                    &if std::path::Path::new(path).is_absolute() {
                        "absolute"
                    } else {
                        "image_root"
                    },
                )?;
                set(&mut p, "perceptualHash", &clip.perceptual_hash)?;
                set(&mut p, "sourceContentHash", &clip.content_hash)?;
            }
            ClipKind::FileClipItem => set(&mut p, "paths", &clip.file_paths)?,
            ClipKind::LinkClipItem => set(&mut p, "url", &clip.body_text)?,
            ClipKind::CmdClipItem => set(&mut p, "command", &clip.body_text)?,
            ClipKind::StepClipItem => set(&mut p, "data", &clip.body_text)?,
            ClipKind::ActionClipItem => {
                set(&mut p, "data", &clip.body_text)?;
                set(&mut p, "executeParam", &clip.execute_param)?;
            }
            ClipKind::FavoriteClipItem => {}
        }
        let payload_json = serde_json::to_string(&p).map_err(|e| e.to_string())?;
        let dedupe_key = if clip.kind == ClipKind::ImageClipItem {
            image_dedupe_key(&payload_json, &self.root, &self.historical_roots)?
        } else {
            dedupe(clip)
        };
        Ok(StoredItem {
            seq: 0,
            scope: scope.into(),
            id: clip.id.clone(),
            kind: kind_token(clip.kind).into(),
            title: clip.title.clone(),
            icon_override: None,
            description: clip.description.clone(),
            payload_version: 1,
            payload_json,
            source_extensions_json: "{}".into(),
            dedupe_key,
            source_process: clip.source_process_name.clone(),
            created_at: time(clip.create_time)?,
            updated_at: time(clip.update_time)?,
            sub_index: clip.sub_index,
            is_pinned: scope == "history" && clip.is_top,
            revision: token(),
            owner_parent_id: None,
            children: (clip.kind == ClipKind::FavoriteClipItem).then(Vec::new),
            source_history_id: None,
        })
    }
    fn insert_raw(&self, c: &Connection, item: &StoredItem) -> Result<(), String> {
        self.clip(item)?;
        c.execute("INSERT INTO items(scope,id,kind,title,icon_override,description,payload_version,payload_json,source_extensions_json,dedupe_key,source_process,created_at,updated_at,sub_index,is_pinned,revision,owner_parent_id,children_json,source_history_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",params![item.scope,item.id,item.kind,item.title,item.icon_override,item.description,item.payload_version,item.payload_json,item.source_extensions_json,item.dedupe_key,item.source_process,item.created_at,item.updated_at,item.sub_index,item.is_pinned,item.revision,item.owner_parent_id,item.children.as_ref().map(serde_json::to_string).transpose().map_err(|e|e.to_string())?,item.source_history_id]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn insert_clip(&self, c: &Connection, clip: &ClipRecord) -> Result<(), String> {
        let item = self.new_item(clip, "history")?;
        write(c, &self.writer, WriteScope::History, || {
            self.insert_raw(c, &item)
        })
    }
    pub fn insert_favor(&self, c: &Connection, favor: &FavorRecord) -> Result<(), String> {
        let clip = favor_clip(favor);
        let mut item = self.new_item(&clip, "favorite")?;
        item.title = Some(favor.name.clone());
        item.icon_override = Some(favor.icon.clone());
        item.sub_index = favor.sub_index;
        item.owner_parent_id = favor.parent_id.clone();
        item.children = favor.is_folder.then(|| favor.children.clone());
        item.source_history_id = favor.source_clip_id.clone();
        write(c, &self.writer, WriteScope::Favorite, || {
            self.insert_raw(c, &item)
        })
    }
    fn check_revision(&self, old: &StoredItem, expected: Option<&str>) -> Result<(), String> {
        if expected != Some(old.revision.as_str()) {
            return Err(
                "WRITE_CONFLICT: missing or stale core revision; refresh before saving.".into(),
            );
        }
        Ok(())
    }
    pub fn validate_source_revision(
        &self,
        c: &Connection,
        table: &str,
        id: &str,
        expected: Option<&str>,
    ) -> Result<(), String> {
        let scope = match table {
            "clip_item" => "history",
            "favor_item" => "favorite",
            _ => return Err("Invalid inheritance source.".into()),
        };
        self.check_revision(&self.item(c, scope, id)?, expected)
    }
    fn patch_content(
        &self,
        old: &StoredItem,
        new: &mut StoredItem,
        baseline: &ClipRecord,
        incoming: &ClipRecord,
    ) -> Result<(), String> {
        if baseline.kind != incoming.kind {
            return Err("Changing type requires an explicit versioned conversion command.".into());
        }
        let mut p = map(&old.payload_json)?;
        let mut changed = false;
        let key = match incoming.kind {
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                Some("text")
            }
            ClipKind::LinkClipItem => Some("url"),
            ClipKind::CmdClipItem => Some("command"),
            ClipKind::StepClipItem | ClipKind::ActionClipItem => Some("data"),
            _ => None,
        };
        if baseline.body_text != incoming.body_text {
            if let Some(key) = key {
                set(&mut p, key, &incoming.body_text)?;
                changed = true;
            }
        }
        for (key, was, is) in [
            ("html", &baseline.html_data, &incoming.html_data),
            (
                "sourceUrl",
                &baseline.html_source_url,
                &incoming.html_source_url,
            ),
            ("xml", &baseline.qq_rich_text, &incoming.qq_rich_text),
        ] {
            if was != is {
                if !matches!(
                    incoming.kind,
                    ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
                ) || key == "xml" && incoming.kind != ClipKind::QQRichTextItem
                {
                    return Err("Rich payload fields do not belong to this type.".into());
                }
                set(&mut p, key, is)?;
                changed = true;
            }
        }
        if baseline.file_paths != incoming.file_paths {
            if incoming.kind != ClipKind::FileClipItem {
                return Err("File fields do not belong to this type.".into());
            }
            set(&mut p, "paths", &incoming.file_paths)?;
            changed = true;
        }
        if baseline.image_path != incoming.image_path {
            if incoming.kind != ClipKind::ImageClipItem {
                return Err("Image fields do not belong to this type.".into());
            }
            let locator = incoming
                .image_path
                .as_ref()
                .ok_or("An image replacement requires a prepared file.")?;
            if incoming.content_hash.is_empty() {
                return Err("An image replacement requires an explicit content hash.".into());
            }
            set(&mut p, "sourceLocator", locator)?;
            set(
                &mut p,
                "locatorBase",
                &if std::path::Path::new(locator).is_absolute() {
                    "absolute"
                } else {
                    "image_root"
                },
            )?;
            set(&mut p, "sourceContentHash", &incoming.content_hash)?;
            set(&mut p, "perceptualHash", &incoming.perceptual_hash)?;
            changed = true;
        }
        if baseline.execute_param != incoming.execute_param {
            if incoming.kind != ClipKind::ActionClipItem {
                return Err("Execution parameters do not belong to this type.".into());
            }
            set(&mut p, "executeParam", &incoming.execute_param)?;
            changed = true;
        }
        if changed {
            new.payload_json = serde_json::to_string(&p).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn update_raw(
        &self,
        c: &Connection,
        old: &StoredItem,
        new: &StoredItem,
    ) -> Result<bool, String> {
        let mut values: Vec<SqlValue> = Vec::new();
        let mut assignments = Vec::new();
        macro_rules! changed {
            ($field:ident,$name:literal) => {
                if old.$field != new.$field {
                    assignments.push(concat!($name, "=?").to_owned());
                    values.push(SqlValue::from(new.$field.clone()));
                }
            };
        }
        changed!(title, "title");
        changed!(icon_override, "icon_override");
        changed!(description, "description");
        changed!(payload_json, "payload_json");
        changed!(source_extensions_json, "source_extensions_json");
        changed!(dedupe_key, "dedupe_key");
        changed!(created_at, "created_at");
        changed!(sub_index, "sub_index");
        if old.is_pinned != new.is_pinned {
            assignments.push("is_pinned=?".into());
            values.push(SqlValue::Integer(new.is_pinned as i64));
        }
        changed!(owner_parent_id, "owner_parent_id");
        if old.children != new.children {
            assignments.push("children_json=?".into());
            values.push(
                new.children
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| e.to_string())?
                    .map(SqlValue::Text)
                    .unwrap_or(SqlValue::Null),
            );
        }
        if assignments.is_empty() {
            return Ok(false);
        }
        assignments.push("updated_at=?".into());
        values.push(SqlValue::Text(new.updated_at.clone()));
        assignments.push("revision=?".into());
        values.push(SqlValue::Text(token()));
        values.extend([
            SqlValue::Text(old.scope.clone()),
            SqlValue::Text(old.id.clone()),
            SqlValue::Text(old.revision.clone()),
        ]);
        let count = c
            .execute(
                &format!(
                    "UPDATE items SET {} WHERE scope=? AND id=? AND revision=?",
                    assignments.join(",")
                ),
                rusqlite::params_from_iter(values),
            )
            .map_err(|e| e.to_string())?;
        if count != 1 {
            return Err("WRITE_CONFLICT: core revision changed during the command.".into());
        }
        Ok(true)
    }
    pub fn save_clip(
        &self,
        c: &Connection,
        incoming: &ClipRecord,
        title_intent: Option<Option<String>>,
    ) -> Result<(), String> {
        write(c, &self.writer, WriteScope::History, || {
            let old = self.item(c, "history", &incoming.id)?;
            self.check_revision(&old, incoming.storage_revision.as_deref())?;
            let baseline = self.clip(&old)?;
            if incoming.source_process_name != baseline.source_process_name {
                return Err("Capture provenance is immutable.".into());
            }
            let mut new = old.clone();
            self.patch_content(&old, &mut new, &baseline, incoming)?;
            if let Some(title) = title_intent {
                new.title = title;
            } else if incoming.title != baseline.title {
                new.title = incoming.title.clone();
            }
            new.description = incoming.description.clone();
            new.is_pinned = incoming.is_top;
            new.sub_index = incoming.sub_index;
            new.created_at = time(incoming.create_time)?;
            new.updated_at = time(incoming.update_time)?;
            if new.payload_json != old.payload_json || new.kind == "link" && new.title != old.title
            {
                new.dedupe_key = self.item_dedupe(&new)?;
            }
            self.clip(&new)?;
            self.update_raw(c, &old, &new)?;
            Ok(())
        })
    }
    pub fn save_favor(
        &self,
        c: &Connection,
        incoming: &FavorRecord,
        title_intent: Option<Option<String>>,
    ) -> Result<(), String> {
        let standalone = c.is_autocommit();
        write(c, &self.writer, WriteScope::Favorite, || {
            let old = self.item(c, "favorite", &incoming.id)?;
            self.check_revision(&old, incoming.storage_revision.as_deref())?;
            let baseline = self.favor(&old)?;
            if incoming.source_process_name != baseline.source_process_name
                || incoming.source_clip_id != baseline.source_clip_id
            {
                return Err("Capture provenance is immutable.".into());
            }
            if incoming.is_folder != baseline.is_folder {
                return Err("Folder conversion requires an explicit conversion command.".into());
            }
            let mut new = old.clone();
            if baseline.image_path != incoming.image_path && old.kind == "image" {
                return Err(
                    "A favorite image replacement requires a typed image command with hashes."
                        .into(),
                );
            }
            self.patch_content(
                &old,
                &mut new,
                &favor_clip(&baseline),
                &favor_clip(incoming),
            )?;
            if let Some(title) = title_intent {
                new.title = title;
            } else if incoming.name != baseline.name {
                new.title = Some(incoming.name.clone());
            }
            if incoming.icon != baseline.icon {
                new.icon_override = Some(incoming.icon.clone());
            }
            new.owner_parent_id = incoming.parent_id.clone();
            new.children = incoming.is_folder.then(|| incoming.children.clone());
            new.sub_index = incoming.sub_index;
            new.updated_at = time(incoming.update_time)?;
            if new.payload_json != old.payload_json || new.kind == "link" && new.title != old.title
            {
                new.dedupe_key = self.item_dedupe(&new)?;
            }
            self.clip(&new)?;
            self.update_raw(c, &old, &new)?;
            if standalone {
                self.validate_graph(c)?;
            }
            Ok(())
        })
    }
    pub fn delete_exact(
        &self,
        c: &Connection,
        scope: &str,
        ids: &[String],
    ) -> Result<usize, String> {
        let s = if scope == "history" {
            WriteScope::History
        } else {
            WriteScope::Favorite
        };
        write(c, &self.writer, s, || {
            let mut n = 0;
            for id in ids {
                n += c
                    .execute(
                        "DELETE FROM items WHERE scope=?1 AND id=?2",
                        params![scope, id],
                    )
                    .map_err(|e| e.to_string())?;
            }
            Ok(n)
        })
    }
    pub fn delete_favors(&self, c: &Connection, ids: &[String]) -> Result<Vec<String>, String> {
        write(c, &self.writer, WriteScope::Favorite, || {
            let all = self.items(c, "favorite")?;
            let mut removed = HashSet::new();
            let mut roots = Vec::new();
            for id in ids {
                if all.iter().any(|i| &i.id == id) {
                    roots.push(id.clone());
                    removed.insert(id.clone());
                }
            }
            loop {
                let mut progress = false;
                for item in &all {
                    if item
                        .owner_parent_id
                        .as_ref()
                        .is_some_and(|p| removed.contains(p))
                        && removed.insert(item.id.clone())
                    {
                        progress = true;
                    }
                }
                if !progress {
                    break;
                }
            }
            for id in &removed {
                c.execute("DELETE FROM items WHERE scope='favorite' AND id=?1", [id])
                    .map_err(|e| e.to_string())?;
            }
            for item in all {
                if !removed.contains(&item.id) {
                    let mut new = item.clone();
                    if let Some(children) = &mut new.children {
                        children.retain(|id| !removed.contains(id));
                    }
                    self.update_raw(c, &item, &new)?;
                }
            }
            self.validate_graph(c)?;
            Ok(roots)
        })
    }
    pub fn validate_graph(&self, c: &Connection) -> Result<(), String> {
        let items = self.items(c, "favorite")?;
        let by_id: HashMap<_, _> = items.iter().map(|i| (i.id.as_str(), i)).collect();
        for item in &items {
            if let Some(parent) = &item.owner_parent_id {
                let p = by_id
                    .get(parent.as_str())
                    .ok_or("Missing favorite owner.")?;
                if p.kind != "folder" {
                    return Err("Favorite owner is not a folder.".into());
                }
            }
            let mut seen = HashSet::new();
            for child in item.children.iter().flatten() {
                if !seen.insert(child) || child == &item.id || !by_id.contains_key(child.as_str()) {
                    return Err("Invalid favorite child reference.".into());
                }
            }
        }
        // The visible graph is explicit Children plus the compatibility
        // owner->child fallback. Individually acyclic graphs can form a cycle
        // when combined, so validate the actual effective edge union.
        let mut degree: HashMap<&str, usize> = by_id.keys().map(|k| (*k, 0)).collect();
        let mut edges: HashMap<&str, HashSet<&str>> = HashMap::new();
        for item in &items {
            for child in item.children.iter().flatten() {
                edges.entry(item.id.as_str()).or_default().insert(child);
            }
            if let Some(owner) = item.owner_parent_id.as_deref() {
                edges.entry(owner).or_default().insert(item.id.as_str());
            }
        }
        for children in edges.values() {
            for child in children {
                *degree.get_mut(child).ok_or("Missing graph reference")? += 1;
            }
        }
        let mut ready: Vec<&str> = degree
            .iter()
            .filter_map(|(id, n)| (*n == 0).then_some(*id))
            .collect();
        let mut count = 0;
        while let Some(id) = ready.pop() {
            count += 1;
            for child in edges.get(id).into_iter().flatten() {
                let n = degree.get_mut(child).unwrap();
                *n -= 1;
                if *n == 0 {
                    ready.push(child);
                }
            }
        }
        if count != items.len() {
            return Err("Favorite effective graph contains a cycle.".into());
        }
        Ok(())
    }
    pub fn inherit(
        &self,
        c: &Connection,
        table: &str,
        id: &str,
        incoming: &FavorRecord,
    ) -> Result<(), String> {
        let scope = match table {
            "clip_item" => "history",
            "favor_item" => "favorite",
            _ => return Err("Invalid inheritance source.".into()),
        };
        write(c, &self.writer, WriteScope::Favorite, || {
            let source = self.item(c, scope, id)?;
            let old = self.item(c, "favorite", &incoming.id)?;
            let mut new = old.clone();
            self.inherited_content(&source, &mut new, incoming)?;
            self.update_raw(c, &old, &new)?;
            Ok(())
        })
    }
    fn inherited_content(
        &self,
        source: &StoredItem,
        new: &mut StoredItem,
        incoming: &FavorRecord,
    ) -> Result<(), String> {
        let source_clip = self.clip(source)?;
        let source_favor = if source.scope == "favorite" {
            self.favor(source)?
        } else {
            FavorRecord::from_clip(&source_clip, None)
        };
        new.payload_json = source.payload_json.clone();
        new.source_extensions_json = source.source_extensions_json.clone();
        new.description = source.description.clone();
        let mut target_clip = favor_clip(incoming);
        // FavorRecord is a display projection with no hash fields. A prepared
        // library copy changes the locator, never the identity of its bytes.
        target_clip.content_hash = source_clip.content_hash.clone();
        target_clip.perceptual_hash = source_clip.perceptual_hash.clone();
        self.patch_content(source, new, &source_clip, &target_clip)?;
        new.title = if incoming.name == source_favor.name {
            source.title.clone()
        } else {
            Some(incoming.name.clone())
        };
        new.icon_override = if incoming.icon.is_empty() || incoming.icon == source_favor.icon {
            source.icon_override.clone()
        } else {
            Some(incoming.icon.clone())
        };
        new.dedupe_key = self.item_dedupe(new)?;
        Ok(())
    }
}
fn favor_clip(f: &FavorRecord) -> ClipRecord {
    let mut c = ClipRecord::new_text("", &f.source_process_name);
    c.storage_revision = f.storage_revision.clone();
    c.id = f.id.clone();
    c.kind = if f.is_folder {
        ClipKind::FavoriteClipItem
    } else {
        f.kind
    };
    c.title = Some(f.name.clone());
    c.body_text = f.body_text.clone();
    c.html_data = f.html_data.clone();
    c.html_source_url = f.html_source_url.clone();
    c.qq_rich_text = f.qq_rich_text.clone();
    c.file_paths = f.file_paths.clone();
    c.image_path = f.image_path.clone();
    c.create_time = f.create_time;
    c.update_time = f.update_time;
    c.sub_index = f.sub_index;
    c.execute_param = f.execute_param.clone();
    c
}
