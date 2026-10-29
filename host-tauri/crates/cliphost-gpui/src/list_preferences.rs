//! Owner-side list preference writes. Editors emit narrow intents; only the
//! shared core is authoritative. Each preset editor owns its own CAS baseline.

use crate::process_filters::{PresetWriter, ProcessPreset};
use cliphost_core::HostState;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

struct PreferenceTooltip {
    text: String,
    theme: crate::preview::ThemeTokens,
}
impl gpui::Render for PreferenceTooltip {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::prelude::*;
        gpui::div()
            .max_w(gpui::px(300.))
            .px(gpui::px(8.))
            .py(gpui::px(5.))
            .rounded(gpui::px(4.))
            .border_1()
            .border_color(crate::view::color(&self.theme.border))
            .bg(crate::view::color(&self.theme.surface))
            .text_size(gpui::px(12.))
            .text_color(crate::view::color(&self.theme.text))
            .child(self.text.clone())
    }
}
pub fn preference_tooltip(
    text: String,
    theme: crate::preview::ThemeTokens,
) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |_, cx| {
        use gpui::AppContext;
        cx.new(|_| PreferenceTooltip {
            text: text.clone(),
            theme: theme.clone(),
        })
        .into()
    }
}

#[derive(Clone)]
pub struct AppearanceWrite {
    pub revision: u64,
    pub patch: Value,
}

#[derive(Default)]
pub struct AppearanceWrites {
    revision: u64,
    protected: HashMap<String, (u64, Value)>,
    pub pending: VecDeque<AppearanceWrite>,
    pub running: bool,
    pub failure: Option<String>,
}

impl AppearanceWrites {
    pub fn accept(&mut self, patch: Value) {
        self.revision = self.revision.wrapping_add(1);
        if let Some(fields) = patch.as_object() {
            for (key, value) in fields {
                self.protected
                    .insert(key.clone(), (self.revision, value.clone()));
            }
        }
        self.pending.push_back(AppearanceWrite {
            revision: self.revision,
            patch,
        });
    }
    pub fn finish(&mut self, write: &AppearanceWrite, succeeded: bool) {
        if !succeeded {
            return;
        }
        for key in write
            .patch
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, _)| key)
        {
            if self
                .protected
                .get(key)
                .is_some_and(|(revision, _)| *revision == write.revision)
            {
                self.protected.remove(key);
            }
        }
        if self.protected.is_empty() {
            self.failure = None;
        }
    }
    /// Merge current core data with queued/rejected local intent, without
    /// turning another window's unrelated fields into a write patch.
    pub fn merge(&self, authoritative: &Value) -> Value {
        let mut value = authoritative.clone();
        if !value.is_object() {
            value = json!({});
        }
        for (key, (_, draft)) in &self.protected {
            value[key] = draft.clone();
        }
        value
    }
}

#[derive(Clone, Debug)]
pub struct PresetSnapshot {
    pub revision: String,
    pub initialized: bool,
    pub presets: Vec<ProcessPreset>,
}
impl PresetSnapshot {
    pub fn from_value(value: Value) -> Result<Self, String> {
        let revision = value["revision"]
            .as_str()
            .filter(|revision| !revision.is_empty())
            .ok_or("Process preset revision is unavailable.")?
            .to_string();
        let presets = serde_json::from_value(value["presets"].clone())
            .map_err(|error| format!("Invalid process preset snapshot: {error}"))?;
        Ok(Self {
            revision,
            presets,
            initialized: value["initialized"].as_bool().unwrap_or(false),
        })
    }
}

/// The shared write gate serializes different editors, but revision belongs
/// to this editor alone. A stale editor is rejected instead of taking the
/// latest revision and replaying its old complete collection over new data.
pub fn preset_writer(
    host: Arc<HostState>,
    snapshot: &PresetSnapshot,
    gate: Arc<Mutex<()>>,
) -> PresetWriter {
    let baseline = Arc::new(Mutex::new(PresetBaseline {
        revision: snapshot.revision.clone(),
    }));
    Arc::new(move |presets| {
        let _serial = gate
            .lock()
            .map_err(|_| "Process preset write queue is unavailable.".to_string())?;
        let mut baseline = baseline
            .lock()
            .map_err(|_| "Process preset editor baseline is unavailable.".to_string())?;
        baseline.save(presets, |params| host.process_filter_presets_save(params))
    })
}

/// The same serial queue may contain several editors, but their loaded
/// revisions remain independent. Failed CAS never advances this baseline.
struct PresetBaseline {
    revision: String,
}
impl PresetBaseline {
    fn save(
        &mut self,
        presets: Vec<ProcessPreset>,
        persist: impl FnOnce(&Value) -> Result<Value, String>,
    ) -> Result<(), String> {
        let saved = persist(&json!({"expectedRevision":self.revision,"presets":presets}))?;
        self.revision = PresetSnapshot::from_value(saved)?.revision;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn two_loaded_editors_keep_separate_cas_baselines_after_success_and_conflict() {
        let mut first = PresetBaseline {
            revision: "loaded".into(),
        };
        let mut stale = PresetBaseline {
            revision: "loaded".into(),
        };
        first
            .save(vec![], |params| {
                assert_eq!(params["expectedRevision"], "loaded");
                Ok(json!({"revision":"first-save","initialized":true,"presets":[]}))
            })
            .unwrap();
        let attempt = stale.save(vec![], |params| {
            assert_eq!(params["expectedRevision"], "loaded");
            Err("The presets changed in another window.".into())
        });
        assert!(attempt.is_err());
        assert_eq!(stale.revision, "loaded");
        first
            .save(vec![], |params| {
                assert_eq!(params["expectedRevision"], "first-save");
                Ok(json!({"revision":"second-save","initialized":true,"presets":[]}))
            })
            .unwrap();
        assert_eq!(stale.revision, "loaded");
    }
    #[test]
    fn stale_appearance_reply_and_events_preserve_newer_intent_and_unrelated_core_fields() {
        let mut writes = AppearanceWrites::default();
        writes.accept(json!({"clipListAppearanceSlot":"custom","clipListCustomAppearance":{"layout":"minimal"}}));
        let old = writes.pending.pop_front().unwrap();
        writes.accept(json!({"clipListAppearanceSlot":"standard"}));
        writes.finish(&old, true);
        let config = writes.merge(&json!({"clipListAppearanceSlot":"custom","unrelated":7}));
        assert_eq!(config["clipListAppearanceSlot"], "standard");
        assert_eq!(config["unrelated"], 7);
        let latest = writes.pending.pop_front().unwrap();
        writes.finish(&latest, false);
        assert_eq!(
            writes.merge(&json!({"clipListAppearanceSlot":"custom"}))["clipListAppearanceSlot"],
            "standard"
        );
        writes.finish(&latest, true);
        assert_eq!(
            writes.merge(&json!({"clipListAppearanceSlot":"simple"}))["clipListAppearanceSlot"],
            "simple"
        );
    }
    #[test]
    fn uninitialized_shared_snapshot_is_readable_without_claiming_legacy_origin_is_empty() {
        let snapshot =
            PresetSnapshot::from_value(json!({"revision":"0","initialized":false,"presets":[]}))
                .unwrap();
        assert!(!snapshot.initialized);
        assert!(snapshot.presets.is_empty());
        assert_eq!(snapshot.revision, "0");
        assert!(PresetSnapshot::from_value(json!({"presets":[]})).is_err());
    }
}
