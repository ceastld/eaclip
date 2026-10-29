//! C# `ClipFilterCatalog` / `ClipFilterItemTypes`.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::dto::ClipFilterItemDto;
use crate::model::{ClipKind, ClipRecord};

pub const ALL: i32 = 0x1;
pub const TEXT: i32 = 0x2;
pub const IMAGE: i32 = 0x4;
pub const FILE: i32 = 0x8;
pub const STEP: i32 = 0x10;
pub const PLAIN_TEXT: i32 = 0x20;
pub const RICH_TEXT: i32 = 0x40;
pub const URL: i32 = 0x80;

pub const TYPE_ALL: &str = "all";
pub const TYPE_TEXT: &str = "text";
pub const TYPE_PLAIN_TEXT: &str = "plaintext";
pub const TYPE_RICH_TEXT: &str = "richtext";
pub const TYPE_IMAGE: &str = "image";
pub const TYPE_FILE: &str = "file";
pub const TYPE_URL: &str = "url";
pub const TYPE_OTHER: &str = "other";
pub const TYPE_STAR: &str = "star";
const MASK_PREFIX: &str = "mask:";

pub fn create_default_tabs() -> Vec<ClipFilterItemDto> {
    vec![
        new_tab("", TYPE_ALL, ALL),
        new_tab("", TYPE_TEXT, TEXT),
        new_tab("", TYPE_RICH_TEXT, RICH_TEXT),
        new_tab("", TYPE_IMAGE, IMAGE),
        new_tab("", TYPE_FILE, FILE),
        new_tab("", TYPE_URL, URL),
    ]
}

pub fn new_tab(display_name: &str, type_key: &str, item_type: i32) -> ClipFilterItemDto {
    ClipFilterItemDto {
        id: Uuid::new_v4().simple().to_string(),
        display_name: display_name.to_string(),
        r#type: type_key.to_string(),
        item_type,
        star_type: 0,
        icon: String::new(),
    }
}

/// C# `ClipFilterCatalog.ParseFormValue`.
pub fn parse_form_value(value: &Value) -> Vec<ClipFilterItemDto> {
    match value {
        Value::Null => create_default_tabs(),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return create_default_tabs();
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(inner) => parse_form_value(&inner),
                Err(_) => create_default_tabs(),
            }
        }
        Value::Array(rows) => {
            let mut tabs = Vec::new();
            for row in rows {
                let Some(obj) = row.as_object() else {
                    continue;
                };
                tabs.push(ClipFilterItemDto {
                    id: read_string(obj, "id", "Id"),
                    display_name: read_string(obj, "displayName", "DisplayName"),
                    r#type: read_string(obj, "type", "Type"),
                    item_type: read_i32(obj, "itemType", "ItemType"),
                    star_type: read_i32(obj, "starType", "StarType"),
                    icon: read_string(obj, "icon", "Icon"),
                });
            }
            normalize(tabs)
        }
        _ => create_default_tabs(),
    }
}

/// C# `ClipFilterCatalog.SerializeFormValue` — camelCase JSON array string, no `icon`.
pub fn serialize_form_value(tabs: &[ClipFilterItemDto]) -> String {
    let normalized = normalize(tabs.to_vec());
    let rows: Vec<Value> = normalized
        .iter()
        .map(|tab| {
            json!({
                "id": tab.id,
                "displayName": tab.display_name,
                "type": tab.r#type,
                "itemType": tab.item_type,
                "starType": tab.star_type,
            })
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
}

/// C# `ClipFilterCatalog.Normalize`.
pub fn normalize(source: Vec<ClipFilterItemDto>) -> Vec<ClipFilterItemDto> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in source {
        if is_legacy_star_or_pinned(&raw) {
            continue;
        }
        let mut tab = repair(raw);
        if tab.id.trim().is_empty() || !seen.insert(tab.id.clone()) {
            tab.id = Uuid::new_v4().simple().to_string();
            seen.insert(tab.id.clone());
        }
        result.push(tab);
    }
    if result.is_empty() {
        create_default_tabs()
    } else {
        result
    }
}

fn is_legacy_star_or_pinned(item: &ClipFilterItemDto) -> bool {
    let type_key = item.r#type.trim();
    if item.star_type != 0 || type_key.eq_ignore_ascii_case(TYPE_STAR) {
        return true;
    }
    // Names are user content once a type is specified. Only untyped legacy
    // entries may use these names to identify the removed built-in filters.
    !is_known_type_key(type_key)
        && item.item_type == 0
        && matches!(item.display_name.trim(), "星标" | "无星标" | "置顶")
}

/// Normalize type metadata while retaining optional user names. An empty name
/// means the renderer supplies a localized type label; it is never persisted.
pub fn repair(item: ClipFilterItemDto) -> ClipFilterItemDto {
    let name = item.display_name.trim().to_string();
    let mut type_key = item.r#type.trim().to_string();
    let mut item_type = item.item_type;

    if is_known_type_key(&type_key) {
        item_type = to_item_type(&type_key);
        type_key = normalize_type_key(&type_key);
    } else if item_type != 0 {
        type_key = to_type_key(item_type);
    } else if let Some((mapped_type, mapped_item)) = map_display_name(&name) {
        // Compatibility with old name-only records, never a type override.
        item_type = mapped_item;
        type_key = mapped_type.to_string();
    } else {
        item_type = ALL;
        type_key = if type_key.is_empty() {
            TYPE_ALL.to_string()
        } else {
            normalize_type_key(&type_key)
        };
    }

    ClipFilterItemDto {
        id: item.id.trim().to_string(),
        display_name: name,
        r#type: type_key,
        item_type,
        star_type: 0,
        icon: item.icon,
    }
}

pub fn is_known_type_key(type_key: &str) -> bool {
    if type_key.trim().is_empty() {
        return false;
    }
    let key = type_key.trim().to_ascii_lowercase();
    matches!(
        key.as_str(),
        TYPE_ALL
            | "default"
            | TYPE_TEXT
            | "plain"
            | TYPE_PLAIN_TEXT
            | "rich"
            | TYPE_RICH_TEXT
            | "html"
            | TYPE_IMAGE
            | TYPE_FILE
            | TYPE_URL
            | TYPE_OTHER
            | "step"
            | TYPE_STAR
    ) || key.starts_with(MASK_PREFIX)
}

pub fn to_type_key(item_type: i32) -> String {
    match item_type {
        ALL => TYPE_ALL.to_string(),
        TEXT => TYPE_TEXT.to_string(),
        PLAIN_TEXT => TYPE_PLAIN_TEXT.to_string(),
        RICH_TEXT => TYPE_RICH_TEXT.to_string(),
        IMAGE => TYPE_IMAGE.to_string(),
        FILE => TYPE_FILE.to_string(),
        URL => TYPE_URL.to_string(),
        STEP => TYPE_OTHER.to_string(),
        0 => TYPE_ALL.to_string(),
        other => format!("{MASK_PREFIX}{other}"),
    }
}

pub fn to_item_type(type_key: &str) -> i32 {
    if type_key.trim().is_empty() {
        return ALL;
    }
    let key = type_key.trim().to_ascii_lowercase();
    if let Some(rest) = key.strip_prefix(MASK_PREFIX) {
        return rest.parse().unwrap_or(ALL);
    }
    match key.as_str() {
        TYPE_ALL | "default" => ALL,
        TYPE_TEXT => TEXT,
        "plain" | TYPE_PLAIN_TEXT => PLAIN_TEXT,
        "rich" | TYPE_RICH_TEXT | "html" => RICH_TEXT,
        TYPE_IMAGE => IMAGE,
        TYPE_FILE => FILE,
        TYPE_URL => URL,
        TYPE_OTHER | "step" => STEP,
        TYPE_STAR => ALL,
        _ => ALL,
    }
}

fn normalize_type_key(type_key: &str) -> String {
    let key = type_key.trim().to_ascii_lowercase();
    match key.as_str() {
        "default" => TYPE_ALL.to_string(),
        "plain" => TYPE_PLAIN_TEXT.to_string(),
        "rich" | "html" => TYPE_RICH_TEXT.to_string(),
        "step" => TYPE_OTHER.to_string(),
        _ => key,
    }
}

/// C# `ClipHostBridgeRouter.ResolveType`.
pub fn resolve_type(
    fallback_type: Option<&str>,
    filter: Option<&ClipFilterItemDto>,
) -> Option<String> {
    let Some(filter) = filter else {
        return fallback_type.map(str::to_string);
    };
    let key = filter.r#type.trim();
    if !key.is_empty() {
        return Some(normalize_type_key(key));
    }
    if filter.item_type != 0 {
        return Some(to_type_key(filter.item_type));
    }
    fallback_type.map(str::to_string)
}

/// Kind-only matching for filters that do not inspect content. URL filters
/// require `matches_with_text`; missing text never counts as a URL.
pub fn matches(kind: ClipKind, is_top: bool, type_key: &str) -> bool {
    matches_with_text(kind, is_top, None, type_key)
}

/// Match URL categories against the same HTTP(S) links highlighted in list text.
pub fn matches_with_text(kind: ClipKind, is_top: bool, text: Option<&str>, type_key: &str) -> bool {
    let normalized = type_key.trim();
    if normalized.is_empty() {
        return true;
    }
    if let Some(rest) = normalized
        .strip_prefix(MASK_PREFIX)
        .or_else(|| normalized.strip_prefix("MASK:"))
    {
        if let Ok(mask) = rest.parse::<i32>() {
            return matches_item_type_mask_with_text(kind, mask, text);
        }
    }
    match normalized.to_ascii_lowercase().as_str() {
        TYPE_ALL | "default" => true,
        TYPE_TEXT => is_text_kind(kind),
        "plain" | TYPE_PLAIN_TEXT => is_plain_kind(kind),
        "rich" | TYPE_RICH_TEXT | "html" => is_html_kind(kind),
        TYPE_IMAGE => kind == ClipKind::ImageClipItem,
        TYPE_FILE => kind == ClipKind::FileClipItem,
        TYPE_URL => has_url(kind, text),
        TYPE_OTHER | "step" => is_other_kind(kind),
        TYPE_STAR => is_top,
        _ => true,
    }
}

/// C# `ClipFilterItemTypes.MatchesItemTypeMask`.
pub fn matches_item_type_mask(kind: ClipKind, mask: i32) -> bool {
    matches_item_type_mask_with_text(kind, mask, None)
}

fn matches_item_type_mask_with_text(kind: ClipKind, mask: i32, text: Option<&str>) -> bool {
    if mask == 0 || mask & ALL == ALL {
        return true;
    }
    (mask & TEXT == TEXT && is_text_kind(kind))
        || (mask & PLAIN_TEXT == PLAIN_TEXT && is_plain_kind(kind))
        || (mask & RICH_TEXT == RICH_TEXT && is_html_kind(kind))
        || (mask & IMAGE == IMAGE && kind == ClipKind::ImageClipItem)
        || (mask & FILE == FILE && kind == ClipKind::FileClipItem)
        || (mask & STEP == STEP && is_other_kind(kind))
        || (mask & URL == URL && has_url(kind, text))
}

fn has_url(kind: ClipKind, text: Option<&str>) -> bool {
    kind.is_text_like() && text.is_some_and(crate::content_links::has_http_link)
}

pub fn matches_record(item: &ClipRecord, type_key: &str) -> bool {
    matches_with_text(item.kind, item.is_top, item.body_text.as_deref(), type_key)
        || (requires_text(type_key)
            && matches_with_text(item.kind, item.is_top, item.title.as_deref(), type_key))
}

/// Browsing reads only indexed list fields unless a category needs body text.
pub fn requires_text(type_key: &str) -> bool {
    let key = type_key.trim().to_ascii_lowercase();
    key == TYPE_URL || key.strip_prefix(MASK_PREFIX).and_then(|rest| rest.parse::<i32>().ok())
        .is_some_and(|mask| mask & ALL == 0 && mask & URL != 0)
}

fn is_text_kind(kind: ClipKind) -> bool {
    // C# `item.Item is TextClipItem` — HtmlClipItem and QQRichTextItem inherit TextClipItem.
    matches!(
        kind,
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
    )
}

fn is_html_kind(kind: ClipKind) -> bool {
    // C# `item.Item is HtmlClipItem` — QQRichTextItem inherits HtmlClipItem.
    matches!(kind, ClipKind::HtmlClipItem | ClipKind::QQRichTextItem)
}

fn is_plain_kind(kind: ClipKind) -> bool {
    // C# `TextClipItem && not HtmlClipItem`.
    kind == ClipKind::TextClipItem
}

fn is_other_kind(kind: ClipKind) -> bool {
    // C# `StepClipItem || (not TextClipItem && not ImageClipItem && not FileClipItem)`.
    kind == ClipKind::StepClipItem
        || (!is_text_kind(kind)
            && kind != ClipKind::ImageClipItem
            && kind != ClipKind::FileClipItem)
}

fn map_display_name(name: &str) -> Option<(&'static str, i32)> {
    match name {
        "默认" => Some((TYPE_ALL, ALL)),
        "文本" | "所有文本" => Some((TYPE_TEXT, TEXT)),
        "纯文本" => Some((TYPE_PLAIN_TEXT, PLAIN_TEXT)),
        "富文本" => Some((TYPE_RICH_TEXT, RICH_TEXT)),
        "图片" => Some((TYPE_IMAGE, IMAGE)),
        "文件" => Some((TYPE_FILE, FILE)),
        "网址" => Some((TYPE_URL, URL)),
        "步骤" => Some((TYPE_OTHER, STEP)),
        _ => None,
    }
}

fn read_string(obj: &serde_json::Map<String, Value>, camel: &str, pascal: &str) -> String {
    obj.get(camel)
        .or_else(|| obj.get(pascal))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn read_i32(obj: &serde_json::Map<String, Value>, camel: &str, pascal: &str) -> i32 {
    let value = obj.get(camel).or_else(|| obj.get(pascal));
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) as i32,
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repair_preserves_explicit_types_before_legacy_name_fallback() {
        let swapped = repair(ClipFilterItemDto {
            id: "a".into(),
            display_name: "图片".into(),
            r#type: String::new(),
            item_type: FILE,
            star_type: 0,
            icon: String::new(),
        });
        assert_eq!(swapped.r#type, TYPE_FILE);
        assert_eq!(swapped.item_type, FILE);
        assert_eq!(swapped.display_name, "图片");

        let legacy_name_only = repair(ClipFilterItemDto {
            display_name: "图片".into(),
            ..ClipFilterItemDto::default()
        });
        assert_eq!(legacy_name_only.r#type, TYPE_IMAGE);
        assert_eq!(legacy_name_only.item_type, IMAGE);

        for (name, type_key, item_type) in [("图片", TYPE_FILE, FILE), ("文件", TYPE_IMAGE, IMAGE)]
        {
            let explicit = repair(new_tab(name, type_key, ALL));
            assert_eq!(explicit.r#type, type_key);
            assert_eq!(explicit.item_type, item_type);
            assert_eq!(explicit.display_name, name);
        }

        let default_alias = repair(ClipFilterItemDto {
            id: "b".into(),
            display_name: "默认".into(),
            r#type: "default".into(),
            item_type: 0,
            star_type: 0,
            icon: String::new(),
        });
        assert_eq!(default_alias.r#type, TYPE_ALL);
        assert_eq!(default_alias.item_type, ALL);

        let html = repair(ClipFilterItemDto {
            id: "c".into(),
            display_name: "网页".into(),
            r#type: "html".into(),
            item_type: 0,
            star_type: 0,
            icon: String::new(),
        });
        assert_eq!(html.r#type, TYPE_RICH_TEXT);
        assert_eq!(html.item_type, RICH_TEXT);
        assert_eq!(html.display_name, "网页");
    }

    #[test]
    fn normalize_drops_star_tabs_and_repairs_duplicate_ids() {
        let tabs = normalize(vec![
            ClipFilterItemDto {
                id: "dup".into(),
                display_name: "星标".into(),
                r#type: TYPE_STAR.into(),
                item_type: ALL,
                star_type: 1,
                icon: String::new(),
            },
            ClipFilterItemDto {
                id: "same".into(),
                display_name: "文本".into(),
                r#type: TYPE_TEXT.into(),
                item_type: TEXT,
                star_type: 0,
                icon: String::new(),
            },
            ClipFilterItemDto {
                id: "same".into(),
                display_name: "文件".into(),
                r#type: TYPE_FILE.into(),
                item_type: FILE,
                star_type: 0,
                icon: String::new(),
            },
        ]);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, "same");
        assert_ne!(tabs[1].id, "same");
        assert_eq!(tabs[1].display_name, "文件");
    }

    #[test]
    fn normalize_retains_custom_names_resembling_removed_legacy_filters() {
        for name in ["星标", "无星标", "置顶"] {
            let typed = normalize(vec![new_tab(name, TYPE_TEXT, TEXT)]);
            assert_eq!(typed.len(), 1);
            assert_eq!(typed[0].display_name, name);
            assert_eq!(typed[0].r#type, TYPE_TEXT);

            let by_item_type = normalize(vec![new_tab(name, "", IMAGE)]);
            assert_eq!(by_item_type.len(), 1);
            assert_eq!(by_item_type[0].display_name, name);
            assert_eq!(by_item_type[0].r#type, TYPE_IMAGE);

            let legacy = normalize(vec![new_tab(name, "", 0), new_tab("保留", TYPE_FILE, FILE)]);
            assert_eq!(legacy.len(), 1);
            assert_eq!(legacy[0].display_name, "保留");
        }
    }

    #[test]
    fn automatic_and_custom_names_survive_form_roundtrip() {
        let defaults = create_default_tabs();
        assert_eq!(defaults.len(), 6);
        assert!(defaults.iter().all(|tab| tab.display_name.is_empty()));
        for value in [Value::Null, json!(""), json!([])] {
            assert!(parse_form_value(&value)
                .iter()
                .all(|tab| tab.display_name.is_empty()));
        }
        let tabs = parse_form_value(&json!([
            {"id":"automatic", "type":"image", "displayName":""},
            {"id":"cleared", "type":"file", "displayName":"  "},
            {"id":"omitted", "type":"text"},
            {"id":"legacy-default", "type":"all", "displayName":"默认"},
            {"id":"custom-default-word", "type":"image", "displayName":"文件"},
            {"id":"custom-english", "type":"text", "displayName":"Images"}
        ]));
        let saved = serialize_form_value(&tabs);
        let rows: Value = serde_json::from_str(&saved).unwrap();
        for index in 0..3 {
            assert_eq!(rows[index]["displayName"], "");
        }
        let reloaded = parse_form_value(&Value::String(saved));
        assert_eq!(
            serde_json::to_value(&reloaded).unwrap(),
            serde_json::to_value(&tabs).unwrap()
        );
        assert_eq!(reloaded[3].display_name, "默认");
        assert_eq!(reloaded[4].display_name, "文件");
        assert_eq!(reloaded[4].r#type, TYPE_IMAGE);
        assert_eq!(reloaded[5].display_name, "Images");
    }

    #[test]
    fn parse_form_value_accepts_json_string_like_csharp() {
        let payload = json!([
            { "Id": "", "DisplayName": "富文本", "Type": "rich", "ItemType": 64, "StarType": 0 }
        ]);
        let tabs = parse_form_value(&Value::String(payload.to_string()));
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].display_name, "富文本");
        assert_eq!(tabs[0].r#type, TYPE_RICH_TEXT);
        assert_eq!(tabs[0].item_type, RICH_TEXT);
        assert!(!tabs[0].id.is_empty());
    }

    #[test]
    fn url_filter_roundtrip_and_masks_keep_content_semantics() {
        let tab = repair(new_tab("", "URL", 0));
        assert_eq!(tab.item_type, URL);
        assert_eq!(tab.r#type, TYPE_URL);
        assert_eq!(to_type_key(URL), TYPE_URL);
        let loaded = parse_form_value(&json!(serialize_form_value(&[tab])));
        assert_eq!(loaded[0].r#type, TYPE_URL);
        let url = Some("标题\r\nhttps://example.test/");
        for kind in [ClipKind::TextClipItem, ClipKind::HtmlClipItem, ClipKind::QQRichTextItem, ClipKind::LinkClipItem] {
            assert!(matches_with_text(kind, false, url, TYPE_URL));
            assert!(!matches_with_text(kind, false, Some("plain"), TYPE_URL));
            assert!(!matches_with_text(kind, false, None, TYPE_URL));
            assert!(matches_with_text(kind, false, url, "mask:128"));
        }
        assert!(!matches_with_text(ClipKind::ImageClipItem, false, url, TYPE_URL));
        assert!(!matches_with_text(ClipKind::FileClipItem, false, url, TYPE_URL));
        let mixed = format!("mask:{}", URL | IMAGE);
        assert!(requires_text(&mixed));
        assert!(matches_with_text(ClipKind::ImageClipItem, false, None, &mixed));
        assert!(matches_with_text(ClipKind::TextClipItem, false, url, &mixed));
        assert!(!matches_with_text(ClipKind::TextClipItem, false, Some("plain"), &mixed));
        assert!(!requires_text("mask:129"));
    }

    #[test]
    fn matches_follows_csharp_inheritance() {
        assert!(matches(ClipKind::QQRichTextItem, false, TYPE_TEXT));
        assert!(matches(ClipKind::QQRichTextItem, false, TYPE_RICH_TEXT));
        assert!(!matches(ClipKind::QQRichTextItem, false, TYPE_PLAIN_TEXT));
        assert!(matches(ClipKind::HtmlClipItem, false, TYPE_TEXT));
        assert!(!matches(ClipKind::HtmlClipItem, false, TYPE_PLAIN_TEXT));
        assert!(matches(ClipKind::TextClipItem, false, TYPE_PLAIN_TEXT));
        assert!(matches(ClipKind::ActionClipItem, false, TYPE_OTHER));
        assert!(!matches(ClipKind::HtmlClipItem, false, TYPE_OTHER));
        assert!(matches(ClipKind::ImageClipItem, true, TYPE_STAR));
        assert!(!matches(ClipKind::ImageClipItem, false, TYPE_STAR));
        assert!(matches_item_type_mask(
            ClipKind::QQRichTextItem,
            TEXT | RICH_TEXT
        ));
        assert!(!matches_item_type_mask(
            ClipKind::QQRichTextItem,
            PLAIN_TEXT
        ));
    }

    #[test]
    fn resolve_type_matches_csharp_router() {
        let filter = ClipFilterItemDto {
            id: "x".into(),
            display_name: "网页".into(),
            r#type: "html".into(),
            item_type: 0,
            star_type: 0,
            icon: String::new(),
        };
        assert_eq!(
            resolve_type(Some("all"), Some(&filter)).as_deref(),
            Some(TYPE_RICH_TEXT)
        );
        let by_mask = ClipFilterItemDto {
            r#type: String::new(),
            item_type: PLAIN_TEXT,
            ..filter.clone()
        };
        assert_eq!(
            resolve_type(Some("file"), Some(&by_mask)).as_deref(),
            Some(TYPE_PLAIN_TEXT)
        );
        assert_eq!(resolve_type(Some("image"), None).as_deref(), Some("image"));
    }

    #[test]
    fn serialize_form_value_is_camel_case_json_string_like_csharp() {
        let raw = serialize_form_value(&[ClipFilterItemDto {
            id: "tab1".into(),
            display_name: "富文本".into(),
            r#type: "html".into(),
            item_type: 0,
            star_type: 0,
            icon: "ignored".into(),
        }]);
        assert!(raw.starts_with('['));
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed[0]["id"], "tab1");
        assert_eq!(parsed[0]["displayName"], "富文本");
        assert_eq!(parsed[0]["type"], TYPE_RICH_TEXT);
        assert_eq!(parsed[0]["itemType"], RICH_TEXT);
        assert!(parsed[0].get("icon").is_none());
    }
}
