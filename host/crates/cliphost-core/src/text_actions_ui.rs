//! Pure models for the native text-action manager and execution menu.
//!
//! Positional action IDs are retained for configuration compatibility. Every
//! delayed native operation also carries the rule token returned by the list.
//! A checked snapshot must be executed directly, never looked up by index again.

use serde_json::{json, Value};

use crate::{i18n::tr, text_actions};

pub fn is_common(action: &Value) -> bool {
    action.get("Common").or_else(|| action.get("common"))
        .and_then(Value::as_bool).unwrap_or(false)
}

/// Hash all original rule data, including unknown fields. Star changes are
/// intentionally independent so that they cannot invalidate an open draft.
pub fn rule_token(action: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut entries: Vec<_> = object.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                Value::Object(entries.into_iter().map(|(key, value)| (key.clone(), canonical(value))).collect())
            }
            Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
            value => value.clone(),
        }
    }
    let mut rule = action.clone();
    if let Some(rule) = rule.as_object_mut() {
        rule.remove("Common");
        rule.remove("common");
    }
    // A serde_json::Value is always serializable; no text/config gets logged.
    let bytes = serde_json::to_vec(&canonical(&rule)).expect("JSON rule serializes");
    format!("sha256:{}", crate::model::sha256_hex(&bytes))
}

pub(crate) fn expected_rule_token(params: &Value) -> Result<Option<&str>, String> {
    match params.get("expectedRuleToken") {
        None => Ok(None),
        Some(Value::String(token)) if !token.trim().is_empty() => Ok(Some(token)),
        Some(_) => Err(tr("缺少有效的文本操作版本，请刷新列表。", "A valid text-action version is required. Refresh the list.").into()),
    }
}

pub fn checked_action<'a>(actions: &'a [Value], id: &str, expected: &str) -> Result<&'a Value, String> {
    let action = text_actions::get_by_id(actions, id)
        .ok_or_else(|| tr("文本操作已删除或位置已变化，请刷新列表。", "The text action was removed or moved. Refresh the list.").to_string())?;
    if expected.is_empty() || rule_token(action) != expected {
        return Err(tr("文本操作已发生变化，请刷新列表后重试。", "The text action has changed. Refresh the list and try again.").into());
    }
    Ok(action)
}

/// Update only the star, preserving every known and unknown rule field.
pub fn set_common(actions: &mut [Value], id: &str, expected: &str, common: bool) -> Result<Value, String> {
    checked_action(actions, id, expected)?;
    let index = id.trim().parse::<usize>().map_err(|_| "Text action not found.".to_string())?;
    let action = &mut actions[index];
    let object = action.as_object_mut().ok_or_else(|| "Invalid text action.".to_string())?;
    object.remove("common");
    object.insert("Common".into(), json!(common));
    Ok(text_actions::to_dto(action, &index.to_string()))
}

/// UI labels distinguish purpose without changing the persisted type IDs.
pub fn scope(action: &Value) -> &'static str {
    match action_kind(action) {
        Some("Send") => "insert",
        Some("ZExpression" | "QuickerAction") => "external",
        _ => "transform",
    }
}

/// Works on list DTOs. `group=None` means all; `Some("")` means ungrouped,
/// leaving literal group names such as "all" and "common" unambiguous.
pub fn filter_actions(
    actions: &[Value], requested_scope: &str, group: Option<&str>, common_only: bool, query: &str,
) -> Vec<Value> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    actions.iter().filter(|action| {
        (requested_scope == "all" || scope(action) == requested_scope)
            && (!common_only || is_common(action))
            && group.is_none_or(|group| action.get("group").and_then(Value::as_str).unwrap_or("") == group)
            && {
                let haystack = ["title", "group", "description", "summary", "typeLabel"]
                    .iter().filter_map(|key| action.get(key).and_then(Value::as_str))
                    .collect::<Vec<_>>().join(" ").to_lowercase();
                words.iter().all(|word| haystack.contains(word))
            }
    }).cloned().collect()
}

fn action_kind(action: &Value) -> Option<&'static str> {
    let value = action.get("Type").or_else(|| action.get("type"))?;
    [ ("Internal", 1), ("SingleReplace", 2), ("ZExpression", 4),
      ("QuickerAction", 5), ("Pair", 6), ("Send", 7) ]
        .into_iter().find_map(|(name, code)| {
            (value.as_str().is_some_and(|value| value.eq_ignore_ascii_case(name))
                || value.as_i64() == Some(code)).then_some(name)
        })
}

/// Pure sample preview. This path cannot read selection/clipboard, evaluate an
/// expression, invoke RPC, launch a Quicker action, or paste the output.
/// Accepts a stored rule or a DTO/editor draft, so unsaved edits can be tried.
pub fn preview(action: &Value, input: &str) -> Value {
    let kind = action_kind(action);
    let input_needed = text_actions::needs_input(action);
    let reason = match kind {
        Some("ZExpression") => Some(tr(
            "样例预览不执行表达式；请在目标应用中使用该操作。",
            "Sample preview does not evaluate expressions. Use this action in the target application.",
        )),
        Some("QuickerAction") => Some(tr(
            "样例预览不运行 Quicker 动作；此入口不返回处理文本。",
            "Sample preview does not run Quicker actions. This entry does not return processed text.",
        )),
        None => Some(tr("此操作类型不支持样例预览。", "Sample preview is unavailable for this action type.")),
        _ => None,
    };
    if let Some(reason) = reason {
        return json!({"output":"", "inputNeeded": input_needed, "supported":false,
            "unsupportedReason":reason, "error":null});
    }
    // Only the four local kinds reach here. The fail-closed runners provide an
    // additional guard if the transformation dispatcher gains another branch.
    let result = text_actions::apply_with_runners(action, input,
        |_, _| Err("External actions are unavailable in sample preview.".into()),
        |_, _| Err("Expressions are unavailable in sample preview.".into()));
    match result {
        Ok(output) => json!({"output":output, "inputNeeded":input_needed, "supported":true,
            "unsupportedReason":null, "error":null}),
        Err(error) => json!({"output":"", "inputNeeded":input_needed, "supported":true,
            "unsupportedReason":null, "error":error}),
    }
}

#[cfg(test)]
#[path = "text_actions_ui_tests.rs"]
mod tests;
