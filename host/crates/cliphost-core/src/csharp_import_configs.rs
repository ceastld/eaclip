//! Prepare compatible C# preferences in memory for an isolated staged import.
//! Global/LAN/account configuration and executable dependencies are not read.
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{json, Map, Value};

use crate::dto::{ClipConfigDto, ClipFilterItemDto, ClipListCustomAppearanceDto};
use crate::settings_form::QuickPasteConfig;

const WHITELIST: [&str; 4] = [
    "ClipConfig.json",
    "ClipFilterConfig.json",
    "QuickPasteConfig.json",
    "TextActionData.json",
];
// These DTO members are projected from live/global state, not C# ClipConfig
// preferences. Importing them must not enable elevation or global side effects.
const PROJECTED_FIELDS: [&str; 16] = [
    "clipMonitorOn",
    "nonPinnedItemCount",
    "pinnedItemCount",
    "uiThemeMode",
    "runAsAdministrator",
    "runAsAdministratorEffective",
    "processElevated",
    "isDebugConfiguration",
    "coldStartElevationSuppressed",
    "windowBackground",
    "fontForeground",
    "windowBackgroundImage",
    "windowBackgroundBlend",
    "windowBackgroundFrostedGlass",
    "launchAtStartup",
    "launchAtStartupEffective",
];

pub(crate) fn prepare_configs(
    source_root: &Path,
    history_count: usize,
) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let source = fs::canonicalize(source_root).map_err(|e| format!("来源目录不可读：{e}"))?;
    let required_limit =
        i32::try_from(history_count).map_err(|_| "导入历史条数超过 Rust 配置容量。")?;
    let configs = source.join("Configs");
    let configs = match fs::symlink_metadata(&configs) {
        Ok(_) => {
            let canonical =
                fs::canonicalize(&configs).map_err(|e| format!("来源 Configs 不可读：{e}"))?;
            if !under(&canonical, &source) || !canonical.is_dir() {
                return Err("来源 Configs 必须是来源数据目录内的目录。".into());
            }
            Some(canonical)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("来源 Configs 不可读：{e}")),
    };
    let mut archive = Map::new();
    if let Some(configs) = &configs {
        for name in WHITELIST {
            let path = configs.join(name);
            match fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("来源 {name} 不可读：{e}")),
            }
            let path = fs::canonicalize(&path).map_err(|e| format!("来源 {name} 不可读：{e}"))?;
            if !under(&path, &source) || !path.is_file() {
                return Err(format!("来源 {name} 必须是来源数据目录内的文件。"));
            }
            let bytes = fs::read(path).map_err(|e| format!("来源 {name} 不可读：{e}"))?;
            // Newtonsoft may write a UTF-8 BOM; serde_json expects the JSON itself.
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
            let value: Value = serde_json::from_slice(bytes)
                .map_err(|_| format!("来源 {name} JSON 无效，已停止导入。"))?;
            if !value.is_object() {
                return Err(format!("来源 {name} 必须是 JSON 对象。"));
            }
            archive.insert(name.into(), value);
        }
    }
    let mut result = Vec::new();
    let clip = clip_config(archive.get("ClipConfig.json"), required_limit)?;
    result.push(prepared("Configs/ClipConfig.json", &clip)?);
    if let Some(value) = archive.get("ClipFilterConfig.json") {
        result.push(prepared(
            "Configs/ClipFilterConfig.json",
            &filter_config(value)?,
        )?);
    }
    if let Some(value) = archive.get("QuickPasteConfig.json") {
        result.push(prepared(
            "Configs/QuickPasteConfig.json",
            &quick_paste_config(value)?,
        )?);
    }
    if let Some(value) = archive.get("TextActionData.json") {
        result.push(prepared(
            "Configs/TextActionData.json",
            &text_action_config(value)?,
        )?);
    }
    if !archive.is_empty() {
        // Keep the complete whitelisted source JSON, including future fields;
        // only validated compatible fields reach the active Rust configs.
        result.push(prepared(
            "migration/source-configs.json",
            &Value::Object(archive),
        )?);
    }
    Ok(result)
}

fn prepared(path: &str, value: &Value) -> Result<(PathBuf, Vec<u8>), String> {
    Ok((
        PathBuf::from(path),
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    ))
}

fn field<'a>(value: &'a Value, name: &str) -> Result<Option<&'a Value>, String> {
    let object = value.as_object().ok_or("配置必须是对象。")?;
    let mut found = object
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let result = found.next().map(|(_, value)| value);
    if found.next().is_some() {
        return Err(format!("配置字段 {name} 有重复大小写名称。"));
    }
    Ok(result)
}

fn merge_known(defaults: Value, source: &Value) -> Result<Value, String> {
    let mut defaults = defaults
        .as_object()
        .ok_or("配置默认值必须是对象。")?
        .clone();
    for (key, target) in &mut defaults {
        if let Some(incoming) = field(source, key)? {
            *target = incoming.clone();
        }
    }
    Ok(Value::Object(defaults))
}

fn clip_config(source: Option<&Value>, history_count: i32) -> Result<Value, String> {
    let mut defaults = serde_json::to_value(ClipConfigDto::default()).map_err(|e| e.to_string())?;
    for key in PROJECTED_FIELDS {
        defaults.as_object_mut().unwrap().remove(key);
    }
    let mut value = match source {
        Some(source) => merge_known(defaults, source)?,
        None => defaults,
    };
    if let Some(source) = source {
        // C# keeps the historical misspelling, while the Rust persisted DTO uses
        // textProcessExpression. Do not silently drop clipboard preprocessing.
        if let Some(expression) = field(source, "TextProcessExpresstion")? {
            if field(source, "TextProcessExpression")?.is_some() {
                return Err("预处理表达式有重复新旧字段。".into());
            }
            value["textProcessExpression"] = expression.clone();
        }
        for name in [
            "showWindowLocation",
            "showWindowLocation2",
            "showWindowLocation3",
        ] {
            if let Some(location) = field(source, name)? {
                let token = match location {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    _ => return Err(format!("ClipConfig.{name} 位置无效。")),
                };
                let location = crate::window_place::parse_loc(&token)
                    .ok_or_else(|| format!("ClipConfig.{name} 位置无效。"))?;
                value[name] = json!(location.as_str());
            }
        }
        let has_custom = [
            "ClipListCustomChrome",
            "ClipListCustomLayout",
            "ClipListCustomShowProcess",
            "ClipListCustomShowTime",
            "ClipListCustomHoverActions",
        ]
        .iter()
        .try_fold(false, |found, key| {
            Ok::<_, String>(found || field(source, key)?.is_some())
        })?;
        if has_custom {
            let mut custom = serde_json::to_value(ClipListCustomAppearanceDto::default())
                .map_err(|e| e.to_string())?;
            for (source_key, target) in [
                ("ClipListCustomChrome", "chrome"),
                ("ClipListCustomLayout", "layout"),
                ("ClipListCustomShowProcess", "showProcess"),
                ("ClipListCustomShowTime", "showTime"),
            ] {
                if let Some(incoming) = field(source, source_key)? {
                    custom[target] = incoming.clone();
                }
            }
            if let Some(actions) = field(source, "ClipListCustomHoverActions")? {
                let actions = actions
                    .as_str()
                    .ok_or("ClipListCustomHoverActions 必须是字符串。")?;
                custom["hoverActions"] = json!(actions
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>());
            }
            normalize_enum(&mut custom, "chrome", &["rounded", "hairline", "flush"])?;
            normalize_enum(&mut custom, "layout", &["standard", "minimal"])?;
            value["clipListCustomAppearance"] = custom;
        }
    }
    normalize_enum(&mut value, "clipListLayoutDensity", &["compact", "relaxed"])?;
    normalize_enum(&mut value, "clipListDisplayMode", &["standard", "minimal"])?;
    normalize_enum(
        &mut value,
        "clipListAppearanceSlot",
        &["standard", "simple", "custom"],
    )?;
    let mut config: ClipConfigDto =
        serde_json::from_value(value).map_err(|e| format!("ClipConfig 字段类型不兼容：{e}"))?;
    config.max_item_size = config.max_item_size.max(history_count).max(1);
    let mut value = serde_json::to_value(config).map_err(|e| e.to_string())?;
    for key in PROJECTED_FIELDS {
        value.as_object_mut().unwrap().remove(key);
    }
    Ok(value)
}

fn normalize_enum(value: &mut Value, key: &str, names: &[&str]) -> Result<(), String> {
    let Some(current) = value.get(key) else {
        return Ok(());
    };
    if current.is_null() {
        return Ok(());
    }
    let name = if let Some(text) = current.as_str() {
        names
            .iter()
            .find(|name| name.eq_ignore_ascii_case(text))
            .copied()
    } else {
        current
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .and_then(|index| names.get(index).copied())
    }
    .ok_or_else(|| format!("配置 {key} 的枚举值不兼容。"))?;
    value[key] = json!(name);
    Ok(())
}

fn quick_paste_config(source: &Value) -> Result<Value, String> {
    let defaults = serde_json::to_value(QuickPasteConfig::default()).map_err(|e| e.to_string())?;
    let mut value = merge_known(defaults, source)?;
    for key in ["Shortcut1Action", "Shortcut2Action"] {
        normalize_enum(&mut value, key, &["Auto", "Simulate", "Open", "None"])?;
    }
    normalize_enum(&mut value, "MouseWheelTrigger", &["none", "x1", "x2"])?;
    let config: QuickPasteConfig = serde_json::from_value(value)
        .map_err(|e| format!("QuickPasteConfig 字段类型不兼容：{e}"))?;
    serde_json::to_value(config).map_err(|e| e.to_string())
}

fn filter_config(source: &Value) -> Result<Value, String> {
    let rows = field(source, "FilterItems")?.ok_or("ClipFilterConfig 缺少 FilterItems。")?;
    let rows = rows
        .as_array()
        .ok_or("ClipFilterConfig.FilterItems 必须是数组。")?;
    for row in rows {
        if !row.is_object() {
            return Err("ClipFilterConfig.FilterItems 包含非对象项。".into());
        }
        let mapped = merge_known(
            serde_json::to_value(ClipFilterItemDto::default()).map_err(|e| e.to_string())?,
            row,
        )?;
        serde_json::from_value::<ClipFilterItemDto>(mapped)
            .map_err(|e| format!("剪贴板分类字段类型不兼容：{e}"))?;
    }
    Ok(json!({"FilterItems":crate::clip_filter::parse_form_value(&Value::Array(rows.clone()))}))
}

fn text_action_config(source: &Value) -> Result<Value, String> {
    let actions = field(source, "Actions")?
        .ok_or("TextActionData 缺少 Actions。")?
        .as_array()
        .ok_or("TextActionData.Actions 必须是数组。")?;
    let types = [
        (1, "Internal"),
        (2, "SingleReplace"),
        (4, "ZExpression"),
        (5, "QuickerAction"),
        (6, "Pair"),
        (7, "Send"),
    ];
    let mut converted = Vec::new();
    for action in actions {
        let discriminator = field(action, "Type")?.ok_or("文本操作缺少 Type。")?;
        let type_name = types
            .iter()
            .find(|(id, name)| {
                discriminator.as_i64() == Some(*id)
                    || discriminator
                        .as_str()
                        .is_some_and(|text| name.eq_ignore_ascii_case(text))
            })
            .map(|(_, name)| *name)
            .ok_or("文本操作类型尚不兼容，已停止导入。")?;
        // Canonical names match the existing Rust loader; unknown fields remain
        // in the source archive, rather than introducing executable dependencies.
        let mut canonical = Map::new();
        for name in [
            "Title",
            "Description",
            "Simulate",
            "SimulateDelay",
            "Group",
            "Data",
        ] {
            if let Some(value) = field(action, name)? {
                canonical.insert(name.into(), value.clone());
            }
        }
        canonical.insert("Type".into(), json!(type_name));
        let action = Value::Object(canonical);
        validate_action(&action, type_name)?;
        converted.push(action);
    }
    Ok(json!({"Actions":converted}))
}

fn validate_action(action: &Value, kind: &str) -> Result<(), String> {
    for key in ["Title", "Description", "Group"] {
        require_type(action, key, "string")?;
    }
    require_type(action, "Simulate", "boolean")?;
    require_type(action, "SimulateDelay", "integer")?;
    let Some(data) = field(action, "Data")?.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    if !data.is_object() {
        return Err("文本操作 Data 必须是对象。".into());
    }
    let (strings, booleans, integers): (&[&str], &[&str], &[&str]) = match kind {
        "Internal" => (&["Type"], &[], &[]),
        "SingleReplace" => (
            &["Pattern", "ReplaceMent"],
            &["EscapePattern", "EscapeReplaceMent", "UseRegex"],
            &["Options"],
        ),
        "Pair" => (&["Prefix", "Suffix"], &["Trim", "MultiLines"], &[]),
        "Send" => (&["Text"], &[], &[]),
        "ZExpression" => (&["Expresstion", "Expression"], &[], &[]),
        "QuickerAction" => (&["Id"], &["WaitResult"], &[]),
        _ => unreachable!(),
    };
    for key in strings {
        require_type(data, key, "string")?;
    }
    for key in booleans {
        require_type(data, key, "boolean")?;
    }
    for key in integers {
        require_type(data, key, "integer")?;
    }
    if kind == "Internal" {
        if let Some(value) = field(data, "Type")?.filter(|value| !value.is_null()) {
            let text = value.as_str().unwrap();
            if ![
                "RemoveEmptyLine",
                "RemoveWhiteSpaceLine",
                "StandardText",
                "FormatJson",
                "ZipJson",
            ]
            .contains(&text)
            {
                return Err("内置文本操作尚不兼容，已停止导入。".into());
            }
        }
    }
    Ok(())
}

fn require_type(object: &Value, key: &str, kind: &str) -> Result<(), String> {
    let Some(value) = field(object, key)?.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let valid = match kind {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .is_some(),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(format!("文本操作 {key} 字段类型不兼容。"))
    }
}

fn under(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().to_ascii_lowercase();
        let root = root
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        path == root || path.starts_with(&(root + "\\"))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_prepared(files: &[(PathBuf, Vec<u8>)], path: &str) -> Value {
        serde_json::from_slice(
            &files
                .iter()
                .find(|(name, _)| name == Path::new(path))
                .unwrap()
                .1,
        )
        .unwrap()
    }

    #[test]
    fn csharp_import_configs_translate_supported_preferences_and_guard_retention() {
        let root = tempfile::tempdir().unwrap();
        let configs = root.path().join("Configs");
        fs::create_dir(&configs).unwrap();
        let clip = json!({"MaxItemSize":25,"UsingNumberKeyToPaste":false,"AllowDuplicateCopy":true,
            "EnableNoFocusMode":false,"DismissBehavior":"Hide","ShowWindowLocation":11,
            "TextProcessExpresstion":"$${text}","ClipListLayoutDensity":"Relaxed",
            "ClipListCustomChrome":"Rounded","ClipListCustomLayout":"Minimal",
            "ClipListCustomHoverActions":"primary, pin, delete","RunAsAdministrator":true,
            "FuturePreference":{"retained":true}});
        fs::write(
            configs.join("ClipConfig.json"),
            serde_json::to_vec(&clip).unwrap(),
        )
        .unwrap();
        fs::write(
            configs.join("QuickPasteConfig.json"),
            br#"{"ListLimit":17,"RememberSelection":false,"Shortcut1Action":"Open"}"#,
        )
        .unwrap();
        fs::write(configs.join("ClipFilterConfig.json"), br#"{"FilterItems":[{"Id":"my-category","DisplayName":"Plain","Type":"plaintext","ItemType":32}]}"#).unwrap();
        fs::write(configs.join("TextActionData.json"), br#"{"Actions":[{"Type":6,"Title":"Wrap","Data":{"ObjType":6,"Prefix":"[","Suffix":"]","Trim":true}}]}"#).unwrap();
        fs::write(configs.join("GlobalConfig.json"), b"not read or copied").unwrap();
        fs::write(configs.join("LanShareConfig.json"), b"not read or copied").unwrap();
        let source_bytes = fs::read(configs.join("ClipConfig.json")).unwrap();

        let prepared = prepare_configs(root.path(), 500).unwrap();
        let clip = read_prepared(&prepared, "Configs/ClipConfig.json");
        assert_eq!(clip["maxItemSize"], 500);
        assert_eq!(clip["usingNumberKeyToPaste"], false);
        assert_eq!(clip["allowDuplicateCopy"], true);
        assert_eq!(clip["enableNoFocusMode"], false);
        assert_eq!(clip["showWindowLocation"], "MouseRightBottom");
        assert_eq!(clip["textProcessExpression"], "$${text}");
        assert_eq!(clip["clipListLayoutDensity"], "relaxed");
        assert_eq!(clip["clipListCustomAppearance"]["chrome"], "rounded");
        assert_eq!(
            clip["clipListCustomAppearance"]["hoverActions"],
            json!(["primary", "pin", "delete"])
        );
        assert!(clip.get("runAsAdministrator").is_none());
        assert!(clip.get("FuturePreference").is_none());
        let quick: QuickPasteConfig =
            serde_json::from_value(read_prepared(&prepared, "Configs/QuickPasteConfig.json"))
                .unwrap();
        assert_eq!(quick.list_limit, 17);
        assert!(!quick.remember_selection);
        assert_eq!(quick.shortcut1_action, "Open");
        assert_eq!(quick.mouse_wheel_trigger, "none");
        assert_eq!(
            read_prepared(&prepared, "Configs/ClipFilterConfig.json")["FilterItems"][0]["id"],
            "my-category"
        );
        assert_eq!(
            read_prepared(&prepared, "Configs/TextActionData.json")["Actions"][0]["Type"],
            "Pair"
        );
        let archive = read_prepared(&prepared, "migration/source-configs.json");
        assert_eq!(
            archive["ClipConfig.json"]["FuturePreference"]["retained"],
            true
        );
        assert!(archive.get("GlobalConfig.json").is_none());
        assert!(prepared
            .iter()
            .all(|(path, _)| path.starts_with("Configs") || path.starts_with("migration")));
        assert_eq!(
            fs::read(configs.join("ClipConfig.json")).unwrap(),
            source_bytes
        );
    }

    #[test]
    fn csharp_import_configs_missing_config_protects_all_imported_records_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let prepared = prepare_configs(root.path(), 1000).unwrap();
        assert_eq!(
            read_prepared(&prepared, "Configs/ClipConfig.json")["maxItemSize"],
            1000
        );
        assert!(!root.path().join("Configs").exists());
        assert!(!root.path().join("migration").exists());
    }

    #[test]
    fn csharp_import_configs_refuses_corrupt_or_incompatible_known_fields() {
        let root = tempfile::tempdir().unwrap();
        let configs = root.path().join("Configs");
        fs::create_dir(&configs).unwrap();
        for (name, bytes) in [
            ("ClipConfig.json", "not JSON"),
            ("ClipConfig.json", r#"{"MaxItemSize":"wrong type"}"#),
            (
                "QuickPasteConfig.json",
                r#"{"RememberSelection":"wrong type"}"#,
            ),
            (
                "ClipFilterConfig.json",
                r#"{"FilterItems":[{"ItemType":"wrong type"}]}"#,
            ),
            (
                "TextActionData.json",
                r#"{"Actions":[{"Type":3,"Data":{}}]}"#,
            ),
        ] {
            fs::write(configs.join(name), bytes).unwrap();
            assert!(
                prepare_configs(root.path(), 100).is_err(),
                "{name}: {bytes}"
            );
            fs::remove_file(configs.join(name)).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn csharp_import_configs_rejects_an_external_configs_symlink() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("Configs")).unwrap();
        assert!(prepare_configs(root.path(), 100).is_err());
    }
}
