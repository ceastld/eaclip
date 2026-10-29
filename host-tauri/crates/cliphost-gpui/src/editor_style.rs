//! Sizes shared with the confirmed fixed-fixture editor design workbench.

use std::sync::OnceLock;

use serde_json::Value;

pub(super) fn metric(name: &str, fallback: f32) -> f32 {
    static TOKENS: OnceLock<Value> = OnceLock::new();
    TOKENS
        .get_or_init(|| serde_json::from_str(crate::preview::TOKENS_JSON).expect("editor design tokens"))
        .get("editorDock")
        .and_then(|tokens| tokens.get(name))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.)
        .map(|value| value as f32)
        .unwrap_or(fallback)
}

pub(super) fn text_stats(text: &str) -> (usize, usize) {
    let characters = text.chars().count();
    let lines = if text.is_empty() {
        0
    } else {
        text.replace("\r\n", "\n").replace('\r', "\n").split('\n').count()
    };
    (characters, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_editor_sizes_share_the_frontend_source() {
        for (name, value) in [("headerHeight", 36.), ("pageTabsHeight", 40.),
            ("controlHeight", 30.), ("footerHeight", 50.), ("minWidth", 320.),
            ("width", 480.), ("maxWidth", 960.), ("propLabelWidth", 72.)] {
            assert_eq!(metric(name, -1.), value, "{name}");
        }
        assert_eq!(metric("missing", 7.), 7.);
    }

    #[test]
    fn character_statistics_count_unicode_without_an_extra_utf16_unit() {
        assert_eq!(text_stats("中文📋\nabc\n"), (8, 3));
        assert_eq!(text_stats(""), (0, 0));
        assert_eq!(text_stats("a\rb\r"), (4, 3));
        assert_eq!(text_stats("a\r\nb"), (4, 2));
    }
}
