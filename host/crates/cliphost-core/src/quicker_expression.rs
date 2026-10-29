//! Keep local interpolation/identity usable without Quicker; evaluate other C#
//! expressions through the optional plugin, never turn a failed call into text.
use std::sync::OnceLock;
use regex::Regex;
use serde_json::{json, Value};
use crate::quicker_rpc::{Registry, INTERACTIVE_TIMEOUT};

pub(crate) fn evaluate(registry: &Registry, expression: &str, params: &Value) -> Result<Value, String> {
    evaluate_with_timeout(registry, expression, params, INTERACTIVE_TIMEOUT)
}

pub(crate) fn evaluate_with_timeout(registry: &Registry, expression: &str, params: &Value, timeout: std::time::Duration) -> Result<Value, String> {
    if let Some(value) = local(expression, params) { return Ok(value); }
    if timeout.is_zero() { return Err("表达式计算超时。".into()); }
    let session = registry.select().ok_or("无法计算表达式：Quicker 插件未连接。")?;
    let response = session.call("quicker.evaluateExpression", json!({
        "Expression": expression, "ParametersJson": params.to_string()
    }), timeout)?;
    let raw = response.as_str().ok_or("表达式未返回结果，请检查表达式及 Quicker 插件。")?;
    let value: Value = serde_json::from_str(raw).map_err(|_| "表达式返回了无效结果。")?;
    if value.is_null() { return Err("表达式未返回结果，请检查表达式及 Quicker 插件。".into()); }
    Ok(value)
}

pub(crate) fn text_params(text: &str) -> Value {
    let list: Vec<_> = text.split(['\r', '\n']).filter(|line| !line.is_empty()).collect();
    json!({"text":text,"list":list})
}

pub(crate) fn text_result(value: Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text),
        Value::Bool(value) => Ok(if value { "True" } else { "False" }.into()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Array(values) if values.iter().all(Value::is_string) =>
            Ok(values.iter().map(|value| value.as_str().unwrap()).collect::<Vec<_>>().join("\r\n")),
        _ => Err("表达式结果不是文本、数值或文本列表，已停止替换。".into()),
    }
}

pub(crate) fn transform_capture(registry: &Registry, text: &str, expression: &str) -> Option<String> {
    if expression.trim().is_empty() { return None; }
    // Automatic preprocessing preserves the original for errors, empty and non-string results.
    evaluate(registry, expression, &json!({"text":text})).ok()?
        .as_str().filter(|text| !text.is_empty()).map(str::to_string)
}

pub(crate) fn local(expression: &str, params: &Value) -> Option<Value> {
    if let Some(body) = expression.strip_prefix("$=") {
        let body = body.trim();
        if let Some(key) = body.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            return variable(key, params);
        }
        // Constants have identical meaning in C# and keep simple conditions usable offline.
        return serde_json::from_str::<Value>(body).ok()
            .filter(|value| value.is_boolean() || value.is_number() || value.is_string());
    }
    let Some(body) = expression.strip_prefix("$$") else { return Some(json!(expression)); };
    static VAR: OnceLock<Regex> = OnceLock::new();
    let pattern = VAR.get_or_init(|| Regex::new(r"\{([^}^{]+)\}").unwrap());
    Some(json!(pattern.replace_all(body, |caps: &regex::Captures| {
        variable(&caps[1], params).map(|value| match value {
            Value::Object(_) => value.to_string(),
            value => text_result(value).unwrap_or_default(),
        }).unwrap_or_else(|| caps[0].into())
    }).into_owned()))
}

fn variable(key: &str, params: &Value) -> Option<Value> {
    if key == "[cliptext]" {
        return Some(json!(crate::clipboard::peek_snapshot().and_then(|snap| snap.text)
            .or_else(crate::clipboard::try_read_text).unwrap_or_default()));
    }
    params.get(key).cloned()
}
