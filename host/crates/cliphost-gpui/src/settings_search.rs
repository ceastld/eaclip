//! Metadata-only settings search. Loading schemas and retaining live editors
//! belong to the settings window; this index never owns field values or drafts.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

#[derive(Default)]
pub(super) struct SettingsSearchIndex {
    configs: HashMap<String, IndexedConfig>,
}

struct IndexedConfig {
    label: String,
    fields: Vec<IndexedField>,
}

struct IndexedField {
    key: String,
    haystacks: Vec<String>,
}

pub(super) struct SettingsSearchMatch {
    pub(super) config_id: String,
    pub(super) label: String,
    /// The live SettingsForm filters its original fields with these keys, so
    /// matching controls retain their existing drafts, IME and save queue.
    pub(super) field_keys: HashSet<String>,
}

impl SettingsSearchMatch {
    pub(super) fn count(&self) -> usize {
        self.field_keys.len()
    }
}

impl SettingsSearchIndex {
    /// Replaces presentation metadata after loading or a locale refresh.
    /// The schema's localized label takes precedence over the catalog fallback.
    pub(super) fn upsert(&mut self, config_id: &str, config_label: &str, schema: &Value) {
        let label = schema["configLabel"]
            .as_str()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or(config_label)
            .to_string();
        let fields = schema["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|field| {
                let key = field["key"].as_str()?.to_string();
                if key.is_empty() {
                    return None;
                }
                let mut haystacks = vec![label.to_lowercase()];
                for property in ["group", "key", "label", "description"] {
                    if let Some(value) = field
                        .get(property)
                        .filter(|value| metadata_is_truthy(value))
                    {
                        haystacks.push(js_string(value).to_lowercase());
                    }
                }
                if field["inputType"] == "enum" {
                    append_enum_metadata(field, &mut haystacks);
                }
                Some(IndexedField { key, haystacks })
            })
            .collect();
        self.configs
            .insert(config_id.to_string(), IndexedConfig { label, fields });
    }

    pub(super) fn remove(&mut self, config_id: &str) {
        self.configs.remove(config_id);
    }

    /// Mirrors SettingsPage.fieldMatchesQuery: one trimmed, case-insensitive
    /// substring must occur in one metadata string. Do not split into terms.
    /// Only the supplied visible catalog is searched, in its existing order.
    pub(super) fn matches(&self, query: &str, config_order: &[String]) -> Vec<SettingsSearchMatch> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        config_order
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|config_id| {
                let config = self.configs.get(config_id)?;
                let field_keys: HashSet<_> = config
                    .fields
                    .iter()
                    .filter(|field| field.haystacks.iter().any(|text| text.contains(&needle)))
                    .map(|field| field.key.clone())
                    .collect();
                if field_keys.is_empty() {
                    return None;
                }
                Some(SettingsSearchMatch {
                    config_id: config_id.clone(),
                    label: config.label.clone(),
                    field_keys,
                })
            })
            .collect()
    }
}

/// Mirrors the frontend's query-change effect: every actual text change clears
/// a selected category scope, including case/whitespace edits to the same needle.
#[derive(Default)]
pub(super) struct SettingsSearchQuery {
    query: String,
    scope: Option<String>,
}

impl SettingsSearchQuery {
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn is_active(&self) -> bool {
        !self.query.trim().is_empty()
    }

    pub(super) fn set_query(&mut self, query: impl Into<String>) -> bool {
        let query = query.into();
        if query == self.query {
            return false;
        }
        self.query = query;
        self.scope = None;
        true
    }

    pub(super) fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    pub(super) fn set_scope(&mut self, scope: Option<String>) -> bool {
        let scope = scope.filter(|id| self.is_active() && !id.is_empty());
        if self.scope == scope {
            return false;
        }
        self.scope = scope;
        true
    }

    pub(super) fn clear(&mut self) -> bool {
        self.set_query("")
    }
}

/// Holds one projection update while any live editor is composing. The window
/// decides readiness; this model neither polls editors nor changes their drafts.
#[derive(Default)]
pub(super) struct DeferredSettingsSearch {
    // None: idle; Some(None): metadata refresh; Some(Some(raw)): latest query.
    pending: Option<Option<String>>,
}

impl DeferredSettingsSearch {
    pub(super) fn defer_query(&mut self, query: String) {
        self.pending = Some(Some(query));
    }

    pub(super) fn defer_refresh(&mut self) {
        // Applying a queued query will also use the latest metadata. A locale
        // event must not replace that query with a refresh-only request.
        if self.pending.is_none() {
            self.pending = Some(None);
        }
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Outer None means busy or idle. Some(None) refreshes metadata without a
    /// query edit; Some(Some(raw)) applies the latest raw query, including clear.
    pub(super) fn take_when_ready(&mut self, ready: bool) -> Option<Option<String>> {
        if ready { self.pending.take() } else { None }
    }
}

/// Keep the frontend's alias, nullish precedence, numeric key coercion and
/// fallback rules from normalizeEnumSelectionItems. A nonempty selectionItems
/// array is authoritative even when all its entries have invalid keys.
fn append_enum_metadata(field: &Value, haystacks: &mut Vec<String>) {
    let candidate = ["selectionItems", "selectionitems", "selectionitem"]
        .into_iter()
        .find_map(|property| field.get(property).filter(|value| !value.is_null()));
    if let Some(items) = candidate
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    {
        for item in items {
            let key = item.get("key").map(js_string).unwrap_or_default();
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            haystacks.push(key.to_lowercase());
            let name = item.get("name").filter(|value| !value.is_null());
            haystacks.push(
                name.map(js_string)
                    .unwrap_or_else(|| key.to_string())
                    .to_lowercase(),
            );
            if let Some(description) = item.get("des") {
                haystacks.push(js_string(description).to_lowercase());
            }
        }
        return;
    }
    for option in field["options"].as_array().into_iter().flatten() {
        let text = js_string(option);
        let text = text.trim();
        if !text.is_empty() {
            haystacks.push(text.to_lowercase());
        }
    }
}

fn metadata_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JavaScript String(value ?? "") for the enum DTO's occasionally numeric keys.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn schema(fields: Value) -> Value {
        json!({"configLabel": "Clipboard", "fields": fields})
    }

    #[test]
    fn searches_all_frontend_metadata_without_combining_terms() {
        let mut index = SettingsSearchIndex::default();
        index.upsert("clip", "Fallback", &schema(json!([
            {"key":"KeepHistory", "label":"Keep copied items", "group":"Storage", "description":"Retain your recent history", "inputType":"boolean"},
            {"key":"Compact", "label":"Dense list", "group":"Appearance", "description":"Small rows", "inputType":"boolean"}
        ])));
        let order = catalog(&["clip"]);
        for query in [
            " KEEPhistory ",
            "COPIED",
            "Storage",
            "recent history",
            "Clipboard",
        ] {
            assert!(
                index.matches(query, &order)[0]
                    .field_keys
                    .contains("KeepHistory")
            );
        }
        assert_eq!(index.matches("clipboard", &order)[0].count(), 2);
        assert!(index.matches("clipboard storage", &order).is_empty());
        assert!(index.matches("copied history", &order).is_empty());
        assert!(index.matches("  \n\t ", &order).is_empty());
    }

    #[test]
    fn enum_aliases_numeric_keys_and_options_follow_normalization() {
        let mut index = SettingsSearchIndex::default();
        index.upsert("clip", "", &schema(json!([
            {"key":"Primary", "inputType":"enum", "selectionItems":[{"key":12,"name":"Bottom right","des":"Screen corner"}], "options":["stale-option"]},
            {"key":"Plural", "inputType":"enum", "selectionItems":null,"selectionitems":[{"key":"dark","name":"Night mode","des":"Low glare"}]},
            {"key":"Singular", "inputType":"enum", "selectionitem":[{"key":false,"name":null,"des":27}]},
            {"key":"Fallback", "inputType":"enum", "selectionItems":[],"options":[null," ",31,"light"]},
            {"key":"NotEnum", "inputType":"text", "options":["not-an-option"]}
        ])));
        let order = catalog(&["clip"]);
        for query in ["12", "bottom right", "screen corner"] {
            assert!(
                index.matches(query, &order)[0]
                    .field_keys
                    .contains("Primary")
            );
        }
        for query in ["night mode", "low glare", "dark"] {
            assert!(
                index.matches(query, &order)[0]
                    .field_keys
                    .contains("Plural")
            );
        }
        for query in ["false", "27"] {
            assert!(
                index.matches(query, &order)[0]
                    .field_keys
                    .contains("Singular")
            );
        }
        assert!(
            index.matches("31", &order)[0]
                .field_keys
                .contains("Fallback")
        );
        assert!(index.matches("stale-option", &order).is_empty());
        assert!(index.matches("not-an-option", &order).is_empty());
    }

    #[test]
    fn enum_nonempty_invalid_items_and_empty_alias_precedence_do_not_fall_back() {
        let mut index = SettingsSearchIndex::default();
        index.upsert("clip", "", &schema(json!([
            {"key":"Invalid", "inputType":"enum", "selectionItems":[{"key":" "},{"key":null}],"options":["must-not-fallback"]},
            {"key":"Alias", "inputType":"enum", "selectionItems":[],"selectionitems":[{"key":"must-not-use-alias"}],"options":["fallback-option"]}
        ])));
        let order = catalog(&["clip"]);
        assert!(index.matches("must-not", &order).is_empty());
        assert!(
            index.matches("fallback-option", &order)[0]
                .field_keys
                .contains("Alias")
        );
    }

    #[test]
    fn never_indexes_values_defaults_presets_or_unlisted_categories() {
        let mut index = SettingsSearchIndex::default();
        index.upsert("clip", "", &schema(json!([
            {"key":"Path", "label":"Data folder", "value":"C:/private/path", "defaultValue":"secret-default", "presets":["secret-preset"]},
            {"key":"Password", "label":"Account credential", "value":"hunter-secret"}
        ])));
        index.upsert(
            "hiddenCloud",
            "Hidden cloud",
            &json!({"fields":[{"key":"CloudToken","label":"Cloud credential"}]}),
        );
        let order = catalog(&["clip"]);
        for query in [
            "private/path",
            "secret-default",
            "secret-preset",
            "hunter-secret",
            "cloud",
        ] {
            assert!(index.matches(query, &order).is_empty());
        }
        assert_eq!(index.matches("credential", &order)[0].count(), 1);
    }

    #[test]
    fn category_order_and_unique_field_counts_survive_partial_schema_collection() {
        let mut index = SettingsSearchIndex::default();
        index.upsert("z", "Zebra", &json!({"fields":[{"key":"B","label":"Enabled"},{"key":"A","label":"Enabled"},{"key":"A","label":"Enabled"}]}));
        index.upsert(
            "a",
            "Alpha",
            &json!({"fields":[{"key":"C","label":"Enabled"}]}),
        );
        let order = catalog(&["z", "not-loaded", "a", "z"]);
        let matches = index.matches("enabled", &order);
        assert_eq!(
            matches
                .iter()
                .map(|row| row.config_id.as_str())
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert_eq!(matches[0].label, "Zebra");
        assert_eq!(matches[0].count(), 2);
        assert_eq!(matches[1].count(), 1);
        index.remove("z");
        assert_eq!(index.matches("enabled", &order)[0].config_id, "a");
    }

    #[test]
    fn locale_refresh_replaces_metadata_and_keeps_source_values_untouched() {
        let mut index = SettingsSearchIndex::default();
        let original = json!({"configLabel":"个性化", "fields":[{"key":"Language","label":"语言","group":"外观","description":"界面文案","inputType":"enum","selectionItems":[{"key":"auto","name":"跟随系统","des":"Windows 显示语言"}],"value":"zh"}]});
        index.upsert("global", "Fallback", &original);
        let order = catalog(&["global"]);
        assert_eq!(index.matches("个性化", &order)[0].label, "个性化");
        let translated = json!({"configLabel":"Personalization", "fields":[{"key":"Language","label":"Language","group":"Appearance","description":"Interface text","inputType":"enum","selectionItems":[{"key":"auto","name":"Follow system","des":"Windows display language"}],"value":"en"}]});
        index.upsert("global", "Fallback", &translated);
        for query in ["个性化", "语言", "外观", "跟随系统"] {
            assert!(index.matches(query, &order).is_empty());
        }
        assert_eq!(
            index.matches("display language", &order)[0].label,
            "Personalization"
        );
        assert!(
            index.matches("Appearance", &order)[0]
                .field_keys
                .contains("Language")
        );
        assert_eq!(original["fields"][0]["value"], "zh");
    }

    #[test]
    fn replacing_empty_schema_clears_old_hits_and_uses_catalog_label_fallback() {
        let mut index = SettingsSearchIndex::default();
        let order = catalog(&["data"]);
        index.upsert(
            "data",
            "Data folder",
            &json!({"configLabel":" ","fields":[{"key":"Path","label":"Directory"}]}),
        );
        assert_eq!(index.matches("data", &order)[0].label, "Data folder");
        index.upsert("data", "Data folder", &json!({"fields":[]}));
        assert!(index.matches("directory", &order).is_empty());
    }

    #[test]
    fn query_change_and_clear_drop_scope_but_unchanged_query_preserves_it() {
        let mut query = SettingsSearchQuery::default();
        assert!(!query.is_active());
        assert!(!query.set_scope(Some("clip".into())));
        assert!(query.set_query("history"));
        assert!(query.set_scope(Some("clip".into())));
        assert!(!query.set_query("history"));
        assert_eq!(query.scope(), Some("clip"));
        assert!(query.set_query(" History "));
        assert_eq!(query.scope(), None);
        assert_eq!(query.query(), " History ");
        query.set_scope(Some("clip".into()));
        assert!(query.clear());
        assert_eq!(query.query(), "");
        assert_eq!(query.scope(), None);
        assert!(!query.is_active());
        assert!(!query.clear());
    }

    #[test]
    fn deferred_search_holds_latest_raw_query_until_editors_are_ready() {
        let mut deferred = DeferredSettingsSearch::default();
        assert!(!deferred.is_pending());
        assert_eq!(deferred.take_when_ready(true), None);
        deferred.defer_query("first".into());
        assert_eq!(deferred.take_when_ready(false), None);
        assert!(deferred.is_pending());
        deferred.defer_query(" Latest 原始查询 ".into());
        assert_eq!(deferred.take_when_ready(false), None);
        assert_eq!(
            deferred.take_when_ready(true),
            Some(Some(" Latest 原始查询 ".into()))
        );
        assert!(!deferred.is_pending());
        assert_eq!(deferred.take_when_ready(true), None);
    }

    #[test]
    fn deferred_clear_remains_distinct_from_a_metadata_refresh() {
        let mut deferred = DeferredSettingsSearch::default();
        deferred.defer_query("history".into());
        deferred.defer_query(String::new());
        deferred.defer_refresh();
        assert_eq!(deferred.take_when_ready(false), None);
        assert_eq!(deferred.take_when_ready(true), Some(Some(String::new())));
        deferred.defer_refresh();
        assert_eq!(deferred.take_when_ready(true), Some(None));
        assert!(!deferred.is_pending());
        assert_eq!(deferred.take_when_ready(true), None);
    }

    #[test]
    fn deferred_metadata_refresh_coalesces_and_preserves_a_queued_query() {
        let mut deferred = DeferredSettingsSearch::default();
        deferred.defer_refresh();
        deferred.defer_refresh();
        assert!(deferred.is_pending());
        assert_eq!(deferred.take_when_ready(false), None);
        assert_eq!(deferred.take_when_ready(true), Some(None));
        assert_eq!(deferred.take_when_ready(true), None);
        deferred.defer_refresh();
        deferred.defer_query("updated".into());
        deferred.defer_refresh();
        deferred.defer_refresh();
        assert_eq!(deferred.take_when_ready(true), Some(Some("updated".into())));
        assert!(!deferred.is_pending());
    }
}
