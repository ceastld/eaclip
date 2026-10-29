//! JSON view adapter for the core's pure native overlay action model.
// Keep this shared source usable before the coordinator publishes the module
// from cliphost-core/lib.rs. No clipboard or input work lives in this adapter.
#[path = "../../cliphost-core/src/overlay_row_actions.rs"]
mod model;
pub use model::*;

use serde_json::Value;

pub fn row_item(item: &Value) -> RowItem {
    RowItem {
        kind: item["kind"].as_str().unwrap_or("").to_string(),
        display_text: item["displayText"].as_str().unwrap_or("").to_string(),
        summary: item["summary"].as_str().unwrap_or("").to_string(),
        title: item["title"].as_str().unwrap_or("").to_string(),
        file_paths: item["filePaths"]
            .as_array()
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        file_count: item["fileCount"].as_u64().unwrap_or(0) as usize,
    }
}

pub fn available_actions(item: &Value, state: &Value) -> Vec<RowAction> {
    let provider = if state["snipasteAvailable"].as_bool() == Some(true) {
        Some("snipaste")
    } else if state["pixPinAvailable"].as_bool() == Some(true) {
        Some("pixpin")
    } else {
        None
    };
    row_actions(
        &row_item(item),
        [
            state["shortcut1Action"].as_str().unwrap_or("auto"),
            state["shortcut2Action"].as_str().unwrap_or("auto"),
        ],
        provider,
    )
}
