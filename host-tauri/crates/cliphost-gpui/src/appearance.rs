//! Native equivalents of `clipListAppearanceConfig`, `useClipListSettings` and
//! the list row chrome in `app.css`. This module does not own history actions,
//! popup lifetime, process-icon loading or configuration persistence.
//!
//! Subscribe to `AppearanceEvent::Changed` and submit its narrow patch through
//! `HostState::config_update`. Apply authoritative config events with
//! `AppearanceControls::set_config`; do not replace a newer edit with an older
//! GET/save reply. The root owns the modal lease and closes it on hide/fold.

/// Kept independent of GPUI so fixture normalization and geometry can be tested
/// without creating a window, host, database, API listener or clipboard monitor.
pub mod model {
    use chrono::{DateTime, Datelike, TimeZone};
    use serde_json::{json, Value};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AppearanceSlot {
        Standard,
        Simple,
        Custom,
    }

    impl AppearanceSlot {
        pub fn normalize(raw: Option<&str>) -> Self {
            match raw.unwrap_or_default().trim().to_ascii_lowercase().as_str() {
                "simple" | "minimal" => Self::Simple,
                "custom" => Self::Custom,
                _ => Self::Standard,
            }
        }
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Standard => "standard",
                Self::Simple => "simple",
                Self::Custom => "custom",
            }
        }
        pub fn from_legacy(slot: Option<&str>, display_mode: Option<&str>) -> Self {
            if slot.is_some_and(|s| !s.trim().is_empty()) {
                Self::normalize(slot)
            } else if RowLayout::normalize(display_mode) == RowLayout::Minimal {
                Self::Simple
            } else {
                Self::Standard
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ItemChrome {
        Rounded,
        Hairline,
        Flush,
    }

    impl ItemChrome {
        pub fn normalize(raw: Option<&str>) -> Self {
            match raw.unwrap_or_default().trim().to_ascii_lowercase().as_str() {
                "hairline" => Self::Hairline,
                "flush" => Self::Flush,
                _ => Self::Rounded,
            }
        }
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Rounded => "rounded",
                Self::Hairline => "hairline",
                Self::Flush => "flush",
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RowLayout {
        Standard,
        Minimal,
    }

    impl RowLayout {
        pub fn normalize(raw: Option<&str>) -> Self {
            if raw
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("minimal")
            {
                Self::Minimal
            } else {
                Self::Standard
            }
        }
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Standard => "standard",
                Self::Minimal => "minimal",
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Density {
        Compact,
        Relaxed,
    }

    impl Density {
        pub fn normalize(raw: Option<&str>) -> Self {
            if raw
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("relaxed")
            {
                Self::Relaxed
            } else {
                Self::Compact
            }
        }
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Compact => "compact",
                Self::Relaxed => "relaxed",
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum HoverAction {
        Primary,
        Edit,
        Pin,
        Favorite,
        Delete,
    }

    /// Display order is fixed even when a persisted selection has another order.
    /// The web toolbar uses a Set and renders these five independent branches.
    pub const HOVER_ACTIONS: [HoverAction; 5] = [
        HoverAction::Primary,
        HoverAction::Edit,
        HoverAction::Pin,
        HoverAction::Favorite,
        HoverAction::Delete,
    ];
    pub const SIMPLE_HOVER_ACTIONS: [HoverAction; 3] =
        [HoverAction::Primary, HoverAction::Pin, HoverAction::Delete];

    impl HoverAction {
        pub fn parse(raw: &str) -> Option<Self> {
            Some(match raw.trim().to_ascii_lowercase().as_str() {
                "primary" => Self::Primary,
                "edit" => Self::Edit,
                "pin" => Self::Pin,
                "favorite" => Self::Favorite,
                "delete" => Self::Delete,
                _ => return None,
            })
        }
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Primary => "primary",
                Self::Edit => "edit",
                Self::Pin => "pin",
                Self::Favorite => "favorite",
                Self::Delete => "delete",
            }
        }
    }

    pub fn normalize_hover_actions(raw: Option<&Value>) -> Vec<HoverAction> {
        let Some(raw) = raw.filter(|value| !value.is_null()) else {
            return SIMPLE_HOVER_ACTIONS.to_vec();
        };
        let mut ordered = Vec::new();
        if let Some(items) = raw.as_array() {
            for item in items {
                if let Some(action) = item.as_str().and_then(HoverAction::parse) {
                    if !ordered.contains(&action) {
                        ordered.push(action);
                    }
                }
            }
        }
        ordered
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Appearance {
        pub chrome: ItemChrome,
        pub layout: RowLayout,
        pub show_process: bool,
        pub show_time: bool,
        pub hover_actions: Vec<HoverAction>,
    }

    impl Appearance {
        /// Owned values ensure controls can never mutate a built-in slot.
        pub fn standard() -> Self {
            Self {
                chrome: ItemChrome::Rounded,
                layout: RowLayout::Standard,
                show_process: true,
                show_time: true,
                hover_actions: HOVER_ACTIONS.to_vec(),
            }
        }
        pub fn simple() -> Self {
            Self {
                chrome: ItemChrome::Hairline,
                layout: RowLayout::Minimal,
                show_process: false,
                show_time: false,
                hover_actions: SIMPLE_HOVER_ACTIONS.to_vec(),
            }
        }
        pub fn normalize_custom(raw: Option<&Value>) -> Self {
            let Some(raw) = raw.filter(|value| !value.is_null()) else {
                return Self::simple();
            };
            // Missing custom object is the simple starting point. An existing
            // empty object follows each field normalizer (rounded/standard).
            Self {
                chrome: ItemChrome::normalize(raw.get("chrome").and_then(Value::as_str)),
                layout: RowLayout::normalize(raw.get("layout").and_then(Value::as_str)),
                show_process: raw.get("showProcess").and_then(Value::as_bool) == Some(true),
                show_time: raw.get("showTime").and_then(Value::as_bool) == Some(true),
                hover_actions: normalize_hover_actions(raw.get("hoverActions")),
            }
        }
        pub fn as_value(&self) -> Value {
            json!({"chrome":self.chrome.as_str(), "layout":self.layout.as_str(), "showProcess":self.show_process, "showTime":self.show_time,
                "hoverActions":self.hover_actions.iter().map(|a| a.as_str()).collect::<Vec<_>>()})
        }
        pub fn rendered_actions(&self, favorite_available: bool) -> Vec<HoverAction> {
            HOVER_ACTIONS
                .into_iter()
                .filter(|a| {
                    self.hover_actions.contains(a)
                        && (*a != HoverAction::Favorite || favorite_available)
                })
                .collect()
        }
    }

    pub fn normalize_image_max_height(raw: Option<f64>) -> i32 {
        let value = raw.unwrap_or(160.0);
        if !value.is_finite() || value <= 0.0 {
            return 160;
        }
        value.round().clamp(40.0, 640.0) as i32
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AppearanceSettings {
        pub slot: AppearanceSlot,
        /// Global density is intentionally separate from the three slots.
        pub density: Density,
        pub custom: Appearance,
        pub image_max_height_px: i32,
    }

    impl Default for AppearanceSettings {
        fn default() -> Self {
            Self {
                slot: AppearanceSlot::Standard,
                density: Density::Compact,
                custom: Appearance::simple(),
                image_max_height_px: 160,
            }
        }
    }

    impl AppearanceSettings {
        pub fn from_config(config: &Value) -> Self {
            Self {
                slot: AppearanceSlot::from_legacy(
                    config["clipListAppearanceSlot"].as_str(),
                    config["clipListDisplayMode"].as_str(),
                ),
                density: Density::normalize(config["clipListLayoutDensity"].as_str()),
                custom: Appearance::normalize_custom(config.get("clipListCustomAppearance")),
                image_max_height_px: normalize_image_max_height(
                    config["clipListImagePreviewMaxHeightPx"].as_f64(),
                ),
            }
        }
        pub fn resolved(&self) -> Appearance {
            match self.slot {
                AppearanceSlot::Standard => Appearance::standard(),
                AppearanceSlot::Simple => Appearance::simple(),
                AppearanceSlot::Custom => self.custom.clone(),
            }
        }
        pub fn row_metrics(&self) -> RowMetrics {
            RowMetrics::new(&self.resolved(), self.density)
        }
        /// Returns a narrow merge patch, never an entire stale configuration.
        pub fn apply_edit(&mut self, edit: AppearanceEdit) -> Value {
            match edit {
                AppearanceEdit::Slot(slot) => {
                    self.slot = slot;
                    return json!({"clipListAppearanceSlot":slot.as_str()});
                }
                AppearanceEdit::Density(density) => {
                    self.density = density;
                    return json!({"clipListLayoutDensity":density.as_str()});
                }
                AppearanceEdit::Chrome(chrome) => self.custom.chrome = chrome,
                AppearanceEdit::Layout(layout) => self.custom.layout = layout,
                AppearanceEdit::ShowProcess(value) => self.custom.show_process = value,
                AppearanceEdit::ShowTime(value) => self.custom.show_time = value,
                AppearanceEdit::CopyPreset(AppearanceSlot::Standard) => {
                    self.custom = Appearance::standard()
                }
                AppearanceEdit::CopyPreset(AppearanceSlot::Simple) => {
                    self.custom = Appearance::simple()
                }
                AppearanceEdit::CopyPreset(AppearanceSlot::Custom) => {}
                AppearanceEdit::HoverAction(action, enabled) => {
                    self.custom.hover_actions = if enabled {
                        HOVER_ACTIONS
                            .into_iter()
                            .filter(|item| {
                                *item == action || self.custom.hover_actions.contains(item)
                            })
                            .collect()
                    } else {
                        self.custom
                            .hover_actions
                            .iter()
                            .copied()
                            .filter(|item| *item != action)
                            .collect()
                    };
                }
            }
            self.slot = AppearanceSlot::Custom;
            json!({"clipListAppearanceSlot":"custom", "clipListCustomAppearance":self.custom.as_value()})
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AppearanceEdit {
        Slot(AppearanceSlot),
        Density(Density),
        Chrome(ItemChrome),
        Layout(RowLayout),
        ShowProcess(bool),
        ShowTime(bool),
        HoverAction(HoverAction, bool),
        CopyPreset(AppearanceSlot),
    }

    /// Exact current web CSS geometry, including cascade overrides for minimal
    /// rows. Existing shared tokens contain the early hairline-only prototype
    /// (relaxedRowPaddingY=13), so they cannot express all slot combinations.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct RowMetrics {
        pub padding_x: f32,
        pub padding_y: f32,
        pub gap_after: f32,
        pub border_width: f32,
        pub radius: f32,
        pub divider_inset: Option<f32>,
        /// Horizontal inset of the hairline row's rounded selection/hover fill.
        pub fill_inset_x: f32,
        pub header_height: f32,
        pub header_gap: f32,
        pub header_margin_bottom: f32,
        pub metadata_gap: f32,
        pub content_gap: f32,
        pub text_line_height: f32,
        pub text_max_lines: usize,
        pub minimal_metadata_reserve: f32,
    }

    impl RowMetrics {
        pub fn new(appearance: &Appearance, density: Density) -> Self {
            let relaxed = density == Density::Relaxed;
            let minimal = appearance.layout == RowLayout::Minimal;
            let (mut x, mut y) = if relaxed { (10.0, 7.0) } else { (9.0, 6.0) };
            if minimal {
                (x, y) = (6.0, 4.0);
            }
            let mut fill_inset_x = 0.0;
            let (gap_after, border_width, radius, divider_inset) = match appearance.chrome {
                ItemChrome::Rounded => (if relaxed { 7.0 } else { 4.0 }, 1.0, 8.0, None),
                // tokens minimalRowPaddingX/Y, rowInsetX, rowRadius (2026-10-14):
                // inset rounded fill, divider aligned with the text.
                ItemChrome::Hairline => {
                    x = 12.0;
                    y = if relaxed { 11.0 } else { 8.0 };
                    fill_inset_x = 2.0;
                    (0.0, 0.0, 6.0, Some(12.0))
                }
                ItemChrome::Flush => (1.0, 0.0, 6.0, None),
            };
            Self {
                padding_x: x,
                padding_y: y,
                gap_after,
                border_width,
                radius,
                divider_inset,
                fill_inset_x,
                header_height: 20.0,
                header_gap: if relaxed { 10.0 } else { 8.0 },
                header_margin_bottom: if relaxed { 7.0 } else { 3.0 },
                metadata_gap: if relaxed { 8.0 } else { 6.0 },
                content_gap: if relaxed { 7.0 } else { 4.0 },
                text_line_height: if relaxed { 1.52 } else { 1.45 },
                text_max_lines: if minimal { 3 } else { 2 },
                minimal_metadata_reserve: if appearance.show_process || appearance.show_time {
                    92.0
                } else {
                    0.0
                },
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum HoverCommand {
        Copy,
        Paste,
        Edit,
        TogglePin,
        Favorite,
        Delete,
    }

    pub fn hover_command(action: HoverAction, execute_mode: i32) -> HoverCommand {
        match action {
            HoverAction::Primary if !matches!(execute_mode, 2 | 3) => HoverCommand::Copy,
            HoverAction::Primary => HoverCommand::Paste,
            HoverAction::Edit => HoverCommand::Edit,
            HoverAction::Pin => HoverCommand::TogglePin,
            HoverAction::Favorite => HoverCommand::Favorite,
            HoverAction::Delete => HoverCommand::Delete,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RowStatistic {
        Characters(i64),
        Files(i64),
    }

    /// Natural image dimensions and metadata prepared outside rendering. A
    /// preview may exist before either dimensions or byte size is available.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct RowImageInformation {
        pub preview: bool,
        pub width: Option<u32>,
        pub height: Option<u32>,
        pub size_bytes: Option<u64>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct RowInformation {
        pub statistic: Option<RowStatistic>,
        pub image: Option<RowImageInformation>,
    }

    /// Raw list DTO values, before display normalization, snippet clipping or
    /// the eight-file preview limit. Kept separate from the host DTO for tests.
    pub struct RowInformationInput<'a> {
        pub kind: &'a str,
        pub text_length: Option<i32>,
        pub display_text: &'a str,
        pub title: &'a str,
        pub summary: &'a str,
        pub file_count: i32,
        pub file_paths: &'a [String],
        pub image_size_bytes: Option<i64>,
        pub image: RowImageInformation,
    }

    /// Mirrors ClipboardPage's standard body badges. Text-family rows retain
    /// their character badge even when a local-path image precedes the text.
    pub fn project_row_information_input(
        input: RowInformationInput<'_>,
        layout: RowLayout,
    ) -> RowInformation {
        if layout == RowLayout::Minimal {
            return RowInformation::default();
        }
        let image_item = input.kind == "ImageClipItem";
        let mut image = input.image;
        image.preview |= image_item;
        if image_item && image.size_bytes.is_none() {
            image.size_bytes = input
                .image_size_bytes
                .and_then(|size| u64::try_from(size).ok());
        }
        if image.width.zip(image.height).is_none() {
            image.width = None;
            image.height = None;
        }
        let statistic = match input.kind {
            "TextClipItem" | "HtmlClipItem" | "QQRichTextItem" => {
                let text = [input.display_text, input.title, input.summary]
                    .into_iter()
                    .find(|text| !text.is_empty())
                    .unwrap_or_default();
                Some(RowStatistic::Characters(
                    input
                        .text_length
                        .map(i64::from)
                        .unwrap_or_else(|| text.chars().count() as i64),
                ))
            }
            "FileClipItem" if !image.preview => {
                let count = if input.file_count != 0 {
                    i64::from(input.file_count)
                } else {
                    input
                        .file_paths
                        .iter()
                        .filter(|path| !path.trim().is_empty())
                        .count()
                        .max(1) as i64
                };
                Some(RowStatistic::Files(count))
            }
            _ => None,
        };
        RowInformation {
            statistic,
            image: (image.preview && (image.width.is_some() || image.size_bytes.is_some()))
                .then_some(image),
        }
    }

    /// Matches web `Math.round`, whose negative half ties round toward +infinity.
    fn js_round(value: f64) -> i64 {
        (value + 0.5).floor() as i64
    }

    pub fn format_natural_time_at<Tz: TimeZone>(
        value: &str,
        now: DateTime<Tz>,
        english: bool,
    ) -> String {
        let Ok(timestamp) = DateTime::parse_from_rfc3339(value) else {
            return String::new();
        };
        let diff = timestamp.timestamp_millis() - now.timestamp_millis();
        let abs = diff.unsigned_abs();
        let just_now = || if english { "Just now" } else { "刚刚" }.to_string();
        if abs < 15_000 {
            return just_now();
        }
        let (count, unit_zh, unit_en) = if abs < 3_600_000 {
            let count = js_round(diff as f64 / 60_000.0);
            if count == 0 {
                return just_now();
            }
            (count, "分钟", "minute")
        } else if abs < 86_400_000 {
            (js_round(diff as f64 / 3_600_000.0), "小时", "hour")
        } else if abs < 30 * 86_400_000 {
            let count = js_round(diff as f64 / 86_400_000.0);
            match (count, english) {
                (-2, false) => return "前天".into(),
                (-1, false) => return "昨天".into(),
                (1, false) => return "明天".into(),
                (2, false) => return "后天".into(),
                (-1, true) => return "Yesterday".into(),
                (1, true) => return "Tomorrow".into(),
                _ => {}
            }
            (count, "天", "day")
        } else {
            let target = timestamp.with_timezone(&now.timezone());
            if english {
                let month = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ][target.month0() as usize];
                return if target.year() == now.year() {
                    format!("{month} {}", target.day())
                } else {
                    format!("{month} {}, {}", target.day(), target.year())
                };
            }
            return if target.year() == now.year() {
                format!("{}月{}日", target.month(), target.day())
            } else {
                format!("{}年{}月{}日", target.year(), target.month(), target.day())
            };
        };
        let n = count.unsigned_abs();
        if english {
            let plural = if n == 1 { "" } else { "s" };
            if count < 0 {
                format!("{n} {unit_en}{plural} ago")
            } else {
                format!("In {n} {unit_en}{plural}")
            }
        } else if count < 0 {
            format!("{n}{unit_zh}前")
        } else {
            format!("{n}{unit_zh}后")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::Utc;

        #[test]
        fn immutable_slots_and_legacy_values_match_web() {
            let mut config = AppearanceSettings::from_config(
                &json!({"clipListDisplayMode":" MINIMAL ","clipListLayoutDensity":" relaxed "}),
            );
            assert_eq!(config.slot, AppearanceSlot::Simple);
            assert_eq!(config.resolved(), Appearance::simple());
            let original = config.custom.clone();
            let patch = config.apply_edit(AppearanceEdit::Slot(AppearanceSlot::Standard));
            assert_eq!(patch, json!({"clipListAppearanceSlot":"standard"}));
            assert_eq!(config.custom, original);
            assert_eq!(config.density, Density::Relaxed);
            assert_eq!(
                AppearanceSlot::normalize(Some("Minimal")),
                AppearanceSlot::Simple
            );
            assert_eq!(
                AppearanceSlot::from_legacy(Some("unknown"), Some("minimal")),
                AppearanceSlot::Standard
            );
        }

        #[test]
        fn absent_empty_and_explicit_empty_custom_are_distinct() {
            assert_eq!(Appearance::normalize_custom(None), Appearance::simple());
            assert_eq!(
                Appearance::normalize_custom(Some(&Value::Null)),
                Appearance::simple()
            );
            let empty = Appearance::normalize_custom(Some(&json!({})));
            assert_eq!(
                (empty.chrome, empty.layout),
                (ItemChrome::Rounded, RowLayout::Standard)
            );
            assert_eq!(empty.hover_actions, SIMPLE_HOVER_ACTIONS);
            assert!(
                Appearance::normalize_custom(Some(&json!({"hoverActions":[]})))
                    .hover_actions
                    .is_empty()
            );
            let custom = Appearance::normalize_custom(Some(
                &json!({"chrome":" FLUSH ","layout":" MINIMAL ","showTime":true,"showProcess":false,"hoverActions":[" DELETE ","unknown","primary","delete"," PIN "]}),
            ));
            assert_eq!(
                custom.hover_actions,
                [HoverAction::Delete, HoverAction::Primary, HoverAction::Pin]
            );
            assert_eq!(custom.rendered_actions(false), SIMPLE_HOVER_ACTIONS);
        }

        #[test]
        fn custom_edits_select_only_custom_and_copy_does_not_change_density() {
            let mut settings = AppearanceSettings {
                density: Density::Relaxed,
                ..Default::default()
            };
            let patch = settings.apply_edit(AppearanceEdit::CopyPreset(AppearanceSlot::Standard));
            assert_eq!(settings.slot, AppearanceSlot::Custom);
            assert_eq!(settings.custom, Appearance::standard());
            assert_eq!(
                patch["clipListCustomAppearance"],
                Appearance::standard().as_value()
            );
            assert!(patch.get("clipListLayoutDensity").is_none());
            settings.apply_edit(AppearanceEdit::HoverAction(HoverAction::Edit, false));
            assert!(!settings.custom.hover_actions.contains(&HoverAction::Edit));
            assert!(Appearance::standard()
                .hover_actions
                .contains(&HoverAction::Edit));
        }

        #[test]
        fn fixed_css_fixture_metrics_match_cascade() {
            let standard = RowMetrics::new(&Appearance::standard(), Density::Compact);
            assert_eq!(
                (
                    standard.padding_x,
                    standard.padding_y,
                    standard.gap_after,
                    standard.radius,
                    standard.border_width
                ),
                (9.0, 6.0, 4.0, 8.0, 1.0)
            );
            let simple = RowMetrics::new(&Appearance::simple(), Density::Relaxed);
            assert_eq!(
                (
                    simple.padding_x,
                    simple.padding_y,
                    simple.gap_after,
                    simple.divider_inset
                ),
                (12.0, 11.0, 0.0, Some(12.0))
            );
            // Compact simple rows follow the shared design tokens (2026-10-14).
            let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
            let compact = RowMetrics::new(&Appearance::simple(), Density::Compact);
            assert_eq!(
                (compact.padding_x, compact.padding_y, compact.fill_inset_x, compact.radius),
                (
                    tokens.layout.minimal_row_padding_x,
                    tokens.layout.minimal_row_padding_y,
                    tokens.layout.row_inset_x,
                    tokens.layout.row_radius,
                )
            );
            let custom = Appearance {
                chrome: ItemChrome::Flush,
                layout: RowLayout::Minimal,
                ..Appearance::standard()
            };
            let metrics = RowMetrics::new(&custom, Density::Relaxed);
            assert_eq!(
                (
                    metrics.padding_x,
                    metrics.padding_y,
                    metrics.gap_after,
                    metrics.radius,
                    metrics.text_max_lines,
                    metrics.minimal_metadata_reserve
                ),
                (6.0, 4.0, 1.0, 6.0, 3, 92.0)
            );
            assert_eq!(metrics.text_line_height, 1.52);
        }

        #[test]
        fn image_and_primary_modes_match_frontend() {
            assert_eq!(normalize_image_max_height(None), 160);
            assert_eq!(normalize_image_max_height(Some(f64::NAN)), 160);
            assert_eq!(normalize_image_max_height(Some(-1.0)), 160);
            assert_eq!(normalize_image_max_height(Some(23.0)), 40);
            assert_eq!(normalize_image_max_height(Some(160.5)), 161);
            assert_eq!(normalize_image_max_height(Some(900.0)), 640);
            assert_eq!(hover_command(HoverAction::Primary, 1), HoverCommand::Copy);
            assert_eq!(hover_command(HoverAction::Primary, 2), HoverCommand::Paste);
            assert_eq!(hover_command(HoverAction::Primary, 3), HoverCommand::Paste);
        }

        fn information_input<'a>(kind: &'a str, paths: &'a [String]) -> RowInformationInput<'a> {
            RowInformationInput {
                kind,
                text_length: None,
                display_text: "",
                title: "",
                summary: "",
                file_count: 0,
                file_paths: paths,
                image_size_bytes: None,
                image: RowImageInformation::default(),
            }
        }

        #[test]
        fn information_uses_full_text_length_before_snippets_and_unicode_fallback() {
            for kind in ["TextClipItem", "HtmlClipItem", "QQRichTextItem"] {
                let mut input = information_input(kind, &[]);
                input.text_length = Some(12_345);
                input.display_text = "short snippet";
                assert_eq!(
                    project_row_information_input(input, RowLayout::Standard).statistic,
                    Some(RowStatistic::Characters(12_345))
                );
            }
            let mut input = information_input("TextClipItem", &[]);
            input.display_text = "A😀中";
            input.title = "unused";
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard).statistic,
                Some(RowStatistic::Characters(3))
            );
            let mut input = information_input("HtmlClipItem", &[]);
            input.text_length = Some(0);
            input.summary = "fallback";
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard).statistic,
                Some(RowStatistic::Characters(0))
            );
        }

        #[test]
        fn information_file_count_is_not_the_preview_count() {
            let paths: Vec<_> = (0..8).map(|index| format!("C:\\file{index}.txt")).collect();
            let mut input = information_input("FileClipItem", &paths);
            input.file_count = 42;
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard).statistic,
                Some(RowStatistic::Files(42))
            );
            let paths = vec![" ".into(), "C:\\a.txt".into(), "C:\\b.txt".into()];
            assert_eq!(
                project_row_information_input(
                    information_input("FileClipItem", &paths),
                    RowLayout::Standard
                )
                .statistic,
                Some(RowStatistic::Files(2))
            );
            assert_eq!(
                project_row_information_input(
                    information_input("FileClipItem", &[]),
                    RowLayout::Standard
                )
                .statistic,
                Some(RowStatistic::Files(1))
            );
        }

        #[test]
        fn information_image_meta_prefers_prepared_size_and_rejects_negative_dto_size() {
            let mut input = information_input("ImageClipItem", &[]);
            input.image_size_bytes = Some(1024);
            input.image = RowImageInformation {
                width: Some(640),
                height: Some(480),
                size_bytes: Some(2048),
                ..Default::default()
            };
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard).image,
                Some(RowImageInformation {
                    preview: true,
                    width: Some(640),
                    height: Some(480),
                    size_bytes: Some(2048)
                })
            );
            let mut input = information_input("ImageClipItem", &[]);
            input.image_size_bytes = Some(0);
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard)
                    .image
                    .unwrap()
                    .size_bytes,
                Some(0)
            );
            let mut input = information_input("ImageClipItem", &[]);
            input.image_size_bytes = Some(-1);
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard),
                RowInformation::default()
            );
        }

        #[test]
        fn information_image_file_suppresses_file_badge_but_text_image_keeps_characters() {
            let mut input = information_input("FileClipItem", &[]);
            input.file_count = 1;
            input.image.preview = true;
            assert_eq!(
                project_row_information_input(input, RowLayout::Standard),
                RowInformation::default()
            );
            let mut input = information_input("TextClipItem", &[]);
            input.text_length = Some(20);
            input.image = RowImageInformation {
                preview: true,
                width: Some(320),
                height: Some(200),
                size_bytes: None,
            };
            let info = project_row_information_input(input, RowLayout::Standard);
            assert_eq!(info.statistic, Some(RowStatistic::Characters(20)));
            assert_eq!(info.image.unwrap().size_bytes, None);
        }

        #[test]
        fn information_minimal_and_step_rows_have_no_standard_badges() {
            let mut input = information_input("TextClipItem", &[]);
            input.text_length = Some(8000);
            input.image = RowImageInformation {
                preview: true,
                width: Some(800),
                height: Some(600),
                size_bytes: Some(4096),
            };
            assert_eq!(
                project_row_information_input(input, RowLayout::Minimal),
                RowInformation::default()
            );
            assert_eq!(
                project_row_information_input(
                    information_input("StepClipItem", &[]),
                    RowLayout::Standard
                ),
                RowInformation::default()
            );
        }

        #[test]
        fn natural_time_uses_web_negative_half_rounding_and_calendar_labels() {
            let now = DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc);
            for (value, expected) in [
                ("invalid", ""),
                ("2026-10-12T11:59:46Z", "刚刚"),
                ("2026-10-12T11:59:30Z", "刚刚"),
                ("2026-10-12T11:58:30Z", "1分钟前"),
                ("2026-10-12T11:00:00Z", "1小时前"),
                ("2026-10-11T12:00:00Z", "昨天"),
                ("2026-10-10T12:00:00Z", "前天"),
                ("2026-10-14T12:00:00Z", "后天"),
                ("2026-09-01T12:00:00Z", "9月1日"),
                ("2025-09-01T12:00:00Z", "2025年9月1日"),
            ] {
                assert_eq!(format_natural_time_at(value, now, false), expected);
            }
        }
    }
}

use gpui_rsx::rsx;
pub use model::*;

use crate::{
    preview::{DesignTokens, ThemeTokens},
    view::color,
};
use cliphost_core::i18n::{is_english, tr};
use gpui::{
    canvas, div, prelude::*, px, relative, AnyElement, App, Context, EventEmitter,
    MouseButton, SharedString, Window,
};
use crate::crisp_svg::svg;
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

fn mix(a: &str, b: &str, a_weight: f32) -> gpui::Hsla {
    let parse = |value: &str| {
        let raw = value.trim_start_matches('#');
        let packed = u32::from_str_radix(raw, 16).unwrap_or(0);
        if raw.len() == 8 {
            (packed >> 8, (packed & 255) as f32 / 255.0)
        } else {
            (packed, 1.0)
        }
    };
    let (a, a_alpha) = parse(a);
    let (b, b_alpha) = parse(b);
    let left = a_alpha * a_weight;
    let right = b_alpha * (1.0 - a_weight);
    let alpha = left + right;
    if alpha <= 0.0 {
        return gpui::rgba(0).into();
    }
    // CSS mixes premultiplied sRGB channels, then unpremultiplies the result.
    // Opaque six-digit colors retain the previous channel calculation.
    let channel = |shift: u32| {
        ((((a >> shift) & 255_u32) as f32 * left
            + ((b >> shift) & 255_u32) as f32 * right)
            / alpha)
            .round() as u32
    };
    gpui::rgba(
        (channel(16) << 24)
            | (channel(8) << 16)
            | (channel(0) << 8)
            | (alpha * 255.0).round() as u32,
    )
    .into()
}

#[cfg(test)]
mod row_color_tests {
    use super::mix;

    #[test]
    fn transparent_row_color_does_not_tint_its_visible_neighbor() {
        assert_eq!(
            mix("#FF000000", "#00FF0080", 0.5),
            gpui::rgba(0x00FF0040).into()
        );
        assert_eq!(mix("#FF000000", "#00FF0000", 0.5), gpui::rgba(0).into());
    }

    #[test]
    fn row_color_mix_respects_unequal_alpha_and_opaque_legacy_colors() {
        assert_eq!(
            mix("#FF000080", "#0000FFFF", 0.5),
            gpui::rgba(0x5500AAC0).into()
        );
        assert_eq!(mix("#FF0000", "#0000FF", 0.5), gpui::rgb(0x800080).into());
    }
}

thread_local! {
    static HOVER_SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While the drag preview captures a row, rows paint as if not hovered: no
/// hover fill and no hover-only action strip (DESIGN 2026-10-27 drag preview).
pub(crate) fn set_hover_suppressed(suppressed: bool) {
    HOVER_SUPPRESSED.with(|flag| flag.set(suppressed));
}

pub(crate) fn hover_suppressed() -> bool {
    HOVER_SUPPRESSED.with(std::cell::Cell::get)
}

/// Root adds its existing click/context/drag handlers and content to this shell.
/// Use the same unique `group` in `render_hover_actions`.
pub fn row_chrome(
    id: SharedString,
    group: SharedString,
    settings: &AppearanceSettings,
    tokens: &DesignTokens,
    dark: bool,
    selected: bool,
    active: bool,
    pinned: bool,
    last: bool,
    snapshot: bool,
) -> gpui::Stateful<gpui::Div> {
    row_chrome_with_wallpaper(
        id, group, settings, tokens, dark, selected, active, pinned, last, snapshot, false,
    )
}

/// Wallpaper tokens already contain the WebView's mixed selected/hover colors
/// and alpha. Preserve them instead of applying the opaque row mix again.
pub fn row_chrome_with_wallpaper(
    id: SharedString,
    group: SharedString,
    settings: &AppearanceSettings,
    tokens: &DesignTokens,
    dark: bool,
    selected: bool,
    active: bool,
    pinned: bool,
    last: bool,
    snapshot: bool,
    wallpaper: bool,
) -> gpui::Stateful<gpui::Div> {
    let snapshot = snapshot || hover_suppressed();
    let appearance = settings.resolved();
    let metrics = settings.row_metrics();
    let t = tokens.theme(dark);
    let rounded = appearance.chrome == ItemChrome::Rounded;
    let hairline = appearance.chrome == ItemChrome::Hairline;
    // Selection is neutral, never an accent tint (DESIGN 2026-10-14).
    let selected_fill = if wallpaper {
        color(&t.neutral_active).opacity(0.85)
    } else {
        color(&t.neutral_active)
    };
    let background = if selected && !hairline {
        selected_fill
    } else if rounded {
        color(&t.surface)
    } else {
        color(&t.surface).opacity(0.0)
    };
    let ring = color(&t.text).opacity(0.3);
    let border = if active {
        ring
    } else if selected {
        color(&t.border_secondary)
    } else {
        color(&t.border).opacity(0.7)
    };
    let hover = if wallpaper {
        color(&t.hover)
    } else if rounded {
        mix(&t.surface, &t.text, 0.93)
    } else {
        mix(&t.surface, &t.border_secondary, 0.91)
    };
    let group_for_divider = group.clone();
    div()
        .id(id)
        .group(group.clone())
        .relative()
        .w_full()
        .px(px(metrics.padding_x))
        .py(px(metrics.padding_y))
        .mb(px(metrics.gap_after))
        .when(!hairline, |row| row.rounded(px(metrics.radius)))
        .border(px(metrics.border_width))
        .border_color(border)
        .bg(background)
        .when(!hairline, |row| {
            row.hover(move |style| {
                if selected || snapshot {
                    style
                } else {
                    style.bg(hover)
                }
            })
        })
        // Hairline rows paint their fill inset and rounded, behind the content.
        .when(hairline, |row| {
            row.child(
                rsx! {
                    <div absolute top_0 bottom_0 left={px(metrics.fill_inset_x)}
                        right={px(metrics.fill_inset_x)} rounded={px(metrics.radius)} />
                }
                    .when(selected, |fill| fill.bg(selected_fill))
                    .when(!selected && !snapshot, |fill| {
                        fill.group_hover(group.clone(), move |style| style.bg(hover))
                    })
                    .when(active && !selected, |fill| fill.border_1().border_color(ring)),
            )
        })
        .when(active && !rounded && !hairline, |row| {
            row.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(metrics.radius))
                    .border_1()
                    .border_color(ring),
            )
        })
        .when(active && selected && rounded, |row| {
            row.shadow(vec![gpui::BoxShadow {
                color: color(&t.text).opacity(0.18),
                offset: gpui::point(px(0.0), px(0.0)),
                blur_radius: px(0.0),
                spread_radius: px(1.0),
            }])
        })
        // Minimal rows mark pins next to the index instead of a side bar.
        .when(pinned && appearance.layout != RowLayout::Minimal, |row| {
            row.child(
                rsx! {
                    <div absolute left_0 top={px(8.0)} bottom={px(8.0)} w={px(3.0)}
                        rounded={px(2.0)} bg={color(&t.pin)} />
                },
            )
        })
        // Callers also pass `last` when the next row is selected, so no
        // divider touches a selected fill; hovering hides the row's own one.
        .when(!last && !selected && metrics.divider_inset.is_some(), |row| {
            row.child(
                rsx! {
                    <div absolute bottom_0 left={px(metrics.divider_inset.unwrap())}
                        right={px(metrics.divider_inset.unwrap())} h={px(1.0)}
                        bg={color(&t.border).opacity(0.7)} />
                }
                    .group_hover(group_for_divider, |style| style.bg(gpui::transparent_black())),
            )
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMetadata {
    pub source_process: String,
    pub create_time: String,
    pub natural_time: String,
}

impl RowMetadata {
    pub fn new(source_process: &str, create_time: &str) -> Self {
        Self {
            source_process: source_process.trim().into(),
            create_time: create_time.into(),
            natural_time: format_natural_time_at(create_time, chrono::Local::now(), is_english()),
        }
    }
    pub fn has_process(&self, appearance: &Appearance) -> bool {
        appearance.show_process && !self.source_process.is_empty()
    }
    pub fn has_time(&self, appearance: &Appearance) -> bool {
        appearance.show_time && !self.natural_time.is_empty()
    }
    pub fn minimal_reserve(&self, appearance: &Appearance) -> f32 {
        if self.has_process(appearance) || self.has_time(appearance) {
            92.0
        } else {
            0.0
        }
    }
}

/// Pass the body classification and natural image metadata from the existing
/// prepared thumbnail/content cache; this function never probes paths or images.
/// Single image files set `image.preview`; local text-path images supply natural
/// dimensions without a file byte size, matching the web list.
pub fn project_row_information(
    item: &cliphost_core::dto::ClipboardItemDto,
    layout: RowLayout,
    image: RowImageInformation,
) -> RowInformation {
    project_row_information_input(
        RowInformationInput {
            kind: &item.kind,
            text_length: item.text_length,
            display_text: &item.display_text,
            title: &item.title,
            summary: &item.summary,
            file_count: item.file_count,
            file_paths: &item.file_paths,
            image_size_bytes: item.image_size_bytes,
            image,
        },
        layout,
    )
}

/// Attach as the last child of the relative *body*, below the header. The
/// absolute badges do not reserve space, add actions or consume row clicks.
/// Image metadata paints first and the character/file pill last, as in Web.
pub fn render_row_information(
    information: &RowInformation,
    body_width: f32,
    tokens: &DesignTokens,
    dark: bool,
) -> AnyElement {
    let t = tokens.theme(dark);
    let mut badges = div().absolute().inset_0();
    if let Some(image) = information.image {
        if let Some(label) = crate::native_content::files::format_image_meta(
            image.width,
            image.height,
            image.size_bytes,
        ) {
            badges = badges.child(
                rsx! {
                    <div absolute right={px(2.0)} bottom={px(2.0)}
                        max_w={px((body_width - 8.0).max(1.0))} px={px(6.0)} py={px(2.0)}
                        rounded={px(4.0)} border_1 border_color={color(&t.border).opacity(0.55)}
                        bg={color(&t.surface).opacity(0.58)} text_size={px(10.0)}
                        line_height={px(13.5)} text_color={color(&t.text)} text_ellipsis
                        overflow_hidden>
                        {label}
                    </div>
                },
            );
        }
    }
    if let Some(statistic) = information.statistic {
        let label = match statistic {
            RowStatistic::Characters(count) => format!("{count} {}", tr("字符", "characters")),
            RowStatistic::Files(count) => format!("{count} {}", tr("个文件", "files")),
        };
        badges = badges.child(
            rsx! {
                <div absolute right={px(2.0)} bottom={px(1.0)} px={px(6.0)} py={px(1.0)}
                    rounded={px(999.0)} border_1
                    border_color={color(&t.border_secondary).opacity(0.38)}
                    bg={color(&t.surface).opacity(0.55)} text_size={px(10.0)}
                    line_height={px(13.0)} text_color={color(&t.text_muted).opacity(0.9)}>
                    {label}
                </div>
            },
        );
    }
    badges.into_any_element()
}

fn source_process(meta: &RowMetadata, icon: Option<AnyElement>, tokens: &DesignTokens, t: &ThemeTokens) -> AnyElement {
    if let Some(key) = crate::history_sync_source::icon_name(&meta.source_process) {
        return rsx! {
            <div id={"appearance-source-process"} flex_none size={px(16.0)} flex items_center justify_center
                text_color={color(&t.text_muted).opacity(0.8)}>
                {svg().path(tokens.icons.get(key).expect("shared device icon").clone())
                    .size(px(16.0)).flex_none().text_color(color(&t.text_muted).opacity(0.8))}
            </div>
        }
            .tooltip(tooltip_builder(tr("设备同步", "Device sync").into(), t.clone()))
            .into_any_element();
    }
    let tooltip = tooltip_builder(meta.source_process.clone(), t.clone());
    if let Some(icon) = icon {
        return rsx! {
            <div id={"appearance-source-process"} flex_none size={px(16.0)}>
                {icon}
            </div>
        }
            .tooltip(tooltip)
            .into_any_element();
    }
    // The same textual fallback used by web IconControl when extracting a
    // process icon fails. Load HostState::file_icon_png(None,Some(proc),None)
    // off the UI thread and pass the resulting 16px icon when available.
    let name = if meta.source_process.to_ascii_lowercase().ends_with(".exe") {
        meta.source_process.clone()
    } else {
        format!("{}.exe", meta.source_process)
    };
    rsx! {
        <div id={"appearance-source-process"} max_w={px(88.0)} text_size={px(10.0)}
            line_height={px(14.0)} text_color={color(&t.text_muted)} text_ellipsis overflow_hidden>
            {name}
        </div>
    }
        .tooltip(tooltip)
        .into_any_element()
}

struct MetadataTooltip {
    text: String,
    theme: ThemeTokens,
}
impl Render for MetadataTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rsx! {
            <div max_w={px(300.0)} px={px(8.0)} py={px(5.0)} rounded={px(4.0)} border_1
                border_color={color(&self.theme.border)} bg={color(&self.theme.surface)}
                text_size={px(12.0)} line_height={relative(1.3)}
                text_color={color(&self.theme.text)}>
                {self.text.clone()}
            </div>
        }
    }
}
fn tooltip_builder(
    text: String,
    theme: ThemeTokens,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    move |_, cx| {
        cx.new(|_| MetadataTooltip {
            text: text.clone(),
            theme: theme.clone(),
        })
        .into()
    }
}
fn row_time(metadata: &RowMetadata, t: &ThemeTokens) -> AnyElement {
    rsx! {
        <div id={"appearance-create-time"} flex_none text_size={px(11.0)}
            line_height={relative(1.2)} text_color={color(&t.text_muted).opacity(0.8)}>
            {metadata.natural_time.clone()}
        </div>
    }
        .tooltip(tooltip_builder(metadata.create_time.clone(), t.clone()))
        .into_any_element()
}

fn digit_shortcut_label(index: usize) -> Option<&'static str> {
    ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"].get(index).copied()
}

fn row_number_width(index: usize, tokens: &DesignTokens) -> f32 {
    let p = tokens.keyboard_paste_index;
    if index < 10 { p.size } else {
        p.size.max(((index + 1).to_string().len() as f32 * p.ordinal_digit_width
            + 2.0 * p.ordinal_padding_x).ceil())
    }
}

/// Paint on the outer relative row, after its body: standard and minimal rows
/// share the same shortcut position without reserving text or thumbnail width.
/// The root owns the real routing state and the shared 120ms HOVER tween.
pub fn render_row_number(
    index: usize,
    active: bool,
    active_amount: f32,
    inactive_reason: &str,
    tokens: &DesignTokens,
    dark: bool,
) -> Option<AnyElement> {
    let shortcut = digit_shortcut_label(index);
    let label = shortcut.map(str::to_owned).unwrap_or_else(|| (index + 1).to_string());
    let active = active && shortcut.is_some();
    let active_amount = if shortcut.is_some() { active_amount } else { 0.0 };
    let t = tokens.theme(dark);
    let p = tokens.keyboard_paste_index;
    let active_background = mix(&t.accent, &t.surface, p.active_background_alpha);
    let active_text = mix(&t.accent, &t.text_secondary, p.active_text_mix);
    let border = crate::motion::mix_hsla(
        color(&t.accent).opacity(0.0),
        color(&t.accent).opacity(p.active_border_alpha),
        active_amount,
    );
    let background = crate::motion::mix_hsla(
        gpui::transparent_black(), active_background, active_amount,
    );
    let foreground = if shortcut.is_none() { color(&t.text_muted).opacity(0.8) } else { crate::motion::mix_hsla(
        color(&t.text_muted).opacity(p.paused_text_alpha), active_text, active_amount,
    ) };
    let description = if active {
        format!("{} {label} {}", tr("按", "Press"), tr(
            "粘贴此条。外部开始打字后退出数字接管；数字开头请先按 Esc。",
            "to paste this item. Typing outside releases digit keys; press Esc before entering a number.",
        ))
    } else {
        format!("{}：{inactive_reason}", tr("数字键粘贴已暂停", "Digit paste paused"))
    };
    Some(rsx! {
        <div id={SharedString::from(format!("appearance-digit-{index}"))}
            absolute top={px(p.top)} right={px(p.right)} flex items_center justify_center
            w={px(row_number_width(index, tokens))} h={px(p.size)} rounded={px(p.radius)} border_1 border_color={border}
            bg={background} text_size={px(p.font_size)} line_height={relative(1.0)}
            text_color={foreground}
            font_weight={if active { gpui::FontWeight::MEDIUM } else { gpui::FontWeight::NORMAL }}>
            {label}
        </div>
    }
        .when(shortcut.is_some(), |element| element.tooltip(tooltip_builder(description, t.clone())))
        .into_any_element())
}

/// Standard layout: source/time on the left and hover actions beside the
/// separate right-hand shortcut. Time remains visible while actions are hidden.
pub fn render_row_header(
    index: usize,
    metadata: &RowMetadata,
    process_icon: Option<AnyElement>,
    actions: AnyElement,
    settings: &AppearanceSettings,
    tokens: &DesignTokens,
    dark: bool,
) -> AnyElement {
    let appearance = settings.resolved();
    let m = settings.row_metrics();
    let t = tokens.theme(dark);
    let p = tokens.keyboard_paste_index;
    let mut meta = rsx! {
        <div flex items_center gap={px(m.metadata_gap)} min_w={px(80.0)} flex_1 />
    };
    if metadata.has_process(&appearance) {
        meta = meta.child(source_process(metadata, process_icon, tokens, t));
    }
    if metadata.has_time(&appearance) {
        meta = meta.child(row_time(metadata, t));
    }
    rsx! {
        <div flex justify_between items_center gap={px(m.header_gap)} min_h={px(m.header_height)}
            mb={px(m.header_margin_bottom)}>
            {meta}
            <div flex_none flex items_center min_w_0 mr={px(row_number_width(index, tokens) + p.action_gap)}>
                {actions}
            </div>
        </div>
    }
        .into_any_element()
}

/// Minimal layout: marks paint over the full-width content on a translucent
/// surface. Only custom process/time metadata reserves body width.
pub fn render_minimal_trail(
    index: usize,
    pinned: bool,
    metadata: &RowMetadata,
    process_icon: Option<AnyElement>,
    actions: AnyElement,
    settings: &AppearanceSettings,
    tokens: &DesignTokens,
    dark: bool,
    group: SharedString,
) -> AnyElement {
    let appearance = settings.resolved();
    let t = tokens.theme(dark);
    let l = &tokens.layout;
    let p = tokens.keyboard_paste_index;
    let metrics = settings.row_metrics();
    let pin_path = pinned.then(|| tokens.icons.get("pin-top")).flatten();
    // The old number is now the outer-row keycap. Keep metadata/pin hover
    // behavior, but end actions one gap before the keycap's left edge.
    let mut trail = rsx! {
        <div absolute top_0 right={px((p.right + row_number_width(index, tokens) + p.action_gap - metrics.padding_x).max(0.0))}
            flex items_center />
    };
    if metadata.has_process(&appearance) {
        trail = trail.child(
            div()
                .mr(px(6.0))
                .child(source_process(metadata, process_icon, tokens, t)),
        );
    }
    if metadata.has_time(&appearance) {
        trail = trail.child(div().mr(px(6.0)).child(row_time(metadata, t)));
    }
    let mut marks = div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .when(pin_path.is_some(), |marks| {
            marks
                .px(px(2.0))
                .py(px(1.0))
                .rounded(px(4.0))
                .bg(color(&t.surface).opacity(0.58))
        })
        .group_hover(group, |style| style.invisible());
    if let Some(path) = pin_path {
        marks = marks.child(
            svg()
                .path(path.clone())
                .size(px(l.pin_mark_size))
                .flex_none()
                .text_color(color(&t.pin)),
        );
    }
    trail
        .child(marks)
        // Hover actions cover the pin, while the shortcut remains visible.
        .child(rsx! {
            <div absolute right_0 top={px(-5.0)}>
                {actions}
            </div>
        })
        .into_any_element()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInvocation {
    pub item_id: String,
    pub command: HoverCommand,
    /// Painted favorite-button bounds in physical screen pixels.
    pub anchor: Option<(i32, i32, i32, i32)>,
    /// Pointer entry/exit for the favorite chooser; None is an explicit click.
    pub hovered: Option<bool>,
}
pub type HoverHandler = Rc<dyn Fn(HoverInvocation, &mut Window, &mut App)>;

fn action_label(command: HoverCommand, pinned: bool) -> &'static str {
    match command {
        HoverCommand::Copy => tr("复制", "Copy"),
        HoverCommand::Paste => tr("粘贴", "Paste"),
        HoverCommand::Edit => tr("编辑", "Edit"),
        HoverCommand::TogglePin if pinned => tr("取消置顶", "Unpin"),
        HoverCommand::TogglePin => tr("置顶", "Pin"),
        HoverCommand::Favorite => tr("收藏", "Favorite"),
        HoverCommand::Delete => tr("删除", "Delete"),
    }
}

fn action_title(command: HoverCommand, pinned: bool, execute_mode: i32) -> &'static str {
    match command {
        HoverCommand::Favorite => tr(
            "单击收藏到总目录，悬停选择文件夹",
            "Click to save to Root; hover to choose a folder",
        ),
        HoverCommand::Copy => tr(
            "复制到剪贴板（单击列表项已粘贴）",
            "Copy to clipboard (a row click pastes)",
        ),
        HoverCommand::Paste if execute_mode == 2 => tr(
            "粘贴到目标窗口（单击列表项已复制）",
            "Paste to target (a row click copies)",
        ),
        HoverCommand::Paste => tr(
            "粘贴到目标窗口（双击列表项或点此粘贴）",
            "Paste to target (double-click the row or click here)",
        ),
        _ => action_label(command, pinned),
    }
}

/// `always_visible` is the equivalent of focus-within or a coarse/touch pointer.
/// A hidden toolbar keeps its layout width and cannot intercept row clicks.
/// Narrow standard rows (<=460px) match the web's icon-only container rule.
pub fn render_hover_actions(
    item_id: &str,
    pinned: bool,
    execute_mode: i32,
    favorite_available: bool,
    settings: &AppearanceSettings,
    tokens: &DesignTokens,
    dark: bool,
    group: SharedString,
    row_width: f32,
    always_visible: bool,
    suppressed: bool,
    handler: HoverHandler,
) -> AnyElement {
    let appearance = settings.resolved();
    let t = tokens.theme(dark);
    let segmented = appearance.layout == RowLayout::Minimal;
    let narrow = row_width <= 460.0;
    let actions = appearance.rendered_actions(favorite_available);
    let mut strip = rsx! {
        <div id={SharedString::from(format!("hover-actions-{item_id}"))} flex items_center
            justify_end />
    }
        .gap(px(if segmented { 0.0 } else { 2.0 }))
        // Borderless floating bar over the index (DESIGN 2026-10-14).
        .when(segmented, |strip| {
            strip
                .p(px(2.0))
                .rounded(px(7.0))
                .bg(color(&t.surface))
                .shadow(vec![
                    gpui::BoxShadow {
                        color: color(&t.border).opacity(0.7),
                        offset: gpui::point(px(0.0), px(0.0)),
                        blur_radius: px(0.0),
                        spread_radius: px(1.0),
                    },
                    gpui::BoxShadow {
                        color: gpui::black().opacity(0.12),
                        offset: gpui::point(px(0.0), px(2.0)),
                        blur_radius: px(6.0),
                        spread_radius: px(0.0),
                    },
                ])
        })
        .when(!segmented, |strip| strip.flex_wrap())
        .when(!always_visible && !suppressed, |strip| {
            strip
                .invisible()
                .when(!hover_suppressed(), |strip| strip.group_hover(group, |style| style.visible()))
        })
        // A context menu hides painting and hit targets, while retaining the
        // toolbar's exact height/width (including wrapping and empty actions).
        .when(suppressed, |strip| strip.invisible())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    for action in actions.iter().copied() {
        let command = hover_command(action, execute_mode);
        let icon = match command {
            HoverCommand::Copy => "copy",
            HoverCommand::Paste => "paste",
            HoverCommand::Edit => "edit",
            // 置顶 is "move to top"; the pushpin stays the window topmost toggle.
            HoverCommand::TogglePin if pinned => "unpin-top",
            HoverCommand::TogglePin => "pin-top",
            HoverCommand::Favorite => "star",
            HoverCommand::Delete => "delete",
        };
        let invocation = HoverInvocation {
            item_id: item_id.into(),
            command,
            anchor: None,
            hovered: None,
        };
        let handler = handler.clone();
        let button_bounds = Rc::new(RefCell::new(None));
        let painted_bounds = button_bounds.clone();
        let hover_bounds = button_bounds.clone();
        let hover_invocation = invocation.clone();
        let hover_handler = handler.clone();
        let danger = command == HoverCommand::Delete;
        let accent = if danger { color(&t.error) } else { color(&t.text) };
        let button_group = SharedString::from(format!("hover-btn-{}-{item_id}", action.as_str()));
        let labelled = !segmented && !narrow && command != HoverCommand::Favorite;
        // Ghost buttons: the icon carries the action, the fill appears only on
        // hover. 14px keeps the 24-unit strokes at about one device pixel;
        // 11px thinned them to grey anti-aliasing.
        let mut button = div()
            .id(SharedString::from(format!(
                "hover-{}-{item_id}",
                action.as_str()
            )))
            .group(button_group.clone())
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .h(px(22.0))
            .min_w(px(22.0))
            .px(px(if labelled { 6.0 } else { 4.0 }))
            .rounded(px(4.0))
            .text_color(color(&t.text_secondary))
            .text_size(px(11.0))
            .line_height(relative(1.0))
            .cursor_pointer()
            .hover(move |style| {
                if danger {
                    style
                        .bg(color(&t.error).opacity(0.14))
                        .text_color(color(&t.error))
                } else {
                    // A text tint reads on plain, hovered and selected rows;
                    // the `hover` token is the row hover and vanishes here.
                    style.bg(color(&t.text).opacity(0.1)).text_color(color(&t.text))
                }
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                let mut invocation = invocation.clone();
                invocation.anchor = button_bounds.borrow().and_then(|bounds| crate::platform::client_rect_on_screen(window, bounds));
                handler(invocation, window, cx);
            })
            .when(command == HoverCommand::Favorite, |button| {
                button.on_hover(move |hovered, window, cx| {
                    let mut invocation = hover_invocation.clone();
                    invocation.hovered = Some(*hovered);
                    invocation.anchor = hover_bounds.borrow().and_then(|bounds| crate::platform::client_rect_on_screen(window, bounds));
                    hover_handler(invocation, window, cx);
                }).child(canvas(move |bounds, window, _| {
                    *painted_bounds.borrow_mut() = Some(bounds.intersect(&window.content_mask().bounds));
                }, |_, _, _, _| {}).absolute().top_0().left_0().size_full())
            })
            .tooltip(tooltip_builder(
                action_title(command, pinned, execute_mode).into(),
                t.clone(),
            ));
        if let Some(path) = tokens.icons.get(icon) {
            button = button.child(
                svg()
                    .path(path.clone())
                    .size(px(16.0))
                    .flex_none()
                    .text_color(color(&t.text_secondary))
                    .group_hover(button_group, move |style| style.text_color(accent)),
            );
        }
        if labelled {
            button = button.child(action_label(command, pinned));
        }
        strip = strip.child(button);
    }
    strip.into_any_element()
}

#[path = "appearance_controls.rs"]
mod controls;
pub use controls::{AppearanceControls, AppearanceEvent};
