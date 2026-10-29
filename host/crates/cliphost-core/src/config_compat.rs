//! Read C#/Rust preferences in place; never replace an unreadable file with defaults.
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Map, Value};
use std::{fs, path::Path};

const BOM: &[u8] = &[0xef, 0xbb, 0xbf];

pub(crate) fn read(path: &Path) -> Result<Option<Value>, String> {
    match fs::read(path) {
        Ok(bytes) => parse(path, &bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取配置 {} 失败：{error}", path.display())),
    }
}

fn parse(path: &Path, bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes.strip_prefix(BOM).unwrap_or(bytes))
        .map_err(|error| format!("配置 {} JSON 无效，未使用默认值：{error}", path.display()))
}

pub(crate) fn load<T: Default + Serialize + DeserializeOwned>(path: &Path) -> Result<T, String> {
    let Some(source) = read(path)? else {
        return Ok(T::default());
    };
    let defaults = serde_json::to_value(T::default()).map_err(|error| error.to_string())?;
    let value = normalize(file_name(path), "", &source, &defaults)?;
    serde_json::from_value(value)
        .map_err(|error| format!("配置 {} 字段不兼容，未使用默认值：{error}", path.display()))
}

pub(crate) fn load_hotkeys(configs: &Path) -> Result<crate::settings_form::GlobalHotkeys, String> {
    let path = configs.join("GlobalHotkeys.json");
    let Some(source) = read(&configs.join("GlobalConfig.json"))? else {
        return load(&path);
    };
    let object = source.as_object().ok_or("GlobalConfig 必须是对象。")?;
    if ![
        "HotkeyShowClipboard",
        "HotkeyFavorToggle",
        "HotkeyQuickPaste",
        "HotkeySuperMenu",
    ]
    .iter()
    .map(|key| matching_key("GlobalConfig.json", object, key))
    .collect::<Result<Vec<_>, _>>()?
    .iter()
    .any(Option::is_some)
    {
        return load(&path);
    }
    let defaults = serde_json::to_value(crate::settings_form::GlobalHotkeys::default())
        .map_err(|error| error.to_string())?;
    serde_json::from_value(normalize("globalconfig.json", "", &source, &defaults)?)
        .map_err(|error| format!("GlobalConfig 快捷键字段不兼容：{error}"))
}

pub(crate) fn load_filters(path: &Path) -> Result<Vec<crate::dto::ClipFilterItemDto>, String> {
    let Some(source) = read(path)? else {
        return Ok(crate::clip_filter::create_default_tabs());
    };
    let rows = field(&source, "FilterItems")?
        .and_then(Value::as_array)
        .ok_or("ClipFilterConfig.FilterItems 必须是数组。")?;
    let defaults = serde_json::to_value(crate::dto::ClipFilterItemDto::default())
        .map_err(|e| e.to_string())?;
    let rows = rows
        .iter()
        .map(|row| {
            if !row.is_object() {
                return Err("ClipFilterConfig 包含非对象分类。".into());
            }
            serde_json::from_value(normalize("ClipFilterConfig.json", "", row, &defaults)?)
                .map_err(|e| format!("剪贴板分类字段不兼容：{e}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut tabs = crate::clip_filter::normalize(rows);
    let url_default_added = match field(&source, "RustUrlFilterDefaultAdded")? {
        None => false,
        Some(Value::Bool(added)) => *added,
        _ => return Err("ClipFilterConfig.RustUrlFilterDefaultAdded 必须是布尔值。".into()),
    };
    if !url_default_added {
        let mut updated = source.clone();
        if !tabs.iter().any(|tab| tab.item_type == crate::clip_filter::URL) {
            let url = crate::clip_filter::new_tab("", crate::clip_filter::TYPE_URL, crate::clip_filter::URL);
            let key = matching_key("ClipFilterConfig.json", source.as_object().unwrap(), "FilterItems")?.unwrap();
            // Append only the new default; preserve existing names, IDs, order
            // and extension fields exactly as stored by either host.
            updated[&key].as_array_mut().unwrap().push(serde_json::to_value(&url).map_err(|e| e.to_string())?);
            tabs.push(url);
        }
        let marker = matching_key("ClipFilterConfig.json", source.as_object().unwrap(), "RustUrlFilterDefaultAdded")?
            .unwrap_or_else(|| "RustUrlFilterDefaultAdded".into());
        updated[marker] = json!(true);
        crate::config_file::save(path, &updated)?;
    }
    Ok(tabs)
}

pub(crate) fn load_actions(path: &Path) -> Result<Vec<Value>, String> {
    let Some(source) = read(path)? else {
        return Ok(Vec::new());
    };
    let rows = field(&source, "Actions")?
        .and_then(Value::as_array)
        .ok_or("TextActionData.Actions 必须是数组。")?;
    rows.iter()
        .map(|row| {
            let mut action = row
                .as_object()
                .ok_or("TextActionData 包含非对象操作。")?
                .clone();
            for key in [
                "Type",
                "Title",
                "Description",
                "Simulate",
                "SimulateDelay",
                "Group",
                "Data",
                "Id",
            ] {
                if let Some(original) = matching_key("TextActionData.json", &action, key)? {
                    let mut value = action.remove(&original).unwrap();
                    if key == "Type" {
                        value = enum_value("TextActionData.json", key, &value)?
                    }
                    action.insert(key.into(), value);
                }
            }
            Ok(Value::Object(action))
        })
        .collect()
}

pub(crate) fn field<'a>(source: &'a Value, name: &str) -> Result<Option<&'a Value>, String> {
    let object = source.as_object().ok_or("配置必须是 JSON 对象。")?;
    let mut matches = object
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let result = matches.next().map(|(_, value)| value);
    if matches.next().is_some() {
        return Err(format!("配置字段 {name} 有重复大小写名称。"));
    }
    Ok(result)
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}

fn canonical(file: &str, key: &str) -> String {
    let lower = key.to_ascii_lowercase();
    match (file.to_ascii_lowercase().as_str(), lower.as_str()) {
        ("clipconfig.json", "textprocessexpresstion") => "textprocessexpression".into(),
        ("globalconfig.json", "windowbackground") => "windowbackgroundcolor".into(),
        ("globalconfig.json", "fontforeground") => "fontforegroundcolor".into(),
        ("globalconfig.json", "windowbackgroundimage") => "windowbackgroundimagepath".into(),
        ("globalconfig.json" | "globalhotkeys.json", "hotkeycliptoggle") => {
            "hotkeyshowclipboard".into()
        }
        _ => lower,
    }
}

fn matching_key(
    file: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    let target = canonical(file, key);
    let mut matches = object.keys().filter(|name| canonical(file, name) == target);
    let first = matches.next().cloned();
    if matches.next().is_some() {
        return Err(format!("配置字段 {key} 有重复新旧/大小写名称。"));
    }
    Ok(first)
}

fn global_appearance_names(file: &str, parent: &str, key: &str) -> Option<(&'static str, &'static str)> {
    if !parent.is_empty() || !file.eq_ignore_ascii_case("GlobalConfig.json") {
        return None;
    }
    match canonical(file, key).as_str() {
        "windowbackgroundcolor" => Some(("WindowBackground", "WindowBackgroundColor")),
        "fontforegroundcolor" => Some(("FontForeground", "FontForegroundColor")),
        "windowbackgroundimagepath" => Some(("WindowBackgroundImage", "WindowBackgroundImagePath")),
        _ => None,
    }
}

fn matching_config_key(
    file: &str,
    parent: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    let Some((persisted, rust)) = global_appearance_names(file, parent, key) else {
        return matching_key(file, object, key);
    };
    // C# still reads/writes the shorter JsonProperty names. Older Rust writes
    // may coexist with them; respect that wire contract before Rust aliases.
    for name in [persisted, rust] {
        if object.contains_key(name) {
            return Ok(Some(name.into()));
        }
        let matches: Vec<_> = object.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name)).collect();
        if let Some((first, value)) = matches.first() {
            if matches.iter().skip(1).any(|(_, other)| other != value) {
                return Err(format!("配置字段 {key} 有重复新旧/大小写名称。"));
            }
            return Ok(Some((*first).clone()));
        }
    }
    Ok(None)
}

fn normalize_global_appearance_wire(file: &str, value: &mut Value) -> Result<(), String> {
    if !file.eq_ignore_ascii_case("GlobalConfig.json") {
        return Ok(());
    }
    let object = value.as_object_mut().ok_or("GlobalConfig 必须是对象。")?;
    for name in ["WindowBackground", "FontForeground", "WindowBackgroundImage"] {
        let Some(original) = matching_config_key(file, "", object, name)? else {
            continue;
        };
        let incoming = object[&original].clone();
        let target = canonical(file, name);
        object.retain(|alias, _| canonical(file, alias) != target);
        object.insert(name.into(), incoming);
    }
    Ok(())
}

fn nullable_default(file: &str, key: &str) -> Option<Value> {
    // C# theme parsing accepts null colours; host defaults restore clip/favor
    // hotkeys, while null quick-paste and Super menu chords mean disabled.
    match (
        file.to_ascii_lowercase().as_str(),
        canonical(file, key).as_str(),
    ) {
        ("globalconfig.json", "uithememode") => Some(json!("Auto")),
        ("globalconfig.json", "windowbackgroundcolor") => Some(json!("#FFFFFFFF")),
        ("globalconfig.json", "fontforegroundcolor") => Some(json!("#FF000000")),
        ("globalconfig.json", "windowbackgroundimagepath" | "hotkeyquickpaste" | "hotkeysupermenu") => Some(json!("")),
        ("globalconfig.json", "windowbackgroundblend") => Some(json!(70)),
        ("globalconfig.json", "windowbackgroundfrostedglass" | "launchatstartup") => {
            Some(json!(true))
        }
        ("globalconfig.json", "runasadministrator") => Some(json!(false)),
        ("globalconfig.json", "hotkeyshowclipboard") => {
            Some(json!(crate::global_hotkey::DEFAULT_CLIP_TOGGLE))
        }
        ("globalconfig.json", "hotkeyfavortoggle") => {
            Some(json!(crate::global_hotkey::DEFAULT_FAVOR_TOGGLE))
        }
        ("filemenuconfig.json", "files") => Some(json!("")),
        (
            "clipconfig.json",
            "blacklist"
            | "fontfamilystring"
            | "textprocessexpression"
            | "clipchangedactionid"
            | "useshiftinserttopasteproc",
        ) => Some(json!("")),
        _ => None,
    }
}

fn normalize(file: &str, parent: &str, source: &Value, defaults: &Value) -> Result<Value, String> {
    let (Some(source), Some(defaults)) = (source.as_object(), defaults.as_object()) else {
        return Ok(source.clone());
    };
    // Keep fields omitted by Serialize (for example empty RustOpenedFolders),
    // and let Deserialize ignore genuinely unknown fields after normalization.
    let mut result = source.clone();
    for (key, default) in defaults {
        if let Some(original) = matching_config_key(file, parent, source, key)? {
            let path = if parent.is_empty() {
                key.clone()
            } else {
                format!("{parent}.{key}")
            };
            let mut value = if source[&original].is_null() {
                nullable_default(file, &path).unwrap_or_else(|| Value::Null)
            } else {
                normalize(file, &path, &source[&original], default)?
            };
            value = enum_value(file, &path, &value)?;
            if global_appearance_names(file, parent, key).is_some() {
                let target = canonical(file, key);
                result.retain(|alias, _| canonical(file, alias) != target);
            } else {
                result.remove(&original);
            }
            result.insert(key.clone(), value);
        } else {
            result.insert(key.clone(), default.clone());
        }
    }
    if file.eq_ignore_ascii_case("ClipConfig.json") && parent.is_empty() {
        let flat = custom_fields()
            .iter()
            .any(|(old, _)| source.keys().any(|key| key.eq_ignore_ascii_case(old)));
        if flat {
            if matching_key(file, source, "clipListCustomAppearance")?.is_some() {
                return Err("列表外观同时包含新旧两套自定义字段，未使用默认值。".into());
            }
            let mut custom = result["clipListCustomAppearance"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for (old, key) in custom_fields() {
                if let Some(original) = matching_key(file, source, old)? {
                    let value = if *key == "hoverActions" {
                        let text = source[&original]
                            .as_str()
                            .ok_or("ClipListCustomHoverActions 必须是字符串。")?;
                        json!(text
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>())
                    } else {
                        enum_value(
                            file,
                            &format!("clipListCustomAppearance.{key}"),
                            &source[&original],
                        )?
                    };
                    custom.insert((*key).into(), value);
                }
            }
            result.insert("clipListCustomAppearance".into(), Value::Object(custom));
        }
    }
    Ok(Value::Object(result))
}

fn custom_fields() -> &'static [(&'static str, &'static str)] {
    &[
        ("ClipListCustomChrome", "chrome"),
        ("ClipListCustomLayout", "layout"),
        ("ClipListCustomShowProcess", "showProcess"),
        ("ClipListCustomShowTime", "showTime"),
        ("ClipListCustomHoverActions", "hoverActions"),
    ]
}

// Numeric values come from the owning C# enums, not array positions guessed at runtime.
fn enum_names(file: &str, key: &str) -> Option<&'static [(i64, &'static str)]> {
    match (
        file.to_ascii_lowercase().as_str(),
        key.to_ascii_lowercase().as_str(),
    ) {
        ("clipconfig.json", "dismissbehavior") => {
            Some(&[(0, "Hide"), (1, "EdgeHide"), (2, "Fold")])
        }
        ("clipconfig.json", "copytoastplacement") => {
            Some(&[(0, "BottomCenter"), (1, "TopRight"), (2, "BottomRight")])
        }
        ("clipconfig.json", "cliplistexecutemode") => Some(&[
            (1, "SingleClickPaste"),
            (2, "CopyThenDoubleClickPaste"),
            (3, "DoubleClickOnly"),
        ]),
        ("clipconfig.json", "cliplistlayoutdensity") => Some(&[(0, "compact"), (1, "relaxed")]),
        ("clipconfig.json", "cliplistdisplaymode" | "cliplistcustomappearance.layout") => {
            Some(&[(0, "standard"), (1, "minimal")])
        }
        ("clipconfig.json", "cliplistappearanceslot") => {
            Some(&[(0, "standard"), (1, "simple"), (2, "custom")])
        }
        ("clipconfig.json", "cliplistcustomappearance.chrome") => {
            Some(&[(0, "rounded"), (1, "hairline"), (2, "flush")])
        }
        ("globalconfig.json", "uithememode") => {
            Some(&[(0, "Auto"), (1, "Light"), (2, "Dark"), (3, "Custom")])
        }
        ("quickpasteconfig.json", "shortcut1action" | "shortcut2action") => {
            Some(&[(0, "Auto"), (1, "Simulate"), (2, "Open"), (3, "None")])
        }
        ("quickpasteconfig.json", "mousewheeltrigger") => {
            Some(&[(0, "none"), (1, "x1"), (2, "x2")])
        }
        ("panelconfig.json", "folderdefaultclickaction" | "filedefaultclickaction") => {
            Some(&[(0, "Open"), (1, "Copy"), (2, "Paste")])
        }
        ("panelconfig.json", "defaultclickaction") => {
            Some(&[(0, "OpenMenu"), (1, "EnterFavorite")])
        }
        ("panelconfig.json", "panelstyle") => Some(&[
            (0, "PanelStyle1"),
            (1, "PanelStyle2"),
            (2, "PanelStyle3"),
            (3, "PanelStyle4"),
        ]),
        ("filemenuconfig.json", "filesorttype") => Some(&[
            (0, "NA"),
            (1, "name"),
            (2, "createTime"),
            (3, "accessTime"),
            (4, "editTime"),
        ]),
        ("filemenuconfig.json", "showtype") => Some(&[(0, "dir_file"), (1, "dir"), (2, "file")]),
        ("favorconfig.json", "defaultinsertpos") => Some(&[(0, "Start"), (1, "End")]),
        ("textactiondata.json", "type") => Some(&[
            (1, "Internal"),
            (2, "SingleReplace"),
            (4, "ZExpression"),
            (5, "QuickerAction"),
            (6, "Pair"),
            (7, "Send"),
        ]),
        _ => None,
    }
}

fn enum_value(file: &str, key: &str, value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Ok(value.clone());
    }
    if key.to_ascii_lowercase().starts_with("showwindowlocation")
        || key.eq_ignore_ascii_case("ShowMenuLocation")
    {
        let token = value
            .as_str()
            .map(str::to_string)
            .or_else(|| value.as_i64().map(|n| n.to_string()))
            .ok_or_else(|| format!("配置 {key} 窗口位置类型不兼容。"))?;
        return crate::window_place::parse_loc(&token)
            .map(|loc| json!(loc.as_str()))
            .ok_or_else(|| format!("配置 {key} 窗口位置不兼容。"));
    }
    let Some(names) = enum_names(file, key) else {
        return Ok(value.clone());
    };
    let number = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()));
    let (id, name) = names
        .iter()
        .find(|(id, name)| {
            number == Some(*id) || value.as_str().is_some_and(|s| s.eq_ignore_ascii_case(name))
        })
        .ok_or_else(|| format!("配置 {key} 枚举值不兼容，未使用默认值。"))?;
    if key.eq_ignore_ascii_case("clipListExecuteMode")
        || key.eq_ignore_ascii_case("DefaultInsertPos")
    {
        Ok(json!(id))
    } else {
        Ok(json!(name))
    }
}

fn supported(path: &Path) -> bool {
    matches!(
        file_name(path).to_ascii_lowercase().as_str(),
        "clipconfig.json"
            | "clipfilterconfig.json"
            | "quickpasteconfig.json"
            | "globalhotkeys.json"
            | "globalconfig.json"
            | "panelconfig.json"
            | "userconfig.json"
            | "globalmenuconfig.json"
            | "filemenuconfig.json"
            | "textactiondata.json"
            | "favorconfig.json"
            | "user_favormenu.json"
    )
}

pub(crate) fn encode_preserving(
    path: &Path,
    value: &Value,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut value = value.clone();
    normalize_global_appearance_wire(file_name(path), &mut value)?;
    if supported(path) {
        if let Some(bytes) = previous {
            let mut original = parse(path, bytes)?;
            if !original.is_object() || !value.is_object() {
                return Err(format!(
                    "配置 {} 必须是对象，原内容未覆盖。",
                    path.display()
                ));
            }
            if file_name(path).eq_ignore_ascii_case("ClipConfig.json") {
                // A live session flag is deliberately not part of either persisted model.
                original
                    .as_object_mut()
                    .unwrap()
                    .retain(|key, _| !key.eq_ignore_ascii_case("ClipMonitorOn"));
                value
                    .as_object_mut()
                    .unwrap()
                    .retain(|key, _| !key.eq_ignore_ascii_case("ClipMonitorOn"));
            }
            if file_name(path).eq_ignore_ascii_case("FileMenuConfig.json")
                && field(&value, "RustOpenedFolders")?.is_none()
            {
                // This known field uses skip_serializing_if when the last folder closes.
                original
                    .as_object_mut()
                    .unwrap()
                    .retain(|key, _| !key.eq_ignore_ascii_case("RustOpenedFolders"));
            }
            value = merge_preserving(file_name(path), "", &original, &value)?;
        }
    }
    // One on-disk field per preference, including the first save of a new
    // file. Keep the model/API names unchanged; only this disk boundary maps
    // the three historical appearance aliases to C#'s current wire names.
    normalize_global_appearance_wire(file_name(path), &mut value)?;
    let mut bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    if supported(path) && previous.is_some_and(|bytes| bytes.starts_with(BOM)) {
        bytes.splice(0..0, BOM.iter().copied());
    }
    Ok(bytes)
}

fn old_style_key(file: &str, key: &str) -> String {
    match (
        file.to_ascii_lowercase().as_str(),
        key.to_ascii_lowercase().as_str(),
    ) {
        ("clipconfig.json", "textprocessexpression") => "TextProcessExpresstion".into(),
        ("globalconfig.json", "windowbackgroundcolor") => "WindowBackground".into(),
        ("globalconfig.json", "fontforegroundcolor") => "FontForeground".into(),
        ("globalconfig.json", "windowbackgroundimagepath") => "WindowBackgroundImage".into(),
        ("globalconfig.json", "hotkeyshowclipboard") => "HotkeyClipToggle".into(),
        _ => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        }
    }
}

fn numeric_enum(file: &str, key: &str, new: &Value) -> Option<i64> {
    if key.to_ascii_lowercase().starts_with("showwindowlocation")
        || key.eq_ignore_ascii_case("ShowMenuLocation")
    {
        let names = [
            "NA",
            "TopLeft",
            "TopCenter",
            "TopRight",
            "LeftCenter",
            "CenterScreen",
            "RightCenter",
            "BottomLeft",
            "BottomCenter",
            "BottomRight",
            "MouseAround",
            "MouseRightBottom",
            "InScreen",
        ];
        return names
            .iter()
            .position(|name| new.as_str().is_some_and(|s| s.eq_ignore_ascii_case(name)))
            .map(|n| n as i64);
    }
    enum_names(file, key)?
        .iter()
        .find(|(id, name)| {
            new.as_i64() == Some(*id) || new.as_str().is_some_and(|s| s.eq_ignore_ascii_case(name))
        })
        .map(|(id, _)| *id)
}

fn merge_preserving(file: &str, parent: &str, old: &Value, new: &Value) -> Result<Value, String> {
    if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
        let legacy = if file.eq_ignore_ascii_case("ClipConfig.json") {
            [
                "MaxItemSize",
                "DismissBehavior",
                "AllowDuplicateCopy",
                "UsingNumberKeyToPaste",
                "TextProcessExpresstion",
            ]
            .iter()
            .any(|key| old.contains_key(*key))
                || custom_fields()
                    .iter()
                    .any(|(name, _)| old.keys().any(|key| key.eq_ignore_ascii_case(name)))
        } else {
            old.keys()
                .any(|key| key.as_bytes().first().is_some_and(u8::is_ascii_uppercase))
        };
        let mut result = old.clone();
        for (key, incoming) in new {
            if file.eq_ignore_ascii_case("ClipConfig.json")
                && parent.is_empty()
                && key.eq_ignore_ascii_case("clipListCustomAppearance")
                && legacy
                && incoming.is_object()
                && matching_key(file, old, key)?.is_none()
            {
                for (flat, nested) in custom_fields() {
                    let Some(value) = incoming.get(*nested) else {
                        continue;
                    };
                    let name = matching_key(file, old, flat)?.unwrap_or_else(|| (*flat).into());
                    let path = format!("clipListCustomAppearance.{nested}");
                    let previous = old.get(&name).unwrap_or(&Value::Null);
                    let updated = if *nested == "hoverActions" {
                        let values = value.as_array().ok_or("自定义悬停操作必须是数组。")?;
                        let names = values
                            .iter()
                            .map(|item| item.as_str().ok_or("自定义悬停操作必须是字符串。"))
                            .collect::<Result<Vec<_>, _>>()?;
                        if previous.as_str().is_some_and(|text| {
                            text.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .eq(names.iter().copied())
                        }) {
                            previous.clone()
                        } else {
                            json!(names.join(","))
                        }
                    } else {
                        merge_preserving(file, &path, previous, value)?
                    };
                    result.insert(name, updated);
                }
                continue;
            }
            let original = matching_config_key(file, parent, old, key)?;
            let name = original.clone().unwrap_or_else(|| {
                if legacy {
                    old_style_key(file, key)
                } else {
                    key.clone()
                }
            });
            let path = if parent.is_empty() {
                key.clone()
            } else {
                format!("{parent}.{key}")
            };
            let previous = original
                .as_ref()
                .and_then(|key| old.get(key))
                .unwrap_or(&Value::Null);
            let value = if original.is_some() {
                merge_preserving(file, &path, previous, incoming)?
            } else {
                incoming.clone()
            };
            result.insert(name, value);
        }
        return Ok(Value::Object(result));
    }
    if let (Some(old), Some(new)) = (old.as_array(), new.as_array()) {
        let rows = new
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = item
                    .as_object()
                    .and_then(|obj| obj.get("id").or_else(|| obj.get("Id")));
                let previous = id
                    .and_then(|id| {
                        old.iter()
                            .find(|row| row.get("id").or_else(|| row.get("Id")) == Some(id))
                    })
                    .or_else(|| {
                        if id.is_none() && old.len() == new.len() {
                            old.get(index)
                        } else {
                            None
                        }
                    });
                let row_parent = if file.eq_ignore_ascii_case("GlobalConfig.json") {
                    parent
                } else {
                    ""
                };
                previous
                    .map(|row| merge_preserving(file, row_parent, row, item))
                    .unwrap_or_else(|| Ok(item.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Array(rows));
    }
    if old.is_null() && nullable_default(file, parent).as_ref() == Some(new) {
        return Ok(Value::Null);
    }
    if (enum_names(file, parent).is_some()
        || parent
            .to_ascii_lowercase()
            .starts_with("showwindowlocation")
        || parent.eq_ignore_ascii_case("ShowMenuLocation"))
        && !old.is_null()
        && !new.is_null()
    {
        if enum_value(file, parent, old)? == enum_value(file, parent, new)? {
            return Ok(old.clone());
        }
        if let Some(id) = numeric_enum(file, parent, new) {
            if old.is_number() {
                return Ok(json!(id));
            }
            if old.as_str().is_some_and(|s| s.parse::<i64>().is_ok()) {
                return Ok(json!(id.to_string()));
            }
        }
    }
    Ok(new.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dto::ClipConfigDto, extra_settings::GlobalAppConfig, settings_form::QuickPasteConfig,
    };

    fn fixture_directory() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/topic-42651/fixtures");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new().prefix("config-compat-").tempdir_in(root).unwrap()
    }

    fn fixture(name: &str, value: &Value, bom: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = fixture_directory();
        let path = dir.path().join(name);
        let mut bytes = if bom { BOM.to_vec() } else { vec![] };
        bytes.extend(serde_json::to_vec(value).unwrap());
        fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn assert_unique_global_appearance_wire(saved: &Value) {
        for (wire, rust) in [
            ("WindowBackground", "WindowBackgroundColor"),
            ("FontForeground", "FontForegroundColor"),
            ("WindowBackgroundImage", "WindowBackgroundImagePath"),
        ] {
            let names = saved.as_object().unwrap().keys()
                .filter(|name| name.eq_ignore_ascii_case(wire) || name.eq_ignore_ascii_case(rust))
                .map(String::as_str).collect::<Vec<_>>();
            assert_eq!(names, [wire]);
        }
    }

    #[test]
    fn global_appearance_aliases_read_csharp_values_without_rewriting_source() {
        for stale in ["#FF112233", "#FF445566"] {
            let original = json!({
                "WindowBackground":"#FF112233", "WindowBackgroundColor":stale,
                "FontForeground":"#FFEEEEEE", "fontForegroundColor":"#FF000000",
                "WindowBackgroundImage":"chosen.png", "WindowBackgroundImagePath":"stale.png",
                "Future":{"WindowBackground":"nested old", "WindowBackgroundColor":"nested new"}
            });
            let (_dir, path) = fixture("GlobalConfig.json", &original, true);
            let before = fs::read(&path).unwrap();
            let config: GlobalAppConfig = load(&path).unwrap();
            assert_eq!(config.window_background_color, "#FF112233");
            assert_eq!(config.font_foreground_color, "#FFEEEEEE");
            assert_eq!(config.window_background_image_path, "chosen.png");
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn global_appearance_alias_casing_respects_wire_group_then_exact_spelling() {
        for (source, expected) in [
            (json!({"windowBackground":"wire", "WindowBackgroundColor":"rust"}), "wire"),
            (json!({"WindowBackground":"exact", "windowBackground":"other", "WindowBackgroundColor":"rust"}), "exact"),
            (json!({"WindowBackgroundColor":"exact rust", "windowBackgroundColor":"other"}), "exact rust"),
            (json!({"windowBackground":"same", "WINDOWBACKGROUND":"same", "WindowBackgroundColor":"rust"}), "same"),
            (json!({"windowBackgroundColor":"same", "WINDOWBACKGROUNDCOLOR":"same"}), "same"),
        ] {
            let (_dir, path) = fixture("GlobalConfig.json", &source, false);
            assert_eq!(load::<GlobalAppConfig>(&path).unwrap().window_background_color, expected);
        }
        for source in [
            json!({"windowBackground":"first", "WINDOWBACKGROUND":"second", "WindowBackgroundColor":"rust"}),
            json!({"windowBackgroundColor":"first", "WINDOWBACKGROUNDCOLOR":"second"}),
        ] {
            let (_dir, path) = fixture("GlobalConfig.json", &source, false);
            let before = fs::read(&path).unwrap();
            assert!(load::<GlobalAppConfig>(&path).is_err());
            assert!(crate::config_file::save(&path, &GlobalAppConfig::default()).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn global_appearance_explicit_save_unifies_root_wire_names_and_preserves_extensions() {
        let original = json!({
            "windowBackground":"#FF111111", "WindowBackgroundColor":"#FF222222", "WINDOWBACKGROUNDCOLOR":"#FF333333",
            "FontForeground":"#FFEEEEEE", "fontForegroundColor":"#FF000000",
            "WindowBackgroundImage":"old.png", "windowBackgroundImagePath":"stale.png",
            "Future":{"WindowBackground":"nested old", "WindowBackgroundColor":"nested new",
                "rows":[{"FontForeground":"one", "FontForegroundColor":"two"}]}
        });
        let (_dir, path) = fixture("GlobalConfig.json", &original, true);
        let mut config: GlobalAppConfig = load(&path).unwrap();
        config.window_background_color = "#FF123456".into();
        config.font_foreground_color = "#FFFEDCBA".into();
        config.window_background_image_path = "chosen.png".into();
        crate::config_file::save(&path, &config).unwrap();
        assert!(fs::read(&path).unwrap().starts_with(BOM));
        let saved = read(&path).unwrap().unwrap();
        assert_unique_global_appearance_wire(&saved);
        assert_eq!(saved["WindowBackground"], "#FF123456");
        assert_eq!(saved["FontForeground"], "#FFFEDCBA");
        assert_eq!(saved["WindowBackgroundImage"], "chosen.png");
        assert_eq!(saved["Future"], original["Future"]);
        let reloaded: GlobalAppConfig = load(&path).unwrap();
        assert_eq!(reloaded.window_background_color, config.window_background_color);
        assert_eq!(reloaded.font_foreground_color, config.font_foreground_color);
        assert_eq!(reloaded.window_background_image_path, config.window_background_image_path);
        crate::config_file::save(&path, &reloaded).unwrap();
        assert_eq!(read(&path).unwrap().unwrap(), saved);
    }

    #[test]
    fn global_appearance_new_file_uses_one_disk_wire_name_while_api_names_stay_unchanged() {
        let dir = fixture_directory();
        let path = dir.path().join("GlobalConfig.json");
        let config = GlobalAppConfig::default();
        let api = serde_json::to_value(&config).unwrap();
        for name in ["WindowBackgroundColor", "FontForegroundColor", "WindowBackgroundImagePath"] {
            assert!(api.get(name).is_some());
        }
        assert!(api.get("WindowBackground").is_none());
        crate::config_file::save(&path, &config).unwrap();
        let saved = read(&path).unwrap().unwrap();
        assert_unique_global_appearance_wire(&saved);
        let direct: GlobalAppConfig = serde_json::from_value(saved).unwrap();
        assert_eq!(serde_json::to_value(direct).unwrap(), api);
        let unrelated = dir.path().join("OtherConfig.json");
        crate::config_file::save(&unrelated, &api).unwrap();
        assert_eq!(read(&unrelated).unwrap().unwrap(), api);
    }

    #[test]
    fn global_appearance_raw_save_respects_wire_authority_and_keeps_nested_scope_strict() {
        for existing in [false, true] {
            let dir = fixture_directory();
            let path = dir.path().join("GlobalConfig.json");
            if existing {
                fs::write(&path, br#"{"WindowBackground":"previous"}"#).unwrap();
            }
            crate::config_file::save(&path, &json!({
                "windowBackground":"wire", "WindowBackgroundColor":"stale rust"
            })).unwrap();
            let saved = read(&path).unwrap().unwrap();
            assert_eq!(saved, json!({"WindowBackground":"wire"}));
        }
        let source = json!({"FutureRows":[{
            "WindowBackground":"nested wire", "WindowBackgroundColor":"nested rust"
        }]});
        let (_dir, path) = fixture("GlobalConfig.json", &source, true);
        let before = fs::read(&path).unwrap();
        let update = json!({"FutureRows":[{"WindowBackgroundColor":"new nested value"}]});
        assert!(crate::config_file::save(&path, &update).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn global_appearance_aliases_keep_authoritative_nulls_and_reject_invalid_values() {
        let (_dir, path) = fixture("GlobalConfig.json", &json!({
            "WindowBackground":null, "WindowBackgroundColor":"#FF112233",
            "FontForeground":null, "FontForegroundColor":"#FFEEEEEE",
            "WindowBackgroundImage":null, "WindowBackgroundImagePath":"stale.png"
        }), true);
        let config: GlobalAppConfig = load(&path).unwrap();
        assert_eq!(config.window_background_color, "#FFFFFFFF");
        assert_eq!(config.font_foreground_color, "#FF000000");
        assert!(config.window_background_image_path.is_empty());
        crate::config_file::save(&path, &config).unwrap();
        let saved = read(&path).unwrap().unwrap();
        assert_unique_global_appearance_wire(&saved);
        for name in ["WindowBackground", "FontForeground", "WindowBackgroundImage"] {
            assert!(saved[name].is_null());
        }
        for source in [
            json!({"WindowBackground":42, "WindowBackgroundColor":"valid fallback must not win"}),
            json!({"FontForeground":false, "FontForegroundColor":"valid fallback must not win"}),
            json!({"WindowBackgroundImage":[], "WindowBackgroundImagePath":"valid fallback must not win"}),
            json!({"RunAsAdministrator":false, "runAsAdministrator":true}),
            json!({"LaunchAtStartup":true, "launchAtStartup":true}),
        ] {
            let (_dir, path) = fixture("GlobalConfig.json", &source, false);
            let before = fs::read(&path).unwrap();
            assert!(load::<GlobalAppConfig>(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        let (dir, path) = fixture("GlobalConfig.json", &json!({"HotkeyClipToggle":"Ctrl+F9", "HotkeyShowClipboard":"Ctrl+F10"}), false);
        let before = fs::read(&path).unwrap();
        assert!(load_hotkeys(dir.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn global_appearance_aliases_allow_isolated_host_startup_and_saved_restart() {
        crate::clipboard::force_headless(true);
        let dir = fixture_directory();
        let paths = crate::AppPaths::from_data_root(dir.path().join("data"));
        paths.ensure().unwrap();
        let path = paths.configs.join("GlobalConfig.json");
        let mut original = BOM.to_vec();
        original.extend(serde_json::to_vec(&json!({
            "WindowBackground":"#FF112233", "windowBackgroundColor":"#FF445566",
            "FontForeground":"#FFEEEEEE", "FontForegroundColor":"#FF000000",
            "WindowBackgroundImage":"", "WindowBackgroundImagePath":"stale.png",
            "Future":{"WindowBackgroundColor":"keep nested"}
        })).unwrap());
        fs::write(&path, &original).unwrap();
        let host = crate::HostState::open(paths.clone(), 28965).unwrap();
        let form = host.settings_form_get(&json!({"configId":"global"})).unwrap();
        let field = form["fields"].as_array().unwrap().iter().find(|field| field["key"] == "WindowBackgroundColor").unwrap();
        assert_eq!(field["value"], "#FF112233");
        assert_eq!(fs::read(&path).unwrap(), original);
        host.settings_form_update(&json!({"configId":"global", "values":{"WindowBackgroundColor":"#FFABCDEF"}})).unwrap();
        drop(host);
        let saved = read(&path).unwrap().unwrap();
        assert_unique_global_appearance_wire(&saved);
        assert_eq!(saved["WindowBackground"], "#FFABCDEF");
        assert_eq!(saved["Future"]["WindowBackgroundColor"], "keep nested");
        let restarted = crate::HostState::open(paths, 28965).unwrap();
        let form = restarted.settings_form_get(&json!({"configId":"global"})).unwrap();
        let field = form["fields"].as_array().unwrap().iter().find(|field| field["key"] == "WindowBackgroundColor").unwrap();
        assert_eq!(field["value"], "#FFABCDEF");
    }

    #[test]
    fn url_filter_default_migrates_once_preserving_custom_tabs_and_removal() {
        let original = json!({"FilterItems":[
            {"Id":"files","DisplayName":"工作文件","Type":"file","ItemType":8,"Future":7},
            {"Id":"text","DisplayName":"网址","Type":"text","ItemType":2},
            {"Id":"mixed","DisplayName":"混合","Type":"mask:129","ItemType":129}
        ],"FutureRoot":{"keep":true}});
        let (_dir, path) = fixture("ClipFilterConfig.json", &original, true);
        let tabs = load_filters(&path).unwrap();
        assert_eq!(tabs.iter().map(|tab| tab.id.as_str()).take(2).collect::<Vec<_>>(), ["files", "text"]);
        assert_eq!(tabs.len(), 4);
        assert_eq!(tabs[3].r#type, crate::clip_filter::TYPE_URL);
        assert!(tabs[3].display_name.is_empty());
        let migrated = read(&path).unwrap().unwrap();
        assert_eq!(migrated["FilterItems"][0], original["FilterItems"][0]);
        assert_eq!(migrated["FilterItems"][1], original["FilterItems"][1]);
        assert_eq!(migrated["FutureRoot"], original["FutureRoot"]);
        let once = fs::read(&path).unwrap();
        assert_eq!(load_filters(&path).unwrap()[3].id, tabs[3].id);
        assert_eq!(fs::read(&path).unwrap(), once);
        crate::config_file::save(&path, &json!({"FilterItems":original["FilterItems"]})).unwrap();
        assert_eq!(load_filters(&path).unwrap().len(), 3, "explicit removal must survive reload");
    }

    #[test]
    fn url_filter_default_does_not_duplicate_an_existing_custom_url_tab() {
        let original = json!({"filterItems":[
            {"id":"url","displayName":"My links","type":"URL","itemType":0}
        ],"rustUrlFilterDefaultAdded":false});
        let (_dir, path) = fixture("ClipFilterConfig.json", &original, false);
        let tabs = load_filters(&path).unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].display_name, "My links");
        assert_eq!(tabs[0].item_type, crate::clip_filter::URL);
        let saved = read(&path).unwrap().unwrap();
        assert_eq!(saved["filterItems"], original["filterItems"]);
        assert_eq!(saved["rustUrlFilterDefaultAdded"], true);
    }

    #[test]
    fn legacy_config_reads_bom_pascal_case_numeric_enums_and_flat_custom_without_cleanup_defaults()
    {
        let (_dir, path) = fixture(
            "ClipConfig.json",
            &json!({"MaxItemSize":9999,"AllowDuplicateCopy":true,
            "DismissBehavior":2,"ShowWindowLocation":5,"CopyToastPlacement":1,"ClipListLayoutDensity":1,
            "TextProcessExpresstion":"$$text","ClipListCustomChrome":2,"ClipListCustomLayout":1,
            "ClipListCustomShowProcess":true,"ClipListCustomHoverActions":"pin, edit","Future":{"keep":1}}),
            true,
        );
        let config: ClipConfigDto = load(&path).unwrap();
        assert_eq!(config.max_item_size, 9999);
        assert!(config.allow_duplicate_copy);
        assert_eq!(config.dismiss_behavior, "Fold");
        assert_eq!(config.show_window_location, "CenterScreen");
        assert_eq!(config.copy_toast_placement, "TopRight");
        assert_eq!(config.clip_list_layout_density, "relaxed");
        assert_eq!(config.text_process_expression, "$$text");
        let custom = config.clip_list_custom_appearance.unwrap();
        assert_eq!(custom.chrome, "flush");
        assert_eq!(custom.hover_actions, ["pin", "edit"]);
        assert!(custom.show_process);
    }

    #[test]
    fn legacy_config_saving_keeps_unknown_fields_bom_names_and_numeric_enum_representation() {
        let original = json!({"MaxItemSize":9999,"DismissBehavior":1,"CopyToastPlacement":0,
            "TextProcessExpresstion":"old","ClipListCustomChrome":1,"ClipListCustomHoverActions":"pin, edit",
            "Future":{"keep":[1,2,3]}});
        let (_dir, path) = fixture("ClipConfig.json", &original, true);
        let mut config: ClipConfigDto = load(&path).unwrap();
        config.max_item_size = 5000;
        config.dismiss_behavior = "Fold".into();
        config.text_process_expression = "new".into();
        crate::config_file::save(&path, &config).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(BOM));
        let result = parse(&path, &bytes).unwrap();
        assert_eq!(result["MaxItemSize"], 5000);
        assert_eq!(result["DismissBehavior"], 2);
        assert_eq!(result["CopyToastPlacement"], 0);
        assert_eq!(result["TextProcessExpresstion"], "new");
        assert_eq!(result["ClipListCustomChrome"], 1);
        assert_eq!(result["ClipListCustomHoverActions"], "pin, edit");
        assert_eq!(result["Future"], original["Future"]);
        assert!(result.get("maxItemSize").is_none());
        assert!(result.get("clipListCustomAppearance").is_none());
        assert_eq!(load::<ClipConfigDto>(&path).unwrap().max_item_size, 5000);
    }

    #[test]
    fn legacy_config_missing_new_preferences_does_not_discard_quick_paste_or_theme() {
        let (_dir, quick) = fixture(
            "QuickPasteConfig.json",
            &json!({"ListLimit":17,"RememberSelection":false,"Shortcut1Action":2}),
            true,
        );
        let config: QuickPasteConfig = load(&quick).unwrap();
        assert_eq!(config.list_limit, 17);
        assert!(!config.remember_selection);
        assert_eq!(config.shortcut1_action, "Open");
        let (dir, global) = fixture(
            "GlobalConfig.json",
            &json!({"UiThemeMode":2,"WindowBackground":"#FF121212",
            "FontForeground":"#FFF0F0F0","WindowBackgroundImage":"image.png","HotkeyClipToggle":"Ctrl+F12",
            "HotkeyFavorToggle":"","HotkeyQuickPaste":"Alt+Oem3","UnknownGlobal":true}),
            true,
        );
        let config: GlobalAppConfig = load(&global).unwrap();
        assert_eq!(config.ui_theme_mode, "Dark");
        assert_eq!(config.window_background_color, "#FF121212");
        assert_eq!(config.window_background_image_path, "image.png");
        assert_eq!(
            load_hotkeys(dir.path()).unwrap().hotkey_show_clipboard,
            "Ctrl+F12"
        );
        crate::config_file::save(&global, &config).unwrap();
        let result = read(&global).unwrap().unwrap();
        assert_eq!(result["UiThemeMode"], 2);
        assert_eq!(result["WindowBackground"], "#FF121212");
        assert_eq!(result["UnknownGlobal"], true);
        assert!(result.get("WindowBackgroundColor").is_none());
    }

    #[test]
    fn legacy_config_invalid_existing_values_are_errors_and_cannot_be_overwritten() {
        for source in [
            json!({"MaxItemSize":"bad"}),
            json!({"MaxItemSize":12,"maxItemSize":9999}),
            json!({"DismissBehavior":999}),
        ] {
            let (_dir, path) = fixture("ClipConfig.json", &source, false);
            assert!(load::<ClipConfigDto>(&path).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ClipConfig.json");
        fs::write(&path, b"broken JSON").unwrap();
        assert!(load::<ClipConfigDto>(&path).is_err());
        assert!(crate::config_file::save(&path, &ClipConfigDto::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken JSON");
        assert_eq!(
            load::<ClipConfigDto>(&dir.path().join("missing.json"))
                .unwrap()
                .max_item_size,
            ClipConfigDto::default().max_item_size
        );
    }

    #[test]
    fn legacy_config_array_edits_preserve_unknown_fields_by_item_identity() {
        let original = json!({"FilterItems":[{"Id":"a","DisplayName":"A","Extra":1},{"Id":"b","DisplayName":"B","Extra":2}],"Future":3});
        let (_dir, path) = fixture("ClipFilterConfig.json", &original, false);
        crate::config_file::save(
            &path,
            &json!({"FilterItems":[{"id":"b","displayName":"B edited"}]}),
        )
        .unwrap();
        let result = read(&path).unwrap().unwrap();
        assert_eq!(result["FilterItems"].as_array().unwrap().len(), 1);
        assert_eq!(result["FilterItems"][0]["Id"], "b");
        assert_eq!(result["FilterItems"][0]["DisplayName"], "B edited");
        assert_eq!(result["FilterItems"][0]["Extra"], 2);
        assert_eq!(result["Future"], 3);
    }

    #[test]
    fn legacy_config_native_camel_case_and_nested_extensions_keep_their_existing_shape() {
        let original = json!({"maxItemSize":400,"Future":true,"clipMonitorOn":false,
            "clipListCustomAppearance":{"chrome":"hairline","layout":"minimal","showProcess":false,
                "showTime":false,"hoverActions":["pin"],"FutureNested":123}});
        let (_dir, path) = fixture("ClipConfig.json", &original, false);
        let mut config: ClipConfigDto = load(&path).unwrap();
        config
            .clip_list_custom_appearance
            .as_mut()
            .unwrap()
            .show_process = true;
        crate::config_file::save(&path, &config).unwrap();
        let result = read(&path).unwrap().unwrap();
        assert_eq!(result["maxItemSize"], 400);
        assert!(result.get("MaxItemSize").is_none());
        assert!(result.get("ClipListCustomChrome").is_none());
        assert!(result.get("clipMonitorOn").is_none());
        assert_eq!(result["clipListCustomAppearance"]["FutureNested"], 123);
        assert!(
            load::<ClipConfigDto>(&path)
                .unwrap()
                .clip_list_custom_appearance
                .unwrap()
                .show_process
        );
    }

    #[test]
    fn legacy_config_filter_actions_and_file_menu_read_bom_numeric_enums_and_keep_extensions() {
        let (_dir, filter) = fixture(
            "ClipFilterConfig.json",
            &json!({"FilterItems":[{"Id":"my-tab","DisplayName":"My Text",
            "Type":"text","ItemType":2,"Future":123}]}),
            true,
        );
        assert_eq!(load_filters(&filter).unwrap()[0].id, "my-tab");
        let (_dir, actions) = fixture(
            "TextActionData.json",
            &json!({"Actions":[{"Type":6,"Title":"Wrap",
            "Data":{"Prefix":"[","Suffix":"]","FutureData":1},"FutureAction":2}],"FutureRoot":3}),
            true,
        );
        let loaded = load_actions(&actions).unwrap();
        assert_eq!(loaded[0]["Type"], "Pair");
        crate::text_actions::save(&actions, &loaded).unwrap();
        let result = read(&actions).unwrap().unwrap();
        assert_eq!(result["Actions"][0]["Type"], 6);
        assert_eq!(result["Actions"][0]["FutureAction"], 2);
        assert_eq!(result["Actions"][0]["Data"]["FutureData"], 1);
        assert_eq!(result["FutureRoot"], 3);
        let (dir, menu) = fixture(
            "FileMenuConfig.json",
            &json!({"FileSortType":4,"ShowType":1,"Files":"C:\\test",
            "RustCustomPathsVersion":1,"RustOpenedFolders":["C:\\open"],"Future":true}),
            true,
        );
        let (settings, mut paths) =
            crate::file_menu::load_config(&menu, &dir.path().join("missing.json")).unwrap();
        assert_eq!(settings.file_sort_type, "editTime");
        assert_eq!(settings.show_type, "dir");
        assert_eq!(paths.opened_folders, ["C:\\open"]);
        paths.opened_folders.clear();
        crate::file_menu::save_config(&paths, &settings, &menu).unwrap();
        let result = read(&menu).unwrap().unwrap();
        assert_eq!(result["FileSortType"], 4);
        assert_eq!(result["ShowType"], 1);
        assert_eq!(result["Future"], true);
        assert!(result.get("RustOpenedFolders").is_none());
    }

    #[test]
    fn legacy_config_global_nulls_follow_csharp_defaults_and_disabled_quick_paste_semantics() {
        let original = json!({"UiThemeMode":null,"WindowBackground":null,"FontForeground":null,
            "WindowBackgroundImage":null,"HotkeyClipToggle":null,"HotkeyFavorToggle":null,"HotkeyQuickPaste":null,
            "Future":null});
        let (dir, path) = fixture("GlobalConfig.json", &original, true);
        let config: GlobalAppConfig = load(&path).unwrap();
        assert_eq!(config.ui_theme_mode, "Auto");
        assert_eq!(config.window_background_color, "#FFFFFFFF");
        assert_eq!(config.font_foreground_color, "#FF000000");
        assert_eq!(config.window_background_image_path, "");
        let hotkeys = load_hotkeys(dir.path()).unwrap();
        assert_eq!(
            hotkeys.hotkey_show_clipboard,
            crate::global_hotkey::DEFAULT_CLIP_TOGGLE
        );
        assert_eq!(
            hotkeys.hotkey_favor_toggle,
            crate::global_hotkey::DEFAULT_FAVOR_TOGGLE
        );
        assert_eq!(hotkeys.hotkey_quick_paste, "");
        crate::config_file::save(&path, &config).unwrap();
        let result = read(&path).unwrap().unwrap();
        for key in original.as_object().unwrap().keys() {
            assert_eq!(result[key], Value::Null, "{key}");
        }
        assert!(load::<GlobalAppConfig>(&path).is_ok());
        let (dir, path) = fixture("FileMenuConfig.json", &json!({"Files":null}), false);
        let (settings, paths) =
            crate::file_menu::load_config(&path, &dir.path().join("missing.json")).unwrap();
        assert!(paths.lines.is_empty());
        assert!(settings.files.is_empty());
        assert_eq!(read(&path).unwrap().unwrap()["Files"], Value::Null);
    }

    #[test]
    fn super_menu_hotkey_loads_from_global_config_and_preserves_explicit_disable() {
        let (dir, path) = fixture("GlobalConfig.json", &json!({"HotkeySuperMenu":"Alt+F10"}), false);
        assert_eq!(load_hotkeys(dir.path()).unwrap().hotkey_super_menu, "Alt+F10");
        for disabled in [json!(""), Value::Null] {
            fs::write(&path, serde_json::to_vec(&json!({"HotkeySuperMenu":disabled})).unwrap()).unwrap();
            assert_eq!(load_hotkeys(dir.path()).unwrap().hotkey_super_menu, "");
        }
        fs::write(&path, br#"{"HotkeyQuickPaste":""}"#).unwrap();
        assert_eq!(load_hotkeys(dir.path()).unwrap().hotkey_super_menu, crate::global_hotkey::DEFAULT_SUPER_MENU);
    }
}
