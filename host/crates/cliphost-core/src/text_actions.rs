//! Text actions aligned with C# `TextActionAppService` + `TextActionData.json`.

use serde_json::{json, Value};

mod regex_replace;
mod regex_pattern;
mod regex_character_class;
mod regex_ecmascript;
mod regex_advanced;
mod regex_directional;
mod regex_mask;
mod regex_captures;
mod regex_capture_stack;
mod regex_linear;

const TYPE_CATALOG: &[(&str, &str, i32, bool)] = &[
    ("Internal", "内置操作", 1, true),
    ("SingleReplace", "单次替换", 2, true),
    ("ZExpression", "表达式", 4, true),
    ("QuickerAction", "自定义动作", 5, true),
    ("Pair", "对符（前后缀）", 6, true),
    ("Send", "发送文本", 7, false),
];

const INTERNAL_TYPES: &[(&str, &str)] = &[
    ("RemoveEmptyLine", "去除空行"),
    ("RemoveWhiteSpaceLine", "去除空白行"),
    ("StandardText", "规范粘贴"),
    ("FormatJson", "格式化Json"),
    ("ZipJson", "压缩Json"),
];

const REGEX_OPTIONS: &[(&str, &str)] = &[
    ("IgnoreCase", "忽略大小写"),
    ("Multiline", "多行模式"),
    ("Singleline", "单行模式"),
    ("RightToLeft", "从右向左"),
];

pub fn type_options() -> Vec<Value> {
    TYPE_CATALOG
        .iter()
        .map(|(id, label, _, need)| json!({ "id": id, "label": label, "needInput": need }))
        .collect()
}

pub fn internal_type_options() -> Vec<Value> {
    INTERNAL_TYPES
        .iter()
        .map(|(id, label)| json!({ "id": id, "label": label }))
        .collect()
}

pub fn regex_options() -> Vec<Value> {
    REGEX_OPTIONS
        .iter()
        .map(|(id, label)| json!({ "id": id, "label": label }))
        .collect()
}

pub fn list_response(actions: &[Value]) -> Value {
    let items: Vec<Value> = actions
        .iter()
        .enumerate()
        .map(|(index, action)| to_dto(action, &index.to_string()))
        .collect();
    json!({
        "actions": items,
        "types": type_options(),
        "internalTypes": internal_type_options(),
        "regexOptions": regex_options(),
    })
}

pub fn load(path: &std::path::Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).ok().unwrap_or_default();
    if text.trim().is_empty() {
        return Vec::new();
    }
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    parsed
        .get("Actions")
        .or_else(|| parsed.get("actions"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

pub fn save(path: &std::path::Path, actions: &[Value]) -> Result<(), String> {
    crate::config_file::save(path, &json!({ "Actions": actions }))
}

pub fn upsert(actions: &mut Vec<Value>, params: &Value) -> Result<Value, String> {
    // C# `TextActionAppService.Upsert`: missing Type → `Unsupported text action type: `.
    let type_name = params
        .get("type")
        .or_else(|| params.get("Type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if type_name.eq_ignore_ascii_case("NA") || type_name.eq_ignore_ascii_case("MultiReplace") {
        return Err(format!("Unsupported text action type: {type_name}"));
    }
    let meta = TYPE_CATALOG
        .iter()
        .find(|(id, _, _, _)| id.eq_ignore_ascii_case(type_name))
        .ok_or_else(|| format!("Unsupported text action type: {type_name}"))?;
    let mut stored = to_stored(params, meta.0, meta.2);
    let id = params.get("id").and_then(|v| v.as_str());
    if let Some(token) = crate::text_actions_ui::expected_rule_token(params)? {
        crate::text_actions_ui::checked_action(actions, id.unwrap_or(""), token)?;
    }
    if let Some(index) = resolve_index(id, actions.len()) {
        // Common is independent UI metadata. An editor opened before a star
        // toggle must not restore its stale copy when saving the rule itself.
        if let Some(common) = actions[index].get("Common").or_else(|| actions[index].get("common")) {
            stored["Common"] = common.clone();
        }
        actions[index] = stored;
        return Ok(to_dto(&actions[index], &index.to_string()));
    }
    actions.push(stored);
    let index = actions.len() - 1;
    Ok(to_dto(&actions[index], &index.to_string()))
}

pub fn delete(actions: &mut Vec<Value>, id: &str) -> Result<(), String> {
    let index = resolve_index(Some(id), actions.len()).ok_or_else(|| "Text action not found.".to_string())?;
    actions.remove(index);
    Ok(())
}

pub fn get_by_id<'a>(actions: &'a [Value], id: &str) -> Option<&'a Value> {
    resolve_index(Some(id), actions.len()).map(|index| &actions[index])
}

pub fn visual_title(action: &Value) -> String {
    let title = take_str(action, "Title", "title");
    if !title.is_empty() {
        return title;
    }
    visual_tips(action)
        .lines()
        .next()
        .unwrap_or("文本操作")
        .to_string()
}

pub fn visual_tips(action: &Value) -> String {
    let description = take_str(action, "Description", "description");
    if !description.is_empty() {
        return description;
    }
    let tips = take_str(action, "Tips", "tips");
    if !tips.is_empty() {
        return tips;
    }
    let type_name = normalize_type(
        action
            .get("Type")
            .or_else(|| action.get("type"))
            .unwrap_or(&Value::Null),
    );
    let meta = TYPE_CATALOG
        .iter()
        .find(|(id, _, _, _)| *id == type_name)
        .copied()
        .unwrap_or(("SingleReplace", "单次替换", 2, true));
    let data = action
        .get("Data")
        .or_else(|| action.get("data"))
        .cloned()
        .unwrap_or_else(|| default_data(meta.0));
    summary(&data, meta.0, meta.1)
}

fn resolve_index(id: Option<&str>, count: usize) -> Option<usize> {
    let raw = id?.trim();
    if raw.is_empty() {
        return None;
    }
    let index = raw.parse::<usize>().ok()?;
    (index < count).then_some(index)
}

fn to_stored(params: &Value, type_name: &str, obj_type: i32) -> Value {
    let mut data = default_data(type_name);
    if let Some(incoming) = params.get("data").or_else(|| params.get("Data")).and_then(|v| v.as_object()) {
        if let Some(obj) = data.as_object_mut() {
            for (key, value) in incoming {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(obj) = data.as_object_mut() {
        obj.insert("ObjType".into(), json!(obj_type));
    }
    json!({
        "Title": params.get("title").or_else(|| params.get("Title")).and_then(|v| v.as_str()).unwrap_or("").trim(),
        "Description": params.get("description").or_else(|| params.get("Description")).and_then(|v| v.as_str()).unwrap_or("").trim(),
        "Type": type_name,
        "Simulate": params.get("simulate").or_else(|| params.get("Simulate")).and_then(|v| v.as_bool()).unwrap_or(false),
        "SimulateDelay": params
            .get("simulateDelay")
            .or_else(|| params.get("SimulateDelay"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .clamp(0, 100),
        "Group": params.get("group").or_else(|| params.get("Group")).and_then(|v| v.as_str()).unwrap_or("").trim(),
        "Data": data,
    })
}

pub(crate) fn to_dto(action: &Value, id: &str) -> Value {
    let type_name = normalize_type(
        action
            .get("Type")
            .or_else(|| action.get("type"))
            .unwrap_or(&Value::Null),
    );
    let meta = TYPE_CATALOG
        .iter()
        .find(|(name, _, _, _)| *name == type_name)
        .copied()
        .unwrap_or(("SingleReplace", "单次替换", 2, true));
    let data = action.get("Data").or_else(|| action.get("data")).cloned().unwrap_or_else(|| default_data(meta.0));
    json!({
        "id": id,
        "title": take_str(action, "Title", "title"),
        "description": take_str(action, "Description", "description"),
        "type": meta.0,
        "typeLabel": meta.1,
        "simulate": action.get("Simulate").or_else(|| action.get("simulate")).and_then(|v| v.as_bool()).unwrap_or(false),
        "simulateDelay": action.get("SimulateDelay").or_else(|| action.get("simulateDelay")).and_then(|v| v.as_i64()).unwrap_or(0),
        "group": take_str(action, "Group", "group"),
        "summary": summary(&data, meta.0, meta.1),
        "needInput": needs_input(action),
        "common": crate::text_actions_ui::is_common(action),
        "ruleToken": crate::text_actions_ui::rule_token(action),
        "data": data,
    })
}

fn normalize_type(value: &Value) -> &'static str {
    if let Some(name) = value.as_str() {
        return TYPE_CATALOG
            .iter()
            .find(|(id, _, _, _)| id.eq_ignore_ascii_case(name))
            .map(|(id, _, _, _)| *id)
            .unwrap_or("SingleReplace");
    }
    if let Some(n) = value.as_i64() {
        return TYPE_CATALOG
            .iter()
            .find(|(_, _, code, _)| *code == n as i32)
            .map(|(id, _, _, _)| *id)
            .unwrap_or("SingleReplace");
    }
    "SingleReplace"
}

fn default_data(type_name: &str) -> Value {
    match type_name {
        "Internal" => json!({ "ObjType": 1, "Type": "RemoveEmptyLine" }),
        "SingleReplace" => json!({
            "ObjType": 2,
            "Pattern": "",
            "ReplaceMent": "",
            "EscapePattern": false,
            "EscapeReplaceMent": true,
            "UseRegex": false,
            "Options": 0
        }),
        "ZExpression" => json!({ "ObjType": 4, "Expresstion": "$={text}" }),
        "QuickerAction" => json!({ "ObjType": 5, "Id": "", "WaitResult": false }),
        "Pair" => json!({
            "ObjType": 6,
            "Prefix": "",
            "Suffix": "",
            "MultiLines": false,
            "Trim": false
        }),
        "Send" => json!({ "ObjType": 7, "Text": "" }),
        _ => json!({ "ObjType": 2, "Pattern": "", "ReplaceMent": "" }),
    }
}

fn summary(data: &Value, type_name: &str, type_label: &str) -> String {
    match type_name {
        "Internal" => {
            let id = data.get("Type").and_then(|v| v.as_str()).unwrap_or("RemoveEmptyLine");
            INTERNAL_TYPES
                .iter()
                .find(|(name, _)| *name == id)
                .map(|(_, label)| (*label).to_string())
                .unwrap_or_else(|| id.to_string())
        }
        "SingleReplace" => data
            .get("Pattern")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(type_label)
            .to_string(),
        "Pair" => {
            let prefix = data.get("Prefix").and_then(|v| v.as_str()).unwrap_or("");
            let suffix = data.get("Suffix").and_then(|v| v.as_str()).unwrap_or("");
            format!("{prefix}…{suffix}")
        }
        "Send" => data.get("Text").and_then(|v| v.as_str()).unwrap_or(type_label).to_string(),
        _ => type_label.to_string(),
    }
}

pub fn needs_input(action: &Value) -> bool {
    let type_name = normalize_type(
        action
            .get("Type")
            .or_else(|| action.get("type"))
            .unwrap_or(&Value::Null),
    );
    if type_name == "ZExpression" {
        let expression = action.get("Data").or_else(|| action.get("data"))
            .map(expression_text).unwrap_or("$={text}");
        return expression.contains("{text}") || expression.contains("{list}");
    }
    TYPE_CATALOG
        .iter()
        .find(|(id, _, _, _)| *id == type_name)
        .map(|(_, _, _, need)| *need)
        .unwrap_or(true)
}

pub fn apply(action: &Value, text: &str) -> Result<String, String> {
    apply_with_action_runner(action, text, |id, input| {
        crate::quicker_action::try_run_quicker_action(id, input).map(|_| ())
    })
}

pub(crate) fn apply_with_action_runner(
    action: &Value,
    text: &str,
    run_action: impl FnOnce(&str, &str) -> Result<(), String>,
) -> Result<String, String> {
    apply_with_runners(action, text, run_action, |expression, text| {
        let value = crate::quicker_expression::local(expression, &crate::quicker_expression::text_params(text))
            .ok_or("无法计算表达式：Quicker 插件未连接。")?;
        crate::quicker_expression::text_result(value)
    })
}

pub(crate) fn apply_with_runners(
    action: &Value, text: &str,
    run_action: impl FnOnce(&str, &str) -> Result<(), String>,
    evaluate: impl FnOnce(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let type_name = normalize_type(
        action
            .get("Type")
            .or_else(|| action.get("type"))
            .unwrap_or(&Value::Null),
    );
    let data = action
        .get("Data")
        .or_else(|| action.get("data"))
        .cloned()
        .unwrap_or_else(|| default_data(type_name));
    match type_name {
        "Internal" => apply_internal(&data, text),
        "SingleReplace" => apply_single_replace(&data, text),
        "Pair" => apply_pair(&data, text),
        "Send" => Ok(data.get("Text").and_then(|v| v.as_str()).unwrap_or("").to_string()),
        "ZExpression" => evaluate(expression_text(&data), text),
        "QuickerAction" => {
            // URI actions have no result transport. Keep the existing unchanged-text output,
            // but never paste it if launching the configured action failed.
            let id = data.get("Id").and_then(|v| v.as_str()).unwrap_or("");
            if !id.is_empty() {
                run_action(id, text)?;
            }
            Ok(text.to_string())
        }
        other => Err(format!("Unsupported text action type: {other}")),
    }
}

/// C# `ClipHostQuickerZExpressionService.TryTransformText` + `TextClipItem.PreProcess`.
/// Local-only fallback. Failed/empty/non-string results preserve the original text.
pub fn try_transform_clipboard_text(text: &str, expression: &str) -> Option<String> {
    if expression.trim().is_empty() {
        return None;
    }
    let result = crate::quicker_expression::local(expression, &json!({"text":text}))?;
    let result = result.as_str()?.to_string();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// C# `ClipItemFactory.CreateFromClipboard`: `PreProcess` then `CanCollect` (Html/QQ inherit Text).
pub fn apply_text_clip_preprocess(
    item: &mut crate::model::ClipRecord,
    expression: &str,
    ignore_white_space: bool,
) -> bool {
    apply_text_clip_preprocess_with(item, expression, ignore_white_space, &try_transform_clipboard_text)
}

pub(crate) fn apply_text_clip_preprocess_with(
    item: &mut crate::model::ClipRecord, expression: &str, ignore_white_space: bool,
    transform: &dyn Fn(&str, &str) -> Option<String>,
) -> bool {
    use crate::model::{hash_text, ClipKind};
    if matches!(
        item.kind,
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
    ) {
        if let Some(text) = item.body_text.clone() {
            if let Some(next) = transform(&text, expression) {
                item.body_text = Some(next.clone());
                item.content_hash = hash_text(&next);
            }
        }
    }
    csharp_can_collect(item, ignore_white_space)
}

fn csharp_can_collect(item: &crate::model::ClipRecord, ignore_white_space: bool) -> bool {
    use crate::model::ClipKind;
    match item.kind {
        ClipKind::TextClipItem => {
            let text = item.body_text.as_deref().unwrap_or("");
            !text.is_empty() && !(ignore_white_space && text.trim().is_empty())
        }
        ClipKind::HtmlClipItem => !item.body_text.as_deref().unwrap_or("").is_empty(),
        ClipKind::QQRichTextItem => {
            !item.qq_rich_text.as_deref().unwrap_or("").is_empty()
                || !item.body_text.as_deref().unwrap_or("").is_empty()
        }
        _ => true,
    }
}

fn expression_text(data: &Value) -> &str {
    data.get("Expresstion").or_else(|| data.get("Expression"))
        .or_else(|| data.get("expresstion")).and_then(Value::as_str).unwrap_or("$={text}")
}

fn apply_internal(data: &Value, text: &str) -> Result<String, String> {
    let kind = data.get("Type").and_then(|v| v.as_str()).unwrap_or("RemoveEmptyLine");
    match kind {
        "RemoveEmptyLine" => Ok(map_lines(text, |line| (!line.is_empty()).then_some(line.to_string()))),
        "RemoveWhiteSpaceLine" => Ok(map_lines(text, |line| (!line.trim().is_empty()).then_some(line.to_string()))),
        "StandardText" => Ok(standard_text(text)),
        "FormatJson" => {
            let parsed: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&parsed).map_err(|e| e.to_string())
        }
        "ZipJson" => {
            let parsed: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            serde_json::to_string(&parsed).map_err(|e| e.to_string())
        }
        other => Err(format!("Unsupported internal text action: {other}")),
    }
}

fn apply_single_replace(data: &Value, text: &str) -> Result<String, String> {
    let pattern = unescape(
        data.get("Pattern").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("EscapePattern").and_then(|v| v.as_bool()).unwrap_or(false),
    );
    let replacement = unescape(
        data.get("ReplaceMent").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("EscapeReplaceMent").and_then(|v| v.as_bool()).unwrap_or(true),
    );
    if data.get("UseRegex").and_then(|v| v.as_bool()).unwrap_or(false) {
        let options = data.get("Options").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        // Compiled is a .NET execution preference. Validate semantic options
        // and incompatible combinations before choosing a matching engine.
        const SUPPORTED: i32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400;
        if options & !SUPPORTED != 0 {
            return Err(format!("当前 Rust 正则引擎不支持选项 0x{:X}，已停止替换。", options & !SUPPORTED));
        }
        if options & 0x100 != 0 && options & !(0x100 | 0x1 | 0x2 | 0x8 | 0x200) != 0 {
            return Err("ECMAScript 不能与此正则选项组合，已停止替换。".into());
        }
        if options & 0x400 != 0 && options & 0x40 != 0 {
            return Err("非回溯模式不能与从右向左模式同时使用，已停止替换。".into());
        }
        let re = regex_pattern::compile(&pattern, options)?;
        regex_replace::replace(&re, text, &replacement, options & 0x40 != 0, options & 0x100 != 0)
    } else if pattern.is_empty() {
        Err("查找内容不能为空。".into())
    } else {
        Ok(text.replace(&pattern, &replacement))
    }
}

fn apply_pair(data: &Value, text: &str) -> Result<String, String> {
    let prefix = data.get("Prefix").and_then(|v| v.as_str()).unwrap_or("");
    let suffix = data.get("Suffix").and_then(|v| v.as_str()).unwrap_or("");
    let mut body = text.to_string();
    if data.get("Trim").and_then(|v| v.as_bool()).unwrap_or(false) {
        body = body.trim().to_string();
    }
    if data.get("MultiLines").and_then(|v| v.as_bool()).unwrap_or(false) {
        // C# WrapPair: split CRLF / CR / LF, omit empty lines and join with CRLF.
        Ok(body.split(['\r', '\n']).filter(|line| !line.is_empty())
            .map(|line| format!("{prefix}{line}{suffix}"))
            .collect::<Vec<_>>().join("\r\n"))
    } else {
        Ok(format!("{prefix}{body}{suffix}"))
    }
}

fn unescape(text: &str, enabled: bool) -> String {
    if !enabled || text.is_empty() {
        return text.to_string();
    }
    text.replace("\\r", "\r").replace("\\n", "\n").replace("\\t", "\t")
}

fn map_lines(text: &str, keep: impl Fn(&str) -> Option<String>) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    text.split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter_map(|line| keep(line))
        .collect::<Vec<_>>()
        .join(newline)
}

fn standard_text(text: &str) -> String {
    crate::layout::try_text_split(text)
}

fn take_str(value: &Value, pascal: &str, camel: &str) -> String {
    value
        .get(pascal)
        .or_else(|| value.get(camel))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_matches_frontend_contract() {
        let mut actions = Vec::new();
        let saved = upsert(
            &mut actions,
            &json!({
                "title": "去空行",
                "type": "Internal",
                "data": { "Type": "RemoveEmptyLine" }
            }),
        )
        .unwrap();
        assert_eq!(saved["id"], "0");
        let list = list_response(&actions);
        assert_eq!(list["actions"][0]["type"], "Internal");
        assert_eq!(list["actions"][0]["typeLabel"], "内置操作");
        assert!(list["types"].as_array().unwrap().iter().any(|t| t["id"] == "SingleReplace"));
        assert!(list["internalTypes"].as_array().unwrap().iter().any(|t| t["id"] == "FormatJson"));
        assert!(list["regexOptions"].as_array().unwrap().iter().any(|t| t["id"] == "IgnoreCase"));
        assert_eq!(
            upsert(&mut actions, &json!({})).unwrap_err(),
            "Unsupported text action type: "
        );
        assert_eq!(
            upsert(&mut actions, &json!({ "type": "MultiReplace" })).unwrap_err(),
            "Unsupported text action type: MultiReplace"
        );
        assert_eq!(delete(&mut actions, "").unwrap_err(), "Text action not found.");
        delete(&mut actions, "0").unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn apply_internal_and_replace() {
        let empty = apply(
            &json!({ "Type": "Internal", "Data": { "Type": "RemoveEmptyLine" } }),
            "a\n\nb",
        )
        .unwrap();
        assert_eq!(empty, "a\nb");
        let pretty = apply(
            &json!({ "Type": "Internal", "Data": { "Type": "FormatJson" } }),
            "{\"a\":1}",
        )
        .unwrap();
        assert!(pretty.contains('\n'));
        let swapped = apply(
            &json!({
                "Type": "SingleReplace",
                "Data": { "Pattern": "foo", "ReplaceMent": "bar", "EscapeReplaceMent": false, "UseRegex": false }
            }),
            "foo-foo",
        )
        .unwrap();
        assert_eq!(swapped, "bar-bar");
        let paired = apply(
            &json!({ "Type": "Pair", "Data": { "Prefix": "[", "Suffix": "]", "MultiLines": true } }),
            "a\nb",
        )
        .unwrap();
        assert_eq!(paired, "[a]\r\n[b]");
        crate::clipboard::force_headless(true);
        let quicker = apply(
            &json!({ "Type": "QuickerAction", "Data": { "Id": "act-1" } }),
            "keep-me",
        )
        .unwrap();
        assert_eq!(quicker, "keep-me");
        let empty_id = apply(
            &json!({ "Type": "QuickerAction", "Data": { "Id": "" } }),
            "keep-me",
        )
        .unwrap();
        assert_eq!(empty_id, "keep-me");
        let identity = apply(&json!({ "Type": "ZExpression" }), "hello").unwrap();
        assert_eq!(identity, "hello");
        let interpolated = apply(
            &json!({ "Type": "ZExpression", "Data": { "Expresstion": "$${text}!" } }),
            "hi",
        )
        .unwrap();
        assert_eq!(interpolated, "hi!");
        let listed = apply(
            &json!({ "Type": "ZExpression", "Data": { "Expresstion": "$={list}" } }),
            "a\n\nb",
        )
        .unwrap();
        assert_eq!(listed, "a\r\nb");
        let complex = apply(
            &json!({ "Type": "ZExpression", "Data": { "Expresstion": "$=1+1" } }),
            "x",
        )
        .unwrap_err();
        assert!(complex.contains("Quicker"));
        crate::clipboard::force_headless(true);
        let _clip = crate::clipboard::install_test_snapshot(crate::clipboard::ClipboardSnapshot {
            text: Some("from-clip".into()),
            ..crate::clipboard::ClipboardSnapshot::default()
        });
        let cliptext = apply(
            &json!({
                "Type": "ZExpression",
                "Data": { "Expresstion": "$${[cliptext]}" }
            }),
            "ignored",
        )
        .unwrap();
        assert_eq!(cliptext, "from-clip");
    }

    #[test]
    fn text_clip_preprocess_matches_csharp_preprocess_and_can_collect() {
        use crate::model::{ClipKind, ClipRecord};
        let mut item = ClipRecord::new_text("hello", "notepad.exe");
        assert!(apply_text_clip_preprocess(&mut item, "$${text}!", false));
        assert_eq!(item.body_text.as_deref(), Some("hello!"));
        let mut kept = ClipRecord::new_text("keep", "notepad.exe");
        assert!(apply_text_clip_preprocess(&mut kept, "$=1+1", false));
        assert_eq!(kept.body_text.as_deref(), Some("keep"));
        let mut blank = ClipRecord::new_text("   ", "notepad.exe");
        assert!(!apply_text_clip_preprocess(&mut blank, "", true));
        let mut html = ClipRecord::new_text("   ", "chrome.exe");
        html.kind = ClipKind::HtmlClipItem;
        assert!(apply_text_clip_preprocess(&mut html, "", true));
    }
}
