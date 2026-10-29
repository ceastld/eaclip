//! Pure conversion of C# `JsonItemWrapper.Data` documents. The import coordinator
//! archives the complete source JSON separately: fields absent from the current
//! Rust records must not be mistaken for metadata that C# never stored.

use chrono::{DateTime, Local, LocalResult, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

use crate::model::{hash_files, hash_text, ClipKind, ClipRecord, FavorRecord};

pub(crate) fn parse_clip(id: &str, document: &Value) -> Result<ClipRecord, String> {
    require_object(document, "Data")?;
    if id.trim().is_empty() {
        return Err("C# 条目 ID 为空。".into());
    }
    let document_id = string(document, "Id")?.ok_or("C# Data 缺少 Id。")?;
    if document_id != id {
        return Err(format!("C# 条目 ID 与数据库行不一致：{id} / {document_id}。"));
    }
    let payload = field(document, "Item").ok_or("C# Data 缺少 Item。")?;
    require_object(payload, "Item")?;
    let kind = parse_kind(payload)?;
    let mut record = ClipRecord::new_text("", "");
    record.id = id.to_string();
    record.kind = kind;
    record.title = string(document, "Title")?;
    record.description = string(document, "Description")?.unwrap_or_default();
    record.source_process_name = string(document, "SourceProcessName")?.unwrap_or_default();
    record.is_top = boolean(document, "IsTop")?.unwrap_or(false);
    record.sub_index = integer(document, "SubIndex")?.unwrap_or(0);
    record.create_time = parse_datetime(
        field(document, "CreateTime").ok_or("C# Data 缺少 CreateTime。")?,
    )?;
    record.update_time = match field(document, "UpdateTime").filter(|v| !v.is_null()) {
        Some(value) => parse_datetime(value)?,
        None => record.create_time,
    };
    record.body_text = None;
    record.content_hash.clear();

    match kind {
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
            record.body_text = Some(string(payload, "Text")?.unwrap_or_default());
            if kind != ClipKind::TextClipItem {
                record.html_data = string(payload, "HtmlData")?;
                record.html_source_url = string(payload, "HtmlSourceUrl")?;
                // Keep old complete documents intact; source metadata can be
                // backfilled without destroying their head/style context.
                if let Some(source) = record.html_data.as_deref()
                    .and_then(crate::cf_html::try_parse_source_url)
                {
                    if record.html_source_url.as_deref().unwrap_or("").is_empty() {
                        record.html_source_url = Some(source);
                    }
                }
            }
            if kind == ClipKind::QQRichTextItem {
                record.qq_rich_text = string(payload, "QQRichText")?;
                record.content_hash = hash_text(&format!(
                    "qq:{}", record.qq_rich_text.as_deref().unwrap_or("")
                ));
            } else {
                record.content_hash = hash_text(record.body_text.as_deref().unwrap_or(""));
            }
        }
        ClipKind::FileClipItem => {
            record.file_paths = string_list(payload, "FileList")?;
            record.content_hash = hash_files(&record.file_paths);
        }
        ClipKind::ImageClipItem => {
            record.image_path = image_path(payload)?;
            if record.image_path.as_deref().is_none_or(|path| path.trim().is_empty()) {
                return Err("C# 图片条目缺少图片路径。".into());
            }
            record.content_hash = string(payload, "ImageHash")?
                .or(string(payload, "ImageMD5")?).unwrap_or_default();
            record.perceptual_hash = string(payload, "PerceptualHash")?.unwrap_or_default();
        }
        ClipKind::LinkClipItem => {
            record.body_text = Some(string(payload, "Url")?.unwrap_or_default());
            if record.title.is_none() {
                record.title = string(payload, "Title")?;
            }
            record.content_hash = hash_text(record.body_text.as_deref().unwrap_or(""));
        }
        ClipKind::CmdClipItem => {
            record.body_text = Some(string(payload, "Command")?.unwrap_or_default());
            record.content_hash = hash_text(record.body_text.as_deref().unwrap_or(""));
        }
        ClipKind::StepClipItem => {
            record.body_text = Some(string(payload, "StepData")?.unwrap_or_default());
            record.content_hash = hash_text(record.body_text.as_deref().unwrap_or(""));
        }
        ClipKind::ActionClipItem => {
            let action = field(payload, "ActionItem").ok_or("C# 动作条目缺少 ActionItem。")?;
            require_object(action, "ActionItem")?;
            let serialized = serde_json::to_string(action).map_err(|e| e.to_string())?;
            // The existing action reader accepts SimpleActionItem JSON and
            // retains its title, description, icon and shared/template IDs.
            crate::quicker_action::ActionReference::parse(&serialized)?;
            record.content_hash = hash_text(&serialized);
            record.body_text = Some(serialized);
            record.execute_param = string(payload, "ExecuteParam")?.unwrap_or_default();
        }
        ClipKind::FavoriteClipItem => {}
    }
    Ok(record)
}

pub(crate) fn parse_favor(id: &str, document: &Value) -> Result<FavorRecord, String> {
    let clip = parse_clip(id, document)?;
    let parent_id = string(document, "ParentId")?.filter(|id| !id.is_empty());
    let mut favor = FavorRecord::from_clip(&clip, parent_id);
    favor.id = clip.id.clone();
    favor.is_folder = clip.kind == ClipKind::FavoriteClipItem;
    favor.name = string(document, "Title")?.unwrap_or_else(|| clip.csharp_visual_name());
    favor.children = string_list(document, "Children")?;
    if !favor.is_folder && !favor.children.is_empty() {
        return Err(format!("C# 非收藏夹条目包含 Children：{id}。"));
    }
    favor.create_time = clip.create_time;
    favor.update_time = clip.update_time;
    favor.sub_index = clip.sub_index;
    favor.source_clip_id = string(document, "SourceClipId")?;
    favor.icon = string(document, "Icon")?.unwrap_or_default();
    if favor.icon.is_empty() && clip.kind == ClipKind::LinkClipItem {
        // Link's custom icon is stored inside Item, unlike the record override.
        favor.icon = string(field(document, "Item").unwrap(), "Icon")?.unwrap_or_default();
    }
    Ok(favor)
}

/// Newtonsoft DateTime rows use ISO timestamps with `Z`, an explicit offset, or
/// no suffix for DateTimeKind.Local/Unspecified. Never reinterpret the latter
/// as UTC. This also accepts .NET's year-0001 minimum timestamp.
pub(crate) fn parse_datetime(value: &Value) -> Result<DateTime<Utc>, String> {
    let text = value.as_str().ok_or("C# 日期必须是 ISO 字符串。")?;
    if let Ok(date) = DateTime::parse_from_rfc3339(text) {
        return Ok(date.with_timezone(&Utc));
    }
    let local = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter().find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
        .ok_or_else(|| format!("C# 日期无效：{text}。"))?;
    match Local.from_local_datetime(&local) {
        LocalResult::Single(date) => Ok(date.with_timezone(&Utc)),
        // The document has no offset to identify either occurrence. Refuse to
        // silently change a persisted timestamp by guessing a DST occurrence.
        LocalResult::Ambiguous(_, _) => Err(format!("C# 本地日期有夏令时歧义：{text}。")),
        LocalResult::None => Err(format!("C# 本地日期无法转换：{text}。")),
    }
}

fn parse_kind(payload: &Value) -> Result<ClipKind, String> {
    let value = field(payload, "ObjType").or_else(|| field(payload, "Type"))
        .ok_or("C# Item 缺少 ObjType。")?;
    let type_id = if let Some(number) = value.as_i64() {
        number
    } else if let Some(text) = value.as_str() {
        match text.trim().to_ascii_lowercase().as_str() {
            "text" | "textclipitem" => 1,
            "file" | "fileclipitem" => 2,
            "image" | "imageclipitem" => 3,
            "html" | "htmlclipitem" => 10,
            "qqrichtext" | "qqrichtextitem" => 11,
            "link" | "linkclipitem" => 20,
            "cmd" | "cmdclipitem" => 21,
            "directory" => 30,
            "quickeractionsteps" | "stepclipitem" => 64,
            "quickeraction" | "actionclipitem" => 65,
            "favorites" | "favoriteclipitem" => 100,
            other => other.parse().map_err(|_| format!("不支持的 C# ObjType：{value}。"))?,
        }
    } else {
        return Err(format!("C# ObjType 无效：{value}。"));
    };
    match type_id {
        1 => Ok(ClipKind::TextClipItem),
        2 | 30 => Ok(ClipKind::FileClipItem),
        3 => Ok(ClipKind::ImageClipItem),
        10 => Ok(ClipKind::HtmlClipItem),
        11 => Ok(ClipKind::QQRichTextItem),
        20 => Ok(ClipKind::LinkClipItem),
        21 => Ok(ClipKind::CmdClipItem),
        64 => Ok(ClipKind::StepClipItem),
        65 => Ok(ClipKind::ActionClipItem),
        100 => Ok(ClipKind::FavoriteClipItem),
        _ => Err(format!("不支持的 C# ObjType：{value}。")),
    }
}

fn image_path(payload: &Value) -> Result<Option<String>, String> {
    if field(payload, "ImagePath").is_some() {
        return string(payload, "ImagePath");
    }
    if let Some(source) = field(payload, "RSource").filter(|v| !v.is_null()) {
        require_object(source, "RSource")?;
        return string(source, if boolean(source, "IsInImageDir")?.unwrap_or(false) {
            "RPath"
        } else {
            "APath"
        });
    }
    string(payload, "ImageSource")
}

fn field<'a>(object: &'a Value, name: &str) -> Option<&'a Value> {
    object.get(name).or_else(|| object.as_object()?.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value))
}

fn require_object(value: &Value, name: &str) -> Result<(), String> {
    if value.is_object() { Ok(()) } else { Err(format!("C# {name} 必须是对象。")) }
}

fn string(object: &Value, name: &str) -> Result<Option<String>, String> {
    match field(object, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(format!("C# {name} 必须是字符串。")),
    }
}

fn boolean(object: &Value, name: &str) -> Result<Option<bool>, String> {
    match field(object, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(format!("C# {name} 必须是布尔值。")),
    }
}

fn integer(object: &Value, name: &str) -> Result<Option<i32>, String> {
    match field(object, name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_i64().and_then(|v| i32::try_from(v).ok())
            .map(Some).ok_or_else(|| format!("C# {name} 必须是 Int32。")),
    }
}

fn string_list(object: &Value, name: &str) -> Result<Vec<String>, String> {
    match field(object, name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(|value| value.as_str()
            .map(str::to_string).ok_or_else(|| format!("C# {name} 包含非字符串项。"))).collect(),
        _ => Err(format!("C# {name} 必须是字符串数组。")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(item: Value) -> Value {
        json!({"Id":"record", "CreateTime":"2026-01-02T03:04:05Z", "Item":item})
    }

    #[test]
    fn html_preservation_import_does_not_normalize_old_complete_documents() {
        let raw = crate::cf_html::build_clipboard_html("<html xmlns:wps='urn:wps'><head><style>table{color:red}</style></head><body>中文😀</body></html>", Some("https://source.test/"));
        let item = parse_clip("record", &row(json!({"ObjType":10,"Text":"中文😀","HtmlData":raw}))).unwrap();
        assert_eq!(item.html_data.as_deref(), Some(raw.as_str()));
        assert_eq!(item.html_source_url.as_deref(), Some("https://source.test/"));
    }

    #[test]
    fn csharp_import_document_existing_wire_records_preserve_metadata_and_rich_sources() {
        let history: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/clip-item-wire/history-record.json"
        )).unwrap();
        let clip = parse_clip("history-record-contract", &history).unwrap();
        assert_eq!(clip.body_text.as_deref(), Some("shared history payload"));
        assert_eq!(clip.title.as_deref(), Some("persisted title"));
        assert_eq!(clip.description, "persisted description");
        assert_eq!(clip.source_process_name, "contract-source");
        assert!(clip.is_top);
        assert_eq!(clip.sub_index, 7);
        assert_eq!(clip.update_time, parse_datetime(&json!("2026-01-02T04:05:06Z")).unwrap());

        let text: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/clip-item-wire/capture-text.json"
        )).unwrap();
        let text = parse_clip("handoff-text", &text).unwrap();
        assert_eq!(text.body_text.as_deref(), Some("same text"));
        assert_eq!(text.create_time, parse_datetime(&json!("2026-09-25T04:00:00Z")).unwrap());
        assert_eq!(text.update_time, text.create_time);

        let html: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/clip-item-wire/capture-html.json"
        )).unwrap();
        let html = parse_clip("handoff-html", &html).unwrap();
        assert_eq!(html.kind, ClipKind::HtmlClipItem);
        assert_eq!(html.html_data.as_deref(), Some("<b>rich text</b>"));
        assert_eq!(html.html_source_url.as_deref(), Some("https://example.test/a"));

        let mut image: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/clip-item-wire/capture-image.json"
        )).unwrap();
        image["Item"]["ImagePath"] = json!("clips/handoff-image.png");
        let image = parse_clip("handoff-image", &image).unwrap();
        assert_eq!(image.image_path.as_deref(), Some("clips/handoff-image.png"));
        assert_eq!(image.content_hash, "ABCD");
    }

    #[test]
    fn csharp_import_document_remaining_value_kinds_and_legacy_aliases_are_explicit() {
        let directory = row(serde_json::from_str(include_str!(
            "../../../../tests/fixtures/clip-item-wire/directory.json"
        )).unwrap());
        let directory = parse_clip("record", &directory).unwrap();
        assert_eq!(directory.kind, ClipKind::FileClipItem);
        assert_eq!(directory.file_paths, [r"C:\work"]);
        let steps = row(serde_json::from_str(include_str!(
            "../../../../tests/fixtures/clip-item-wire/quicker-steps.json"
        )).unwrap());
        assert_eq!(parse_clip("record", &steps).unwrap().body_text.as_deref(), Some("serialized-action-steps"));
        let qq = parse_clip("record", &row(json!({"Type":"QQRichText", "Text":"plain",
            "HtmlData":"<b>plain</b>", "HtmlSourceUrl":"https://source.test/", "QQRichText":"envelope"}))).unwrap();
        assert_eq!(qq.kind, ClipKind::QQRichTextItem);
        assert_eq!(qq.qq_rich_text.as_deref(), Some("envelope"));
        assert_eq!(qq.html_source_url.as_deref(), Some("https://source.test/"));
        let link = parse_clip("record", &row(json!({"objType":"20", "Url":"https://link.test/", "Title":"link title"}))).unwrap();
        assert_eq!(link.kind, ClipKind::LinkClipItem);
        assert_eq!(link.body_text.as_deref(), Some("https://link.test/"));
        assert_eq!(link.title.as_deref(), Some("link title"));
        let command = parse_clip("record", &row(json!({"ObjType":"cMd", "Command":"echo 中文"}))).unwrap();
        assert_eq!(command.kind, ClipKind::CmdClipItem);
        assert_eq!(command.body_text.as_deref(), Some("echo 中文"));
        let image = parse_clip("record", &row(json!({"ObjType":3,
            "RSource":{"IsInImageDir":true,"RPath":"old/image.png"}, "ImageMD5":"legacy-hash", "PerceptualHash":"1010"}))).unwrap();
        assert_eq!(image.image_path.as_deref(), Some("old/image.png"));
        assert_eq!(image.content_hash, "legacy-hash");
        assert_eq!(image.perceptual_hash, "1010");
        let file = parse_clip("record", &row(json!({"ObjType":"File", "FileList":["a", "b"]}))).unwrap();
        assert_eq!(file.file_paths, ["a", "b"]);
    }

    #[test]
    fn csharp_import_document_favorites_preserve_tree_order_and_action_payload() {
        let mut folder = row(json!({"ObjType":100}));
        folder["Title"] = json!("我的收藏");
        folder["Icon"] = json!("custom:folder");
        folder["Children"] = json!(["second", "first"]);
        folder["ParentId"] = json!("parent");
        folder["SubIndex"] = json!(9);
        let folder = parse_favor("record", &folder).unwrap();
        assert!(folder.is_folder);
        assert_eq!(folder.children, ["second", "first"]);
        assert_eq!(folder.parent_id.as_deref(), Some("parent"));
        assert_eq!(folder.name, "我的收藏");
        assert_eq!(folder.icon, "custom:folder");
        assert_eq!(folder.sub_index, 9);
        assert_eq!(folder.source_clip_id, None);

        let action = json!({"Id":"action-id", "Title":"original action", "Description":"details",
            "Icon":"action:icon", "TemplateId":"template", "SharedActionId":"shared"});
        let mut document = row(json!({"ObjType":"QuickerAction", "ActionItem":action, "ExecuteParam":"a\nb"}));
        document["Title"] = json!("用户标题");
        let favor = parse_favor("record", &document).unwrap();
        assert_eq!(favor.name, "用户标题");
        assert_eq!(favor.execute_param, "a\nb");
        assert_eq!(serde_json::from_str::<Value>(favor.body_text.as_deref().unwrap()).unwrap(), action);
        assert_eq!(favor.item_get_icon(), "action:icon");
    }

    #[test]
    fn csharp_import_document_datetime_uses_offsets_local_time_and_dotnet_minimum() {
        let utc = parse_datetime(&json!("2026-01-02T03:04:05.123Z")).unwrap();
        assert_eq!(parse_datetime(&json!("2026-01-02T11:04:05.1230000+08:00")).unwrap(), utc);
        let local = NaiveDateTime::parse_from_str("2026-01-02T03:04:05.123", "%Y-%m-%dT%H:%M:%S%.f").unwrap();
        assert_eq!(parse_datetime(&json!("2026-01-02T03:04:05.123")).unwrap(), Local.from_local_datetime(&local).latest().unwrap().with_timezone(&Utc));
        assert_eq!(parse_datetime(&json!("0001-01-01T00:00:00Z")).unwrap().to_rfc3339(), "0001-01-01T00:00:00+00:00");
        assert!(parse_datetime(&json!("not-a-date")).is_err());
    }

    #[test]
    fn csharp_import_document_rejects_unknown_types_and_malformed_values() {
        for discriminator in [json!(0), json!(999), json!("FutureClipItem"), json!(false)] {
            assert!(parse_clip("record", &row(json!({"ObjType":discriminator, "Text":"do not downgrade"}))).is_err());
        }
        assert!(parse_clip("record", &row(json!({"Text":"missing discriminator"}))).is_err());
        assert!(parse_clip("record", &row(json!({"ObjType":2, "FileList":["a", 1]}))).is_err());
        assert!(parse_clip("other-id", &row(json!({"ObjType":1, "Text":"a"}))).is_err());
    }
}
