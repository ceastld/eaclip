//! Offline Quicker step clipboard presentation, aligned with C# `StepClipboardPresentation`.

use serde_json::{json, Value};

const PREVIEW_LINE_LIMIT: usize = 4;
const DEFAULT_STEP_ICON: &str = "fa:Light_ListOl";

fn offline_runner(key: &str) -> Option<(&'static str, &'static str)> {
    match key.to_ascii_lowercase().as_str() {
        "sys:runscript" => Some(("运行脚本", "fa:Light_Scroll:#6aaded")),
        "sys:csscript" => Some(("运行C#代码", "fa:Light_Scroll:#6aaded")),
        "sys:subprogram" => Some(("运行子程序", "fa:Solid_Cubes:#3196F4")),
        "sys:assign" => Some(("赋值", "fa:Light_Edit:#6aaded")),
        "sys:evalexpression" => Some(("执行表达式", "fa:Light_Code")),
        "sys:notify" => Some(("提示消息", "fa:Light_Bullhorn:#6aaded")),
        "sys:msgbox" => Some(("弹窗提示或确认", DEFAULT_STEP_ICON)),
        "sys:getsysinfo" => Some(("获取系统或动作信息", "fa:Light_Cog:#6aaded")),
        "sys:keyoperation" => Some(("按键操作", "fa:Light_Keyboard:#6aaded")),
        "sys:numcompare" => Some(("比较数字", "fa:Light_Calculator:#6aaded")),
        "sys:keyinput" => Some(("模拟按键A（录入）", "fa:Light_Keyboard:#6aaded")),
        "sys:sendkeys" => Some(("模拟按键B（参数）", "fa:Light_Keyboard:#6aaded")),
        "sys:waitkeyboard" => Some(("等待按键", "fa:Light_Keyboard:#6aaded")),
        _ => None,
    }
}

pub fn analyze(steps_data: &str) -> Value {
    if steps_data.trim().is_empty() {
        return empty_analysis();
    }
    let Ok(root) = serde_json::from_str::<Value>(steps_data) else {
        return fallback_analysis(steps_data);
    };
    let steps = root
        .get("Steps")
        .or_else(|| root.get("steps"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let variables = root
        .get("Variables")
        .or_else(|| root.get("variables"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let step_count = count_steps(&steps);
    let preview_lines: Vec<Value> = steps
        .iter()
        .filter_map(|v| v.as_object())
        .take(PREVIEW_LINE_LIMIT)
        .map(present_step)
        .collect();
    let preview_text = build_preview_text(&preview_lines, step_count, &variables);
    let primary_icon = preview_lines
        .first()
        .and_then(|v| v.get("icon").and_then(|i| i.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if step_count > 0 {
                DEFAULT_STEP_ICON.to_string()
            } else {
                String::new()
            }
        });
    json!({
        "stepCount": step_count,
        "variableCount": variables.len(),
        "previewLines": preview_lines,
        "previewText": preview_text,
        "primaryIcon": primary_icon,
        "stats": { "count": step_count, "variableCount": variables.len() },
        "statsLabel": format!("{step_count} 步 · {} 变量", variables.len()),
    })
}

/// C# `QuickerActionStepsSummaryDto` (offline `ClipHostStepPresentationLocal.ToSummaryDto`).
pub fn summary_dto(steps_data: &str) -> Value {
    let analysis = analyze(steps_data);
    json!({
        "stepCount": analysis["stepCount"],
        "variableCount": analysis["variableCount"],
        "statsLabel": analysis["statsLabel"],
        "previewText": analysis["previewText"],
        "primaryIcon": analysis["primaryIcon"],
    })
}

/// C# `QuickerActionStepsPresentationDto` (offline `ToPresentationDto`).
pub fn presentation_dto(steps_data: &str) -> Value {
    let analysis = analyze(steps_data);
    json!({
        "stepCount": analysis["stepCount"],
        "variableCount": analysis["variableCount"],
        "statsLabel": analysis["statsLabel"],
        "previewText": analysis["previewText"],
        "primaryIcon": analysis["primaryIcon"],
        "previewLines": analysis["previewLines"],
    })
}

pub fn build_summary(steps_data: &str) -> String {
    let analysis = analyze(steps_data);
    if let Some(text) = analysis.get("previewText").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return text.to_string();
    }
    fallback_preview(steps_data)
}

fn fallback_preview(raw: &str) -> String {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return "Quicker 步骤".into();
    }
    let first = raw.lines().next().unwrap_or("").trim();
    if first.chars().count() > 80 {
        format!("{}...", first.chars().take(80).collect::<String>())
    } else {
        first.to_string()
    }
}

fn empty_analysis() -> Value {
    json!({
        "stepCount": 0,
        "variableCount": 0,
        "previewLines": [],
        "previewText": "",
        "primaryIcon": "",
        "stats": { "count": 0, "variableCount": 0 },
        "statsLabel": "0 步 · 0 变量",
    })
}

fn fallback_analysis(raw: &str) -> Value {
    let mut body = empty_analysis();
    body["previewText"] = json!(fallback_preview(raw));
    body
}

fn count_steps(steps: &[Value]) -> i32 {
    let mut count = 0;
    for step in steps.iter().filter_map(|v| v.as_object()) {
        let disabled = step
            .get("Disabled")
            .or_else(|| step.get("disabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if disabled {
            continue;
        }
        count += 1;
        if let Some(inner) = step.get("IfSteps").or_else(|| step.get("ifSteps")).and_then(|v| v.as_array()) {
            count += count_steps(inner);
        }
        if let Some(inner) = step.get("ElseSteps").or_else(|| step.get("elseSteps")).and_then(|v| v.as_array()) {
            count += count_steps(inner);
        }
    }
    count
}

fn present_step(step: &serde_json::Map<String, Value>) -> Value {
    let key = step
        .get("StepRunnerKey")
        .or_else(|| step.get("stepRunnerKey"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let offline = offline_runner(&key);
    let title = offline
        .map(|(title, _)| title.to_string())
        .unwrap_or_else(|| if key.is_empty() { "步骤".into() } else { key.clone() });
    let summary = step
        .get("inputParams")
        .or_else(|| step.get("InputParams"))
        .and_then(|v| v.get("summary").or_else(|| v.get("Summary")))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&title)
        .to_string();
    let disabled = step
        .get("Disabled")
        .or_else(|| step.get("disabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let display = if summary == title {
        title.clone()
    } else {
        format!("{title}: {summary}")
    };
    let icon = step
        .get("Icon")
        .or_else(|| step.get("icon"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| offline.map(|(_, icon)| icon.to_string()))
        .unwrap_or_else(|| DEFAULT_STEP_ICON.to_string());
    json!({
        "icon": icon,
        "title": title,
        "summary": summary,
        "displayText": display,
        "summarySuffix": if summary == title { String::new() } else { format!(": {summary}") },
        "disabled": disabled,
    })
}

fn build_preview_text(lines: &[Value], step_count: i32, variables: &[Value]) -> String {
    if lines.is_empty() && step_count == 0 {
        return String::new();
    }
    let mut out = String::new();
    for line in lines {
        if !out.is_empty() {
            out.push('\n');
        }
        if let Some(text) = line.get("displayText").and_then(|v| v.as_str()) {
            out.push_str(text);
        }
    }
    if step_count > lines.len() as i32 {
        out.push_str(&format!("\n… 共 {step_count} 步"));
    }
    let keys: Vec<&str> = variables
        .iter()
        .filter_map(|v| v.get("Key").or_else(|| v.get("key")).and_then(|k| k.as_str()))
        .filter(|k| !k.is_empty())
        .take(12)
        .collect();
    if !keys.is_empty() {
        out.push_str("\n变量: ");
        out.push_str(&keys.join("  "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzes_offline_runner_and_variables() {
        let raw = r#"{
            "Steps": [
                { "StepRunnerKey": "sys:assign", "inputParams": { "summary": "x=1" } },
                { "StepRunnerKey": "sys:notify", "Disabled": true }
            ],
            "Variables": [{ "Key": "x" }]
        }"#;
        let analysis = analyze(raw);
        assert_eq!(analysis["stepCount"], 1);
        assert_eq!(analysis["variableCount"], 1);
        assert!(analysis["previewText"].as_str().unwrap().contains("赋值: x=1"));
        assert!(analysis["previewText"].as_str().unwrap().contains("变量: x"));
        assert_eq!(analysis["primaryIcon"], "fa:Light_Edit:#6aaded");
        let summary = summary_dto(raw);
        assert!(summary.get("previewLines").is_none());
        assert_eq!(summary["stepCount"], 1);
        assert_eq!(summary["primaryIcon"], analysis["primaryIcon"]);
        assert_eq!(build_summary("not-json"), "not-json");
        assert_eq!(build_summary("{"), "Quicker 步骤");
        assert_eq!(summary_dto("")["primaryIcon"], "");
    }
}
