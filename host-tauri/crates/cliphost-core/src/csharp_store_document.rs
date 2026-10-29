//! In-place C# document compatibility. The original `Id`/`Data` tables remain
//! authoritative; relational Rust rows are only a query cache of these documents.
//! Never rebuild a document from scratch on update: newer/older C# fields that
//! Rust does not model must survive a round trip.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};

use crate::model::{ClipKind, ClipRecord, FavorRecord};
use crate::store::UsageInfo;

pub(crate) fn parse_clip(id: &str, document: &Value) -> Result<ClipRecord, String> {
    crate::csharp_import_document::parse_clip(id, document)
}

pub(crate) fn parse_favor(id: &str, document: &Value) -> Result<FavorRecord, String> {
    match crate::csharp_import_document::parse_favor(id, document) {
        Ok(record) => Ok(record),
        Err(error) => {
            // Old C# leaf records may retain a previous folder's Children.
            // They are inactive metadata for a leaf, not permission to turn it
            // into a folder or erase its original document. Keep import and
            // migration strict; normalize only this runtime query projection.
            if !has_residual_children(document)
                || parse_clip(id, document)?.kind == ClipKind::FavoriteClipItem
            {
                return Err(error);
            }
            let mut projected = document.clone();
            set(projected.as_object_mut().unwrap(), "Children", json!([]));
            crate::csharp_import_document::parse_favor(id, &projected)
        }
    }
}

fn has_residual_children(document: &Value) -> bool {
    document.as_object().and_then(|object| field(object, "Children"))
        .and_then(Value::as_array)
        .is_some_and(|children| !children.is_empty() && children.iter().all(Value::is_string))
}

/// Accepts either serde's camelCase record or a SQL `json_object` of the cache
/// columns (snake_case, epoch milliseconds, arrays serialized as JSON strings).
pub(crate) fn write_document(
    table: &str,
    rust: &Value,
    previous: Option<&Value>,
) -> Result<Value, String> {
    let favorite = match table {
        "clip" | "clip_item" => false,
        "favor" | "favor_item" => true,
        _ => return Err(format!("不支持的 C# 文档表：{table}。")),
    };
    let rust = expanded_record(rust)?;
    let mut document = previous_object(previous, "Data")?;
    let id = text(&rust, &["id"], None)?.ok_or("Rust 条目缺少 id。")?;
    if id.trim().is_empty() {
        return Err("Rust 条目 ID 为空。".into());
    }
    if let Some(old_id) = field(&document, "Id").and_then(Value::as_str) {
        if old_id != id {
            return Err("不能改写不同 ID 的 C# 文档。".into());
        }
    }
    let kind_name = text(&rust, &["kind"], None)?.ok_or("Rust 条目缺少 kind。")?;
    let kind = kind(&kind_name)?;
    let folder = favorite && flag(&rust, &["isFolder", "is_folder"], false)?;
    let kind = if folder {
        ClipKind::FavoriteClipItem
    } else {
        kind
    };
    let mut item = match field(&document, "Item") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(object)) => object.clone(),
        _ => return Err("原 C# Item 必须是对象。".into()),
    };
    let old_type = field(&item, "ObjType").or_else(|| field(&item, "Type"));
    let directory = kind == ClipKind::FileClipItem
        && (kind_name.eq_ignore_ascii_case("directory") || is_directory_type(old_type));
    set(&mut item, "ObjType", json!(type_id(kind, directory)));
    // FileClipItem's legacy Type setter is applied by Newtonsoft as well. A
    // stale Type would otherwise change a newly written ObjType back to File.
    if field(&item, "Type").is_some() {
        set(&mut item, "Type", json!(type_id(kind, directory)));
    }

    set(&mut document, "Id", json!(id));
    set_time(
        &mut document,
        &rust,
        "CreateTime",
        &["createTime", "create_time"],
        true,
    )?;
    set_time(
        &mut document,
        &rust,
        "UpdateTime",
        &["updateTime", "update_time"],
        false,
    )?;
    if field(&document, "UpdateTime").is_none() {
        let created = field(&document, "CreateTime").cloned().unwrap();
        set(&mut document, "UpdateTime", created);
    }
    copy_text(
        &mut document,
        &rust,
        "SourceProcessName",
        &["sourceProcessName", "source_process_name", "source_process"],
    )?;
    copy_text(&mut document, &rust, "Description", &["description"])?;
    copy_text(&mut document, &rust, "Icon", &["icon"])?;
    if find(&rust, &["subIndex", "sub_index"]).is_some() {
        let value = integer(&rust, &["subIndex", "sub_index"], 0)?;
        set(&mut document, "SubIndex", json!(value));
    }
    if favorite {
        copy_text(&mut document, &rust, "Title", &["name", "title"])?;
        copy_text(&mut document, &rust, "ParentId", &["parentId", "parent_id"])?;
        copy_text(
            &mut document,
            &rust,
            "SourceClipId",
            &["sourceClipId", "source_clip_id"],
        )?;
        let children = string_array(&rust, &["children"], Vec::new())?;
        if kind != ClipKind::FavoriteClipItem && !children.is_empty() {
            return Err("非收藏夹条目不能包含子项。".into());
        }
        let preserve_residual_children = kind != ClipKind::FavoriteClipItem
            && previous.is_some_and(has_residual_children)
            && previous.map(|old| parse_clip(&id, old)).transpose()?
                .is_some_and(|old| old.kind != ClipKind::FavoriteClipItem);
        if !preserve_residual_children {
            set(&mut document, "Children", json!(children));
        }
    } else {
        copy_text(&mut document, &rust, "Title", &["title"])?;
        if find(&rust, &["isTop", "is_top"]).is_some() {
            set(
                &mut document,
                "IsTop",
                json!(flag(&rust, &["isTop", "is_top"], false)?),
            );
        }
    }

    let body = || text(&rust, &["bodyText", "body_text"], Some(""));
    match kind {
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
            set(&mut item, "Text", json!(body()?.unwrap_or_default()));
            if kind != ClipKind::TextClipItem {
                copy_text(&mut item, &rust, "HtmlData", &["htmlData", "html_data"])?;
                copy_text(
                    &mut item,
                    &rust,
                    "HtmlSourceUrl",
                    &["htmlSourceUrl", "html_source_url"],
                )?;
            }
            if kind == ClipKind::QQRichTextItem {
                copy_text(
                    &mut item,
                    &rust,
                    "QQRichText",
                    &["qqRichText", "qq_rich_text"],
                )?;
            }
        }
        ClipKind::FileClipItem => {
            set(
                &mut item,
                "FileList",
                json!(string_array(
                    &rust,
                    &["filePaths", "file_paths"],
                    Vec::new()
                )?),
            );
        }
        ClipKind::ImageClipItem => {
            let path = text(&rust, &["imagePath", "image_path"], None)?
                .filter(|path| !path.trim().is_empty())
                .ok_or("图片条目缺少路径。")?;
            let old_path = image_path(&item);
            let path_changed = old_path.as_deref().is_some_and(|old| old != path);
            set(&mut item, "ImagePath", json!(path));
            if field(&item, "ImageSource").is_some() {
                set(&mut item, "ImageSource", json!(path));
            }
            if let Some(Value::Object(source)) = field(&item, "RSource") {
                let mut source = source.clone();
                let absolute = is_absolute_path(&path);
                set(&mut source, "IsInImageDir", json!(!absolute));
                set(
                    &mut source,
                    if absolute { "APath" } else { "RPath" },
                    json!(path),
                );
                set(&mut item, "RSource", Value::Object(source));
            }
            if find(&rust, &["contentHash", "content_hash"]).is_some() {
                let hash =
                    text(&rust, &["contentHash", "content_hash"], Some(""))?.unwrap_or_default();
                set(&mut item, "ImageHash", json!(hash));
                if field(&item, "ImageMD5").is_some() {
                    set(&mut item, "ImageMD5", json!(hash));
                }
            } else if path_changed {
                // FavorRecord does not carry image hashes. Never keep the old
                // image's equality hash after changing its file reference.
                set(&mut item, "ImageHash", json!(""));
                if field(&item, "ImageMD5").is_some() {
                    set(&mut item, "ImageMD5", json!(""));
                }
            }
            copy_text(
                &mut item,
                &rust,
                "PerceptualHash",
                &["perceptualHash", "perceptual_hash"],
            )?;
            if path_changed && find(&rust, &["perceptualHash", "perceptual_hash"]).is_none() {
                set(&mut item, "PerceptualHash", json!(""));
            }
        }
        ClipKind::LinkClipItem => {
            set(&mut item, "Url", json!(body()?.unwrap_or_default()));
            // A link's Item.Title/Icon are distinct from record overrides. Keep
            // existing link metadata; set a title on a new link only.
            if field(&item, "Title").is_none() {
                copy_text(
                    &mut item,
                    &rust,
                    "Title",
                    if favorite {
                        &["name", "title"]
                    } else {
                        &["title"]
                    },
                )?;
            }
        }
        ClipKind::CmdClipItem => {
            set(&mut item, "Command", json!(body()?.unwrap_or_default()));
        }
        ClipKind::StepClipItem => {
            let data = body()?.unwrap_or_default();
            if field(&item, "StepData").and_then(Value::as_str) != Some(data.as_str()) {
                set(
                    &mut item,
                    "StepSummary",
                    json!(crate::quicker_steps::build_summary(&data)),
                );
            }
            set(&mut item, "StepData", json!(data));
        }
        ClipKind::ActionClipItem => {
            let encoded = body()?.ok_or("动作条目缺少 ActionItem。")?;
            let action: Value = serde_json::from_str(&encoded)
                .map_err(|error| format!("动作 JSON 无效：{error}"))?;
            crate::quicker_action::ActionReference::parse(&encoded)?;
            let incoming = action.as_object().ok_or("ActionItem 必须是对象。")?;
            let mut action = match field(&item, "ActionItem") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(object)) => object.clone(),
                _ => return Err("原 ActionItem 必须是对象。".into()),
            };
            for (name, value) in incoming {
                set(&mut action, name, value.clone());
            }
            set(&mut item, "ActionItem", Value::Object(action));
            copy_text(
                &mut item,
                &rust,
                "ExecuteParam",
                &["executeParam", "execute_param"],
            )?;
        }
        ClipKind::FavoriteClipItem => {}
    }
    set(&mut document, "Item", Value::Object(item));
    let document = Value::Object(document);
    // The same parser used for existing C# rows validates new writes before
    // SQLite's trigger can commit an unreadable payload to the original table.
    if favorite {
        parse_favor(&id, &document)?;
    } else {
        parse_clip(&id, &document)?;
    }
    Ok(document)
}

pub(crate) fn parse_usage(id: &str, data: &Value) -> Result<UsageInfo, String> {
    let object = data.as_object().ok_or("使用统计 Data 必须是对象。")?;
    if field(object, "Id").and_then(Value::as_str) != Some(id) {
        return Err(format!("使用统计 {id} 的文档 ID 不一致。"));
    }
    let usage_count = field(object, "UsageCount")
        .and_then(Value::as_i64)
        .filter(|count| (0..=i32::MAX as i64).contains(count))
        .ok_or_else(|| format!("使用统计 {id} 的次数无效。"))? as i32;
    let value = field(object, "LastUseTime").ok_or("使用统计缺少时间。")?;
    let last_use_time = if value
        .as_str()
        .is_some_and(|text| text.starts_with("0001-01-01T00:00:00"))
    {
        None
    } else {
        Some(crate::csharp_import_document::parse_datetime(value)?)
    };
    if last_use_time.is_none() && usage_count != 0 {
        return Err("使用统计有次数但没有有效时间。".into());
    }
    Ok(UsageInfo {
        last_use_time,
        usage_count,
    })
}

pub(crate) fn write_usage(
    id: &str,
    last_use_time: Option<DateTime<Utc>>,
    usage_count: i32,
    previous: Option<&Value>,
) -> Result<Value, String> {
    if id.trim().is_empty() || usage_count < 0 || (last_use_time.is_none() && usage_count != 0) {
        return Err("使用统计 ID、次数或时间无效。".into());
    }
    let mut document = previous_object(previous, "UsageInfo Data")?;
    if let Some(previous_id) = field(&document, "Id").and_then(Value::as_str) {
        if previous_id != id {
            return Err("不能改写不同 ID 的使用统计。".into());
        }
    }
    set(&mut document, "Id", json!(id));
    let date = last_use_time
        .map(|time| json!(time.to_rfc3339()))
        .unwrap_or_else(|| json!("0001-01-01T00:00:00"));
    set(&mut document, "LastUseTime", date);
    set(&mut document, "UsageCount", json!(usage_count));
    Ok(Value::Object(document))
}

fn expanded_record(rust: &Value) -> Result<Map<String, Value>, String> {
    let object = rust.as_object().ok_or("Rust 记录必须是对象。")?;
    let extra = find(object, &["extraJson", "extra_json"]);
    let mut result = match extra {
        None | Some(Value::Null) => Map::new(),
        Some(Value::String(text)) if text.is_empty() => Map::new(),
        Some(Value::String(text)) => serde_json::from_str::<Value>(text)
            .map_err(|error| format!("extra_json 无效：{error}"))?
            .as_object()
            .cloned()
            .ok_or("extra_json 必须是对象。")?,
        Some(Value::Object(value)) => value.clone(),
        _ => return Err("extra_json 必须是 JSON 对象。".into()),
    };
    // SQL column values are authoritative over extra_json's old projections.
    for (name, value) in object {
        if !name.eq_ignore_ascii_case("extra_json") && !name.eq_ignore_ascii_case("extraJson") {
            let counterpart = match name.as_str() {
                "body_text" => Some("bodyText"),
                "html_data" => Some("htmlData"),
                "html_source_url" => Some("htmlSourceUrl"),
                "qq_rich_text" => Some("qqRichText"),
                "file_paths" => Some("filePaths"),
                "image_path" => Some("imagePath"),
                "is_top" => Some("isTop"),
                "sub_index" => Some("subIndex"),
                "create_time" => Some("createTime"),
                "update_time" => Some("updateTime"),
                "source_process" | "source_process_name" => Some("sourceProcessName"),
                "content_hash" => Some("contentHash"),
                "execute_param" => Some("executeParam"),
                "parent_id" => Some("parentId"),
                "is_folder" => Some("isFolder"),
                "source_clip_id" => Some("sourceClipId"),
                "perceptual_hash" => Some("perceptualHash"),
                _ => None,
            };
            if let Some(counterpart) = counterpart {
                result.remove(counterpart);
            }
            result.insert(name.clone(), value.clone());
        }
    }
    Ok(result)
}

fn previous_object(previous: Option<&Value>, name: &str) -> Result<Map<String, Value>, String> {
    match previous {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(object)) => Ok(object.clone()),
        _ => Err(format!("原 {name} 必须是对象。")),
    }
}

fn field<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    object.get(name).or_else(|| {
        object
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    })
}

fn find<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| field(object, name))
}

fn set(object: &mut Map<String, Value>, name: &str, value: Value) {
    let existing = if object.contains_key(name) {
        Some(name.to_string())
    } else {
        object
            .keys()
            .find(|key| key.eq_ignore_ascii_case(name))
            .cloned()
    };
    object.insert(existing.unwrap_or_else(|| name.to_string()), value);
}

fn text(
    object: &Map<String, Value>,
    names: &[&str],
    default: Option<&str>,
) -> Result<Option<String>, String> {
    match find(object, names) {
        None => Ok(default.map(str::to_string)),
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(format!("{} 必须是字符串。", names[0])),
    }
}

fn copy_text(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    name: &str,
    names: &[&str],
) -> Result<(), String> {
    if find(source, names).is_some() {
        set(target, name, json!(text(source, names, None)?));
    }
    Ok(())
}

fn integer(object: &Map<String, Value>, names: &[&str], default: i32) -> Result<i32, String> {
    match find(object, names) {
        None => Ok(default),
        Some(value) => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| format!("{} 必须是 Int32。", names[0])),
    }
}

fn flag(object: &Map<String, Value>, names: &[&str], default: bool) -> Result<bool, String> {
    match find(object, names) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(value) if value.as_i64() == Some(0) => Ok(false),
        Some(value) if value.as_i64() == Some(1) => Ok(true),
        _ => Err(format!("{} 必须是布尔值或 0/1。", names[0])),
    }
}

fn string_array(
    object: &Map<String, Value>,
    names: &[&str],
    default: Vec<String>,
) -> Result<Vec<String>, String> {
    let Some(value) = find(object, names) else {
        return Ok(default);
    };
    let decoded = if let Value::String(text) = value {
        serde_json::from_str::<Value>(text)
            .map_err(|error| format!("{} 数组无效：{error}", names[0]))?
    } else {
        value.clone()
    };
    let array = decoded
        .as_array()
        .ok_or_else(|| format!("{} 必须是字符串数组。", names[0]))?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{} 包含非字符串值。", names[0]))
        })
        .collect()
}

fn set_time(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    name: &str,
    names: &[&str],
    required: bool,
) -> Result<(), String> {
    let Some(value) = find(source, names) else {
        return if required && field(target, name).is_none() {
            Err(format!("Rust 条目缺少 {name}。"))
        } else {
            Ok(())
        };
    };
    let time = if let Some(millis) = value.as_i64() {
        Utc.timestamp_millis_opt(millis)
            .single()
            .ok_or("日期超出支持范围。")?
    } else {
        crate::csharp_import_document::parse_datetime(value)?
    };
    // Preserve original timezone and sub-millisecond precision when the cache
    // describes the same instant. Cache epoch milliseconds are deliberately coarse.
    let unchanged = field(target, name)
        .and_then(|old| crate::csharp_import_document::parse_datetime(old).ok())
        .is_some_and(|old| {
            if value.is_i64() {
                old.timestamp_millis() == time.timestamp_millis()
            } else {
                old == time
            }
        });
    if !unchanged {
        set(target, name, json!(time.to_rfc3339()));
    }
    Ok(())
}

fn kind(name: &str) -> Result<ClipKind, String> {
    match name {
        "TextClipItem" | "text" => Ok(ClipKind::TextClipItem),
        "HtmlClipItem" | "html" => Ok(ClipKind::HtmlClipItem),
        "QQRichTextItem" => Ok(ClipKind::QQRichTextItem),
        "FileClipItem" | "file" | "Directory" | "directory" => Ok(ClipKind::FileClipItem),
        "ImageClipItem" | "image" => Ok(ClipKind::ImageClipItem),
        "LinkClipItem" => Ok(ClipKind::LinkClipItem),
        "CmdClipItem" => Ok(ClipKind::CmdClipItem),
        "StepClipItem" => Ok(ClipKind::StepClipItem),
        "ActionClipItem" => Ok(ClipKind::ActionClipItem),
        "FavoriteClipItem" => Ok(ClipKind::FavoriteClipItem),
        _ => Err(format!("不支持的 Rust 条目类型：{name}。")),
    }
}

fn type_id(kind: ClipKind, directory: bool) -> i32 {
    match kind {
        ClipKind::TextClipItem => 1,
        ClipKind::FileClipItem if directory => 30,
        ClipKind::FileClipItem => 2,
        ClipKind::ImageClipItem => 3,
        ClipKind::HtmlClipItem => 10,
        ClipKind::QQRichTextItem => 11,
        ClipKind::LinkClipItem => 20,
        ClipKind::CmdClipItem => 21,
        ClipKind::StepClipItem => 64,
        ClipKind::ActionClipItem => 65,
        ClipKind::FavoriteClipItem => 100,
    }
}

fn is_directory_type(value: Option<&Value>) -> bool {
    value.is_some_and(|value| {
        value.as_i64() == Some(30)
            || value
                .as_str()
                .is_some_and(|text| text == "30" || text.eq_ignore_ascii_case("directory"))
    })
}

fn image_path(item: &Map<String, Value>) -> Option<String> {
    if let Some(path) = field(item, "ImagePath").and_then(Value::as_str) {
        return Some(path.to_string());
    }
    if let Some(Value::Object(source)) = field(item, "RSource") {
        let relative = field(source, "IsInImageDir")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return field(source, if relative { "RPath" } else { "APath" })
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    field(item, "ImageSource")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn is_absolute_path(path: &str) -> bool {
    // Windows drive/UNC paths also need to work in cross-platform pure tests.
    std::path::Path::new(path).is_absolute()
        || path.starts_with("\\\\")
        || (path.as_bytes().get(1) == Some(&b':')
            && path
                .as_bytes()
                .get(2)
                .is_some_and(|b| *b == b'\\' || *b == b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(item: Value) -> Value {
        json!({"Id":"row", "CreateTime":"2026-01-02T11:04:05.1234567+08:00",
            "UpdateTime":"2026-01-02T12:04:05+08:00", "Title":"旧标题",
            "Description":"备注", "Icon":"custom:record", "IsTop":true,
            "SubIndex":7,"SourceProcessName":"app.exe","Item":item,
            "FutureMetadata":{"retain":[1,2,3]}})
    }

    #[test]
    fn csharp_store_document_edits_preserve_unknown_metadata_case_and_date_precision() {
        let previous = document(json!({"ObjType":1,"Text":"原文","FutureItem":{"nested":true}}));
        let mut record = parse_clip("row", &previous).unwrap();
        record.body_text = Some("改后的中文🦀\r\n文本".into());
        record.title = Some("改标题".into());
        let written = write_document(
            "clip",
            &serde_json::to_value(&record).unwrap(),
            Some(&previous),
        )
        .unwrap();
        assert_eq!(written["FutureMetadata"], previous["FutureMetadata"]);
        assert_eq!(
            written["Item"]["FutureItem"],
            previous["Item"]["FutureItem"]
        );
        assert_eq!(written["CreateTime"], previous["CreateTime"]);
        assert_eq!(written["Icon"], previous["Icon"]);
        assert_eq!(
            parse_clip("row", &written).unwrap().body_text,
            record.body_text
        );
        let mut lower = written.clone();
        let value = lower.as_object_mut().unwrap().remove("Title").unwrap();
        lower["title"] = value;
        let updated = write_document(
            "clip_item",
            &serde_json::to_value(record).unwrap(),
            Some(&lower),
        )
        .unwrap();
        assert!(updated.get("Title").is_none());
        assert_eq!(updated["title"], "改标题");
    }

    #[test]
    fn csharp_store_document_sql_columns_override_extra_json_and_keep_unchanged_time() {
        let previous = document(json!({"ObjType":1,"Text":"旧正文"}));
        let record = parse_clip("row", &previous).unwrap();
        let sql = json!({"id":"row","kind":"TextClipItem","body_text":"新正文",
            "create_time":record.create_time.timestamp_millis(),"update_time":record.update_time.timestamp_millis(),
            "is_top":0,"extra_json":serde_json::to_string(&record).unwrap()});
        let written = write_document("clip", &sql, Some(&previous)).unwrap();
        assert_eq!(written["Item"]["Text"], "新正文");
        assert_eq!(written["IsTop"], false);
        assert_eq!(written["CreateTime"], previous["CreateTime"]);
    }

    #[test]
    fn csharp_store_document_all_value_kinds_can_be_written_and_read_back() {
        for item in [
            json!({"ObjType":1,"Text":"文本"}),
            json!({"ObjType":2,"FileList":["C:\\file.txt","D:\\second.txt"]}),
            json!({"ObjType":3,"ImagePath":"Temp/image.png","ImageHash":"hash","PerceptualHash":"1010"}),
            json!({"ObjType":10,"Text":"rich","HtmlData":"<b>rich</b>","HtmlSourceUrl":"https://source.test/"}),
            json!({"ObjType":11,"Text":"qq","HtmlData":"<i>qq</i>","QQRichText":"<QQ>value</QQ>"}),
            json!({"ObjType":20,"Url":"https://link.test/","Title":"链接标题","Icon":"custom:link"}),
            json!({"ObjType":21,"Command":"cmd /c echo hello"}),
            json!({"ObjType":64,"StepData":"{\"steps\":[]}","StepSummary":"缓存摘要"}),
            json!({"ObjType":65,"ActionItem":{"Id":"action","Title":"动作","SharedActionId":"shared","FutureAction":3},"ExecuteParam":"参数"}),
        ] {
            let previous = document(item);
            let record = parse_clip("row", &previous).unwrap();
            let written = write_document(
                "clip",
                &serde_json::to_value(&record).unwrap(),
                Some(&previous),
            )
            .unwrap();
            let parsed = parse_clip("row", &written).unwrap();
            assert_eq!(parsed.kind, record.kind);
            assert_eq!(parsed.body_text, record.body_text);
            assert_eq!(parsed.image_path, record.image_path);
            assert_eq!(parsed.file_paths, record.file_paths);
            assert_eq!(parsed.html_data, record.html_data);
            assert_eq!(parsed.qq_rich_text, record.qq_rich_text);
            assert_eq!(
                written["Item"]["StepSummary"],
                previous["Item"]["StepSummary"]
            );
            assert_eq!(written["Item"]["Icon"], previous["Item"]["Icon"]);
        }
        let fresh = ClipRecord::new_text("首次复制", "fixture.exe");
        let written = write_document("clip", &serde_json::to_value(&fresh).unwrap(), None).unwrap();
        assert_eq!(
            parse_clip(&fresh.id, &written).unwrap().body_text,
            fresh.body_text
        );
    }

    #[test]
    fn csharp_store_document_directory_identity_and_legacy_image_aliases_survive_edits() {
        let previous = document(json!({"Type":"Directory","ObjType":30,"FileList":["C:\\folder"]}));
        let record = parse_clip("row", &previous).unwrap();
        let written = write_document(
            "clip",
            &serde_json::to_value(record).unwrap(),
            Some(&previous),
        )
        .unwrap();
        assert_eq!(written["Item"]["ObjType"], 30);
        assert_eq!(written["Item"]["Type"], 30);
        let previous = document(
            json!({"ObjType":3,"RSource":{"IsInImageDir":true,"RPath":"old.png","FuturePath":9},"ImageMD5":"oldhash"}),
        );
        let mut record = parse_clip("row", &previous).unwrap();
        record.image_path = Some("C:\\external\\new.png".into());
        record.content_hash = "newhash".into();
        let written = write_document(
            "clip",
            &serde_json::to_value(&record).unwrap(),
            Some(&previous),
        )
        .unwrap();
        assert_eq!(written["Item"]["RSource"]["IsInImageDir"], false);
        assert_eq!(written["Item"]["RSource"]["APath"], "C:\\external\\new.png");
        assert_eq!(written["Item"]["RSource"]["FuturePath"], 9);
        assert_eq!(written["Item"]["ImageMD5"], "newhash");
        assert_eq!(
            parse_clip("row", &written).unwrap().image_path,
            record.image_path
        );
    }

    #[test]
    fn csharp_store_document_favorites_preserve_tree_and_unknown_non_modeled_metadata() {
        let mut previous = document(json!({"ObjType":100,"FutureItem":true}));
        previous["Children"] = json!(["second", "first"]);
        previous["ParentId"] = json!("parent");
        let mut record = parse_favor("row", &previous).unwrap();
        record.children.reverse();
        record.parent_id = Some("newparent".into());
        record.name = "新收藏夹".into();
        let written = write_document(
            "favor_item",
            &serde_json::to_value(&record).unwrap(),
            Some(&previous),
        )
        .unwrap();
        let parsed = parse_favor("row", &written).unwrap();
        assert_eq!(parsed.children, ["first", "second"]);
        assert_eq!(parsed.parent_id.as_deref(), Some("newparent"));
        assert_eq!(parsed.name, "新收藏夹");
        assert_eq!(written["Description"], previous["Description"]);
        assert_eq!(written["IsTop"], previous["IsTop"]);
        assert_eq!(written["FutureMetadata"], previous["FutureMetadata"]);
        assert_eq!(written["Item"]["FutureItem"], true);
    }

    #[test]
    fn csharp_store_document_runtime_projects_leaf_children_without_changing_source() {
        let mut previous = document(json!({"ObjType":1,"Text":"保留正文"}));
        previous["Children"] = json!(["retained-child", "second-child"]);
        let original = serde_json::to_string(&previous).unwrap();
        // Offline imports and new-format migrations must still flag this graph.
        assert!(crate::csharp_import_document::parse_favor("row", &previous).is_err());
        let record = parse_favor("row", &previous).unwrap();
        assert!(!record.is_folder);
        assert!(record.children.is_empty());
        assert_eq!(record.body_text.as_deref(), Some("保留正文"));
        assert_eq!(serde_json::to_string(&previous).unwrap(), original);

        for children in [json!([7]), json!({"child":true}), json!("child")] {
            previous["Children"] = children;
            assert!(parse_favor("row", &previous).is_err());
        }
        previous["Children"] = json!(["child"]);
        previous["Item"]["ObjType"] = json!(9999);
        assert!(parse_favor("row", &previous).is_err());
    }

    #[test]
    fn csharp_store_document_leaf_edits_preserve_residual_children_and_reject_new_edges() {
        let mut previous = document(json!({"ObjType":20,"Url":"https://example.test/","Title":"原网页标题","FutureItem":9}));
        // Preserve the spelling as well as the value of legacy extension data.
        previous["children"] = json!(["retained-child", "second-child"]);
        let mut record = parse_favor("row", &previous).unwrap();
        record.name = "更新标题".into();
        record.icon = "updated:icon".into();
        let written = write_document(
            "favor_item", &serde_json::to_value(&record).unwrap(), Some(&previous),
        ).unwrap();
        assert_eq!(written["children"], previous["children"]);
        assert!(written.get("Children").is_none());
        assert_eq!(written["Item"], previous["Item"]);
        assert_eq!(written["FutureMetadata"], previous["FutureMetadata"]);
        let read = parse_favor("row", &written).unwrap();
        assert_eq!(read.name, "更新标题");
        assert_eq!(read.icon, "updated:icon");
        assert!(read.children.is_empty());

        record.children = vec!["new-edge".into()];
        let invalid = serde_json::to_value(&record).unwrap();
        assert!(write_document("favor_item", &invalid, Some(&previous)).is_err());
        assert!(write_document("favor_item", &invalid, None).is_err());
    }

    #[test]
    fn csharp_store_document_folder_to_leaf_conversion_does_not_retain_active_edges() {
        let mut previous = document(json!({"ObjType":100}));
        previous["Children"] = json!(["former-child"]);
        let mut record = parse_favor("row", &previous).unwrap();
        record.kind = ClipKind::TextClipItem;
        record.is_folder = false;
        record.children.clear();
        record.body_text = Some("转换后正文".into());
        let written = write_document(
            "favor_item", &serde_json::to_value(&record).unwrap(), Some(&previous),
        ).unwrap();
        assert_eq!(written["Children"], json!([]));
        let parsed = crate::csharp_import_document::parse_favor("row", &written).unwrap();
        assert!(!parsed.is_folder);
        assert!(parsed.children.is_empty());
    }

    #[test]
    fn csharp_store_document_action_extension_data_survives_partial_action_edits() {
        let previous = document(
            json!({"ObjType":65,"ActionItem":{"Id":"action","Title":"旧标题","TemplateId":"template","FutureAction":7},"ExecuteParam":"old"}),
        );
        let mut record = parse_clip("row", &previous).unwrap();
        record.body_text = Some("{\"Id\":\"action\",\"Title\":\"新标题\"}".into());
        record.execute_param = "new".into();
        let written = write_document(
            "clip",
            &serde_json::to_value(record).unwrap(),
            Some(&previous),
        )
        .unwrap();
        assert_eq!(written["Item"]["ActionItem"]["Title"], "新标题");
        assert_eq!(written["Item"]["ActionItem"]["TemplateId"], "template");
        assert_eq!(written["Item"]["ActionItem"]["FutureAction"], 7);
        assert_eq!(written["Item"]["ExecuteParam"], "new");
    }

    #[test]
    fn csharp_store_document_usage_matches_csharp_defaults_and_preserves_extensions() {
        let previous = json!({"id":"clip_item_row","lastUseTime":"0001-01-01T00:00:00","usageCount":0,"FutureUsage":{"retain":true}});
        assert!(parse_usage("clip_item_row", &previous)
            .unwrap()
            .last_use_time
            .is_none());
        let time = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let written = write_usage("clip_item_row", Some(time), 2, Some(&previous)).unwrap();
        let parsed = parse_usage("clip_item_row", &written).unwrap();
        assert_eq!(parsed.last_use_time, Some(time));
        assert_eq!(parsed.usage_count, 2);
        assert_eq!(written["FutureUsage"], previous["FutureUsage"]);
        assert!(written.get("UsageCount").is_none());
        let blank = write_usage("favor_item_row", None, 0, None).unwrap();
        assert!(parse_usage("favor_item_row", &blank)
            .unwrap()
            .last_use_time
            .is_none());
        assert!(write_usage("favor_item_row", None, 1, None).is_err());
        assert!(write_usage("favor_item_row", Some(time), -1, None).is_err());
    }

    #[test]
    fn csharp_store_document_rejects_invalid_writes_instead_of_replacing_previous_data() {
        let previous = document(json!({"ObjType":1,"Text":"retain"}));
        for invalid in [
            json!({"id":"different","kind":"TextClipItem","create_time":0}),
            json!({"id":"row","kind":"unknown","create_time":0}),
            json!({"id":"row","kind":"ImageClipItem","create_time":0,"image_path":""}),
            json!({"id":"row","kind":"FileClipItem","create_time":0,"file_paths":"[7]"}),
            json!({"id":"row","kind":"ActionClipItem","create_time":0,"body_text":"not json"}),
        ] {
            assert!(write_document("clip", &invalid, Some(&previous)).is_err());
        }
        assert_eq!(previous["Item"]["Text"], "retain");
    }
}
