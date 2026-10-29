//! Private QP/Toast theme snapshots. Read only the existing in-memory global
//! settings form; the incoming runtime palette is never a derivation base.
use std::time::{Duration, Instant};

use cliphost_core::host::HostState;
use serde_json::{json, Value};

use crate::preview::{DesignTokens, TOKENS_JSON};

const READ_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawTheme {
    mode: String,
    background: String,
    foreground: String,
}

impl RawTheme {
    fn from_form(form: &Value) -> Option<Self> {
        let fields = form["fields"].as_array()?;
        let value = |key| {
            fields
                .iter()
                .find(|field| field["key"].as_str() == Some(key))?["value"]
                .as_str()
                .map(str::to_owned)
        };
        Some(Self {
            mode: value("UiThemeMode")?,
            background: value("WindowBackgroundColor")?,
            foreground: value("FontForegroundColor")?,
        })
    }

    fn config(&self) -> Value {
        // These remain the persisted WPF strings. Only resolved_tokens turns
        // them into CSS colors; never parse incoming resolved token colors.
        json!({
            "uiThemeMode": self.mode,
            "windowBackground": self.background,
            "fontForeground": self.foreground,
        })
    }

    fn dark(&self, system_dark: bool) -> bool {
        match self.mode.to_ascii_lowercase().as_str() {
            "dark" => true,
            "light" => false,
            _ => system_dark,
        }
    }
}

pub(crate) struct OverlayTheme {
    base: DesignTokens,
    snapshot: Option<(RawTheme, bool)>,
    last_read: Option<Instant>,
    needs_emit: bool,
}

impl OverlayTheme {
    pub(crate) fn new(incoming: &DesignTokens) -> Self {
        let mut base = incoming.clone();
        base.themes = DesignTokens::from_json(TOKENS_JSON)
            .expect("validated original overlay theme tokens")
            .themes;
        Self {
            base,
            snapshot: None,
            last_read: None,
            needs_emit: false,
        }
    }

    /// None leaves the caller's current palette intact. Show forces a fresh
    /// successful result; maintenance is throttled and deduplicated separately
    /// from the surface's content state, presentation lifetime and timers.
    pub(crate) fn read(
        &mut self,
        host: &HostState,
        system_dark: bool,
        force: bool,
    ) -> Option<(DesignTokens, bool)> {
        self.read_with(Instant::now(), system_dark, force, || {
            host.settings_form_get(&json!({ "configId": "global" }))
        })
    }

    fn read_with(
        &mut self,
        now: Instant,
        system_dark: bool,
        force: bool,
        fetch: impl FnOnce() -> Result<Value, String>,
    ) -> Option<(DesignTokens, bool)> {
        if force {
            // A failed show may use incoming tokens as its fallback. Re-emit
            // the next valid snapshot even if its raw values are unchanged.
            self.needs_emit = true;
        }
        if !force
            && self
                .last_read
                .is_some_and(|last| now.saturating_duration_since(last) < READ_INTERVAL)
        {
            return None;
        }
        // Failed reads also respect the limit, but never replace the last
        // accepted raw snapshot; a repaired form is retried on the next read.
        self.last_read = Some(now);
        let raw = RawTheme::from_form(&fetch().ok()?)?;
        if !self.needs_emit && self.snapshot.as_ref() == Some(&(raw.clone(), system_dark)) {
            return None;
        }
        let dark = raw.dark(system_dark);
        let tokens = crate::custom_theme::resolved_tokens(&self.base, &raw.config());
        self.snapshot = Some((raw, system_dark));
        self.needs_emit = false;
        Some((tokens, dark))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn original() -> DesignTokens {
        DesignTokens::from_json(TOKENS_JSON).unwrap()
    }

    fn form(mode: &str, background: &str, foreground: &str) -> Value {
        json!({"fields":[
            {"key":"UiThemeMode","value":mode},
            {"key":"WindowBackgroundColor","value":background},
            {"key":"FontForegroundColor","value":foreground},
            {"key":"Unrelated","value":"ignored"},
        ]})
    }

    #[test]
    fn resolved_incoming_keeps_geometry_but_restores_original_theme_on_exit() {
        let original = original();
        let mut incoming = crate::custom_theme::resolved_tokens(
            &original,
            &json!({"uiThemeMode":"custom","windowBackground":"#80112233","fontForeground":"#C0667788"}),
        );
        incoming.window.width = 777.0;
        incoming.window.height = 666.0;
        incoming.icons.insert("kept".into(), "kept.svg".into());
        let mut theme = OverlayTheme::new(&incoming);
        let base_before = serde_json::to_value(&theme.base).unwrap();
        let now = Instant::now();
        let custom = form("custom", "#80112233", "#C0667788");
        let before = custom.clone();
        let (resolved, dark) = theme
            .read_with(now, true, true, || Ok(custom.clone()))
            .unwrap();
        assert!(dark);
        assert_eq!(resolved.themes.dark.surface, "#11223380");
        assert_eq!(resolved.themes.dark.text, "#667788C0");
        assert_eq!(custom, before);
        let (restored, dark) = theme
            .read_with(now, false, true, || {
                Ok(form("light", "#80112233", "#C0667788"))
            })
            .unwrap();
        assert!(!dark);
        assert_eq!(
            serde_json::to_value(&restored.themes).unwrap(),
            serde_json::to_value(&original.themes).unwrap()
        );
        let mut restored = serde_json::to_value(restored).unwrap();
        let incoming = serde_json::to_value(incoming).unwrap();
        restored["themes"] = incoming["themes"].clone();
        assert_eq!(restored, incoming);
        assert_eq!(serde_json::to_value(&theme.base).unwrap(), base_before);
    }

    #[test]
    fn same_dark_custom_color_change_and_system_change_refresh() {
        let mut theme = OverlayTheme::new(&original());
        let now = Instant::now();
        let first = form("custom", "#FF112233", "#FF445566");
        let (tokens, dark) = theme
            .read_with(now, true, false, || Ok(first.clone()))
            .unwrap();
        assert!(dark);
        assert_eq!(tokens.themes.dark.text, "#445566FF");
        let second = form("custom", "#FF112233", "#FF778899");
        let (tokens, dark) = theme
            .read_with(now + READ_INTERVAL, true, false, || Ok(second.clone()))
            .unwrap();
        assert!(dark);
        assert_eq!(tokens.themes.dark.text, "#778899FF");
        let (_, dark) = theme
            .read_with(now + READ_INTERVAL * 2, false, false, || Ok(second))
            .unwrap();
        assert!(!dark);
    }

    #[test]
    fn throttle_avoids_fetch_and_force_bypasses_unchanged_snapshot() {
        let mut theme = OverlayTheme::new(&original());
        let now = Instant::now();
        let calls = Cell::new(0);
        let fetch = || {
            calls.set(calls.get() + 1);
            Ok(form("auto", "#FFFFFFFF", "#FF000000"))
        };
        assert!(theme.read_with(now, true, false, fetch).is_some());
        assert!(theme
            .read_with(now + Duration::from_millis(99), true, false, fetch)
            .is_none());
        assert_eq!(calls.get(), 1);
        assert!(theme
            .read_with(now + READ_INTERVAL, true, false, fetch)
            .is_none());
        assert_eq!(calls.get(), 2);
        assert!(theme
            .read_with(now + READ_INTERVAL, true, true, fetch)
            .is_some());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn missing_or_failed_snapshot_preserves_accepted_theme_and_retries() {
        let mut theme = OverlayTheme::new(&original());
        let now = Instant::now();
        let valid = form("dark", "#FFFFFFFF", "#FF000000");
        theme
            .read_with(now, false, false, || Ok(valid.clone()))
            .unwrap();
        let accepted = theme.snapshot.clone();
        let mut missing = valid.clone();
        missing["fields"].as_array_mut().unwrap().remove(2);
        assert!(theme
            .read_with(now + READ_INTERVAL, false, false, || Ok(missing))
            .is_none());
        assert_eq!(theme.snapshot, accepted);
        assert!(theme
            .read_with(now + READ_INTERVAL * 2, false, false, || Err(
                "unavailable".into()
            ))
            .is_none());
        assert_eq!(theme.snapshot, accepted);
        assert!(theme
            .read_with(now + READ_INTERVAL * 3, false, false, || Ok(valid))
            .is_none());
        let (tokens, _) = theme
            .read_with(now + READ_INTERVAL * 4, false, false, || {
                Ok(form("custom", "#80112233", "#FF445566"))
            })
            .unwrap();
        assert_eq!(tokens.themes.dark.surface, "#11223380");
        let accepted = theme.snapshot.clone();
        assert!(theme
            .read_with(now + READ_INTERVAL * 4, false, true, || Err(
                "force unavailable".into()
            ))
            .is_none());
        assert_eq!(theme.snapshot, accepted);
        let same = form("custom", "#80112233", "#FF445566");
        let (tokens, _) = theme
            .read_with(now + READ_INTERVAL * 5, false, false, || Ok(same.clone()))
            .unwrap();
        assert_eq!(tokens.themes.dark.surface, "#11223380");
        assert!(theme
            .read_with(now + READ_INTERVAL * 6, false, false, || Ok(same))
            .is_none());
    }

    #[test]
    fn dark_selection_matches_root_without_extra_trimming() {
        let mut theme = OverlayTheme::new(&original());
        let now = Instant::now();
        for (mode, system_dark, expected) in [
            ("DARK", false, true),
            ("LIGHT", true, false),
            (" dark ", false, false),
            ("CUSTOM", true, true),
            ("auto", false, false),
            ("unknown", true, true),
        ] {
            let (_, dark) = theme
                .read_with(now, system_dark, true, || {
                    Ok(form(mode, "#FFFFFFFF", "#FF000000"))
                })
                .unwrap();
            assert_eq!(dark, expected, "{mode}");
        }
    }
}
