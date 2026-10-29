//! Isolated UI fixture adapter. This module owns only in-memory records and
//! calls the existing core query/DTO code; it never initializes a host or store.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use cliphost_core::{
    dto::{clip_to_dto, ClipboardItemDto},
    host::query_clip_records,
    model::{hash_files, hash_text, ClipKind, ClipRecord},
};
use serde::{Deserialize, Serialize};

pub const FIXTURE_JSON: &str = include_str!("../../../../ui-design/clip-main.fixture.json");
pub const TOKENS_JSON: &str = include_str!("../../../../ui-design/clip-main.tokens.json");
const MAX_FIXTURE_ITEMS: usize = 1_000;
const MAX_PAGE_SIZE: usize = 1_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignTokens {
    pub schema_version: u32,
    pub window: WindowTokens,
    pub typography: TypographyTokens,
    pub layout: LayoutTokens,
    pub titlebar_search: TitlebarSearchTokens,
    #[serde(default)]
    pub keyboard_paste_index: KeyboardPasteIndexTokens,
    #[serde(default)]
    pub ceabox_proposal: CeaBoxTokens,
    #[serde(default)]
    pub file_browser_proposal: FileBrowserTokens,
    pub settings: SettingsTokens,
    pub list_settings_popover: ListSettingsPopoverTokens,
    pub themes: ThemeVariants,
    pub icons: HashMap<String, String>,
    pub scrollbar: ScrollbarTokens,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileBrowserTokens {
    pub menu_width: f32,
    pub menu_height: f32,
    pub min_panel_width: f32,
    pub panel_header_height: f32,
    pub width: f32,
    pub height: f32,
    pub min_width: f32,
    pub min_height: f32,
    pub caption_height: f32,
    pub navigation_height: f32,
    pub footer_height: f32,
    pub row_height: f32,
    pub sidebar_width: f32,
    pub narrow_sidebar_width: f32,
    pub narrow_breakpoint: f32,
}

impl Default for FileBrowserTokens {
    fn default() -> Self {
        Self { menu_width: 1180.0, menu_height: 784.0, min_panel_width: 440.0, panel_header_height: 32.0,
            width: 940.0, height: 620.0, min_width: 640.0, min_height: 440.0,
            caption_height: 39.0, navigation_height: 57.0, footer_height: 31.0,
            row_height: 39.0, sidebar_width: 168.0, narrow_sidebar_width: 140.0, narrow_breakpoint: 760.0 }
    }
}

/// Compact CeaBox layout confirmed in the frontend workbench.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CeaBoxTokens {
    pub width: f32,
    pub height: f32,
    pub min_width: f32,
    pub min_height: f32,
    pub search_height: f32,
    pub filter_height: f32,
    pub footer_height: f32,
    pub row_height: f32,
    pub row_radius: f32,
    pub row_inset: f32,
    pub row_padding: f32,
    pub icon_size: f32,
    pub icon_box_size: f32,
    pub search_font_size: f32,
    pub title_font_size: f32,
    pub meta_font_size: f32,
}

impl Default for CeaBoxTokens {
    fn default() -> Self {
        Self {
            width: 680.0, height: 510.0, min_width: 360.0, min_height: 330.0,
            search_height: 40.0, filter_height: 26.0, footer_height: 34.0,
            row_height: 54.0, row_radius: 6.0, row_inset: 8.0, row_padding: 12.0,
            icon_size: 20.0, icon_box_size: 30.0, search_font_size: 16.0,
            title_font_size: 14.0, meta_font_size: 12.0,
        }
    }
}

/// Footer list-settings design confirmed in the frontend workbench.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSettingsPopoverTokens {
    pub width: f32,
    pub height: f32,
    pub inset: f32,
    pub anchor_gap: f32,
    pub radius: f32,
    pub body_padding_x: f32,
    pub body_padding_top: f32,
    pub body_padding_bottom: f32,
    pub tab_height: f32,
    pub footer_height: f32,
    pub row_height: f32,
    pub font_size: f32,
    pub preset_height: f32,
    pub switch_width: f32,
    pub switch_height: f32,
    pub number_height: f32,
    pub choice_height: f32,
    pub control_gap: f32,
}

/// Shared titlebar search sizing confirmed in the frontend design preview.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitlebarSearchTokens {
    pub button_size: f32,
    pub min_search_width: f32,
    pub preferred_search_width: f32,
    pub min_drag_width: f32,
}

/// Digit-paste state is shown on its row (frontend `.keyboard-paste-index`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KeyboardPasteIndexTokens {
    pub size: f32,
    pub top: f32,
    pub right: f32,
    pub radius: f32,
    pub font_size: f32,
    pub action_gap: f32,
    pub ordinal_digit_width: f32,
    pub ordinal_padding_x: f32,
    pub active_border_alpha: f32,
    pub active_background_alpha: f32,
    pub active_text_mix: f32,
    pub paused_text_alpha: f32,
}

impl Default for KeyboardPasteIndexTokens {
    fn default() -> Self {
        Self {
            size: 20.0, top: 7.0, right: 9.0, radius: 5.0, font_size: 11.0,
            action_gap: 8.0, ordinal_digit_width: 8.25, ordinal_padding_x: 2.0,
            active_border_alpha: 0.24, active_background_alpha: 0.07,
            active_text_mix: 0.82, paused_text_alpha: 0.6,
        }
    }
}

/// Overlay list scrollbar (2026-10-14 confirmed): no gutter, auto-hide.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollbarTokens {
    pub hit_width: f32,
    pub thumb_width: f32,
    pub thumb_expanded_width: f32,
    pub thumb_inset_right: f32,
    pub min_thumb: f32,
    pub idle_hide_ms: f32,
    pub fade_in_ms: f32,
    pub fade_out_ms: f32,
    pub thumb_alpha: f32,
    pub thumb_hover_alpha: f32,
    pub thumb_drag_alpha: f32,
    pub track_alpha: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowTokens {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypographyTokens {
    pub font_family: String,
    pub fallback_font_family: String,
    pub font_size: f32,
    pub small_font_size: f32,
    pub title_font_size: f32,
    pub category_font_size: f32,
    pub line_height: f32,
    pub text_weight: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutTokens {
    pub title_bar_height: f32,
    pub title_bar_padding_x: f32,
    pub title_bar_gap: f32,
    pub nav_height: f32,
    pub nav_padding_x: f32,
    pub nav_gap: f32,
    pub nav_radius: f32,
    pub chrome_button_size: f32,
    pub chrome_button_gap: f32,
    pub content_padding_x: f32,
    pub content_padding_y: f32,
    pub search_height: f32,
    pub search_radius: f32,
    pub search_icon_size: f32,
    pub search_icon_inset: f32,
    pub search_input_padding_x: f32,
    pub search_action_width: f32,
    pub toolbar_gap: f32,
    pub search_margin_bottom: f32,
    pub category_height: f32,
    pub category_gap: f32,
    pub category_icon_gap: f32,
    pub category_padding_x: f32,
    pub category_radius: f32,
    pub category_margin_bottom: f32,
    pub list_padding_top: f32,
    pub list_padding_bottom: f32,
    pub row_padding_x: f32,
    pub row_padding_y: f32,
    /// `ClipListLayoutDensity = relaxed` (web: larger in-row padding).
    pub relaxed_row_padding_y: f32,
    pub row_index_width: f32,
    pub row_divider_width: f32,
    pub text_max_lines: u32,
    pub pin_width: f32,
    pub pin_inset_y: f32,
    pub image_max_width: f32,
    pub image_max_height: f32,
    pub image_radius: f32,
    pub file_chip_gap: f32,
    pub file_chip_padding_x: f32,
    pub file_chip_padding_y: f32,
    pub file_chip_radius: f32,
    pub footer_height: f32,
    pub icon_size: f32,
    pub chrome_icon_size: f32,
    pub action_icon_size: f32,
    pub scrollbar_width: f32,
    // 2026-10-14 entry reorganisation (ui-design/README.md).
    pub icon_button_size: f32,
    pub inline_icon_button_size: f32,
    pub toolbar_height: f32,
    pub search_rest_width: f32,
    pub search_min_width: f32,
    pub chip_height: f32,
    pub chip_padding_x: f32,
    pub chip_gap: f32,
    pub chip_radius: f32,
    pub row_radius: f32,
    pub row_inset_x: f32,
    pub row_meta_width: f32,
    pub pin_mark_size: f32,
    pub digit_shortcut_rows: f32,
    pub minimal_image_max_height: f32,
    pub minimal_row_padding_x: f32,
    pub minimal_row_padding_y: f32,
}

/// Native settings window and the list-settings popover share these sizes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsTokens {
    pub width: f32,
    pub height: f32,
    pub min_width: f32,
    pub min_height: f32,
    pub sidebar_width: f32,
    pub sidebar_item_height: f32,
    pub sidebar_padding: f32,
    pub content_padding_x: f32,
    pub content_padding_y: f32,
    pub page_title_font_size: f32,
    pub group_title_font_size: f32,
    pub description_font_size: f32,
    pub card_radius: f32,
    pub row_padding_x: f32,
    pub row_padding_y: f32,
    pub row_gap: f32,
    pub control_height: f32,
    pub control_width: f32,
    pub chip_height: f32,
    pub chip_padding_x: f32,
    pub switch_width: f32,
    pub switch_height: f32,
    pub panel_width: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeVariants {
    pub light: ThemeTokens,
    pub dark: ThemeTokens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeTokens {
    pub background: String,
    pub surface: String,
    pub secondary: String,
    pub tertiary: String,
    pub text: String,
    pub text_secondary: String,
    pub text_muted: String,
    pub border: String,
    pub border_secondary: String,
    pub accent: String,
    pub pin: String,
    pub selected: String,
    pub hover: String,
    pub tab_active: String,
    pub search_background: String,
    pub title_active: String,
    pub scrollbar_thumb: String,
    pub error: String,
    /// Neutral selected fill for the main window (titles, categories, rows).
    pub neutral_active: String,
    /// Paused-capture emphasis.
    pub warning: String,
}

impl DesignTokens {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let tokens: Self = serde_json::from_str(json)
            .map_err(|error| format!("Invalid clipboard design tokens: {error}"))?;
        if tokens.schema_version != 1 {
            return Err("Unsupported clipboard design tokens schemaVersion.".into());
        }
        if tokens.window.width <= 0.0
            || tokens.window.height <= 0.0
            || tokens.typography.font_size <= 0.0
            || tokens.typography.line_height <= 0.0
            || tokens.typography.font_family.trim().is_empty()
        {
            return Err("Clipboard design tokens require positive dimensions and a font.".into());
        }
        let layout = serde_json::to_value(&tokens.layout).map_err(|error| error.to_string())?;
        if layout
            .as_object()
            .unwrap()
            .values()
            .any(|value| value.as_f64().is_none_or(|number| number < 0.0))
        {
            return Err("Clipboard layout tokens must be non-negative numbers.".into());
        }
        let titlebar = tokens.titlebar_search;
        if ![
            titlebar.button_size,
            titlebar.min_search_width,
            titlebar.preferred_search_width,
            titlebar.min_drag_width,
        ]
        .iter()
        .all(|value| value.is_finite() && *value > 0.0)
            || titlebar.preferred_search_width < titlebar.min_search_width
        {
            return Err("Clipboard titlebar search tokens require positive sizes and an ordered search range.".into());
        }
        for theme in [&tokens.themes.light, &tokens.themes.dark] {
            let colors = serde_json::to_value(theme).map_err(|error| error.to_string())?;
            if colors.as_object().unwrap().values().any(|value| {
                let Some(color) = value.as_str() else {
                    return true;
                };
                color.len() != 7
                    || !color.starts_with('#')
                    || !color[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err("Clipboard theme colors must use #RRGGBB.".into());
            }
        }
        Ok(tokens)
    }

    pub fn theme(&self, dark: bool) -> &ThemeTokens {
        if dark {
            &self.themes.dark
        } else {
            &self.themes.light
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewFixture {
    pub schema_version: u32,
    pub title: String,
    pub selected_id: String,
    pub items: Vec<FixtureItem>,
    pub stress: StressFixture,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureItem {
    pub id: String,
    pub kind: ClipKind,
    pub title: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub file_paths: Vec<String>,
    #[serde(default)]
    pub is_top: bool,
    pub create_time: DateTime<Utc>,
    pub source_process_name: String,
    #[serde(default)]
    pub description: String,
    pub thumbnail: Option<PreviewThumbnail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StressFixture {
    pub count: usize,
    #[serde(default)]
    pub seed: usize,
}

/// Fixture identifiers refer to embedded assets. Host thumbnails carry an
/// original path and a version for the managed, display-only image loader.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewThumbnail {
    pub asset: String,
    pub width: u32,
    pub height: u32,
    pub label: String,
    #[serde(default)]
    pub source_version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRow {
    pub item: ClipboardItemDto,
    /// Exact shared design text. This fixture field deliberately keeps explicit
    /// line breaks; production list rendering will use the actual core DTO.
    pub preview_text: String,
    pub thumbnail: Option<PreviewThumbnail>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewPage {
    pub rows: Vec<PreviewRow>,
    pub total: usize,
    pub skip: usize,
    pub take: usize,
    pub has_more: bool,
    pub next_skip: usize,
}

/// Clone into a background worker before filtering/ranking larger fixtures.
/// No clipboard monitor, OS hook, port, persistent database or user path exists.
#[derive(Debug, Clone)]
pub struct PreviewModel {
    fixture: PreviewFixture,
    records: Vec<ClipRecord>,
    thumbnails: HashMap<String, PreviewThumbnail>,
}

impl PreviewModel {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let fixture: PreviewFixture = serde_json::from_str(json)
            .map_err(|error| format!("Invalid clipboard preview fixture: {error}"))?;
        validate_fixture(&fixture)?;
        let mut model = Self {
            fixture,
            records: Vec::new(),
            thumbnails: HashMap::new(),
        };
        model.set_stress_count(None)?;
        Ok(model)
    }

    pub fn fixture(&self) -> &PreviewFixture {
        &self.fixture
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// None restores the exact shared base fixture. Some(count) produces exactly
    /// count records, using the shared seed/index contract without randomness.
    pub fn set_stress_count(&mut self, count: Option<usize>) -> Result<(), String> {
        let stress = count.is_some();
        let count = count.unwrap_or(self.fixture.items.len());
        if count > MAX_FIXTURE_ITEMS {
            return Err(format!(
                "Preview fixture is limited to {MAX_FIXTURE_ITEMS} rows."
            ));
        }
        self.records.clear();
        self.thumbnails.clear();
        let base_count = self.fixture.items.len();
        for index in 0..count {
            let base_index = if stress {
                (index + self.fixture.stress.seed % base_count) % base_count
            } else {
                index
            };
            let base = &self.fixture.items[base_index];
            let id = if stress {
                format!("{}-stress-{index}", base.id)
            } else {
                base.id.clone()
            };
            self.records.push(to_record(base, id.clone(), index as i32));
            if let Some(thumbnail) = &base.thumbnail {
                self.thumbnails.insert(id, thumbnail.clone());
            }
        }
        Ok(())
    }

    pub fn enable_stress(&mut self) -> Result<(), String> {
        self.set_stress_count(Some(self.fixture.stress.count))
    }

    /// Keep real core matching, ranking, category and pin semantics. Only a
    /// visible page is mapped to DTOs. Fixture queries permit up to 1,000 rows;
    /// this does not change the production HostState/Bridge paging contract.
    pub fn query(&self, category: &str, query: &str, skip: usize, take: usize) -> PreviewPage {
        let category = if category == "top" { "star" } else { category };
        let take = if take == 0 {
            100
        } else {
            take.min(MAX_PAGE_SIZE)
        };
        let normalized = cliphost_core::clip_fuzzy::normalize_for_search(query);
        let (page, total) = query_clip_records(&self.records, category, &normalized, skip, take);
        let query = (!normalized.is_empty()).then_some(normalized.as_str());
        let rows: Vec<PreviewRow> = page
            .iter()
            .map(|record| PreviewRow {
                // image_path is deliberately None: clip_to_dto therefore cannot
                // call filesystem metadata for a preview image.
                item: clip_to_dto(record, query),
                preview_text: record
                    .title
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| record.body_text.as_deref().unwrap_or(""))
                    .to_string(),
                thumbnail: self.thumbnails.get(&record.id).cloned(),
            })
            .collect();
        let next_skip = skip.saturating_add(rows.len());
        PreviewPage {
            rows,
            total,
            skip,
            take,
            has_more: next_skip < total,
            next_skip,
        }
    }

    /// A local display-state exercise, never a persisted product action.
    pub fn toggle_pin(&mut self, id: &str) -> bool {
        let Some(record) = self.records.iter_mut().find(|record| record.id == id) else {
            return false;
        };
        record.is_top = !record.is_top;
        true
    }
}

fn validate_fixture(fixture: &PreviewFixture) -> Result<(), String> {
    if fixture.schema_version != 1 {
        return Err("Unsupported clipboard preview fixture schemaVersion.".into());
    }
    if fixture.items.is_empty() || fixture.items.len() > MAX_FIXTURE_ITEMS {
        return Err(
            "Clipboard preview fixture must contain a bounded, non-empty item list.".into(),
        );
    }
    if fixture.stress.count == 0 || fixture.stress.count > MAX_FIXTURE_ITEMS {
        return Err("Clipboard preview stress count is outside the supported range.".into());
    }
    let mut ids = HashSet::new();
    for item in &fixture.items {
        if item.id.trim().is_empty() || !ids.insert(item.id.as_str()) {
            return Err("Clipboard preview item ids must be non-empty and unique.".into());
        }
        if let Some(thumbnail) = &item.thumbnail {
            if thumbnail.width == 0 || thumbnail.height == 0 || thumbnail.asset.trim().is_empty() {
                return Err(format!("Invalid thumbnail fixture for {}.", item.id));
            }
        }
    }
    if !ids.contains(fixture.selected_id.as_str()) {
        return Err("Clipboard preview selectedId must refer to a fixture item.".into());
    }
    Ok(())
}

fn to_record(item: &FixtureItem, id: String, sub_index: i32) -> ClipRecord {
    ClipRecord {
        storage_revision: None,
        id,
        kind: item.kind,
        title: item.title.clone(),
        body_text: Some(item.text.clone()),
        html_data: None,
        html_source_url: None,
        qq_rich_text: None,
        file_paths: item.file_paths.clone(),
        image_path: None,
        is_top: item.is_top,
        sub_index,
        create_time: item.create_time,
        update_time: item.create_time,
        source_process_name: item.source_process_name.clone(),
        content_hash: if item.kind == ClipKind::FileClipItem {
            hash_files(&item.file_paths)
        } else {
            hash_text(&item.text)
        },
        last_access: None,
        usage_count: 0,
        execute_param: String::new(),
        description: item.description.clone(),
        perceptual_hash: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_tokens_cover_both_themes_and_design_geometry() {
        let tokens = DesignTokens::from_json(TOKENS_JSON).unwrap();
        assert!(
            tokens.window.width > tokens.layout.row_index_width + 2.0 * tokens.layout.row_padding_x
        );
        assert!(
            tokens.window.height
                > tokens.layout.title_bar_height
                    + tokens.layout.footer_height
                    + tokens.layout.search_height
                    + tokens.layout.category_height
        );
        assert_ne!(
            tokens.theme(false).background,
            tokens.theme(true).background
        );
        assert!(tokens.layout.image_max_width > 0.0);
        assert!(tokens.icons.contains_key("search"));
    }

    #[test]
    fn shared_titlebar_search_tokens_round_trip_and_reject_invalid_search_ranges() {
        let mut json: serde_json::Value = serde_json::from_str(TOKENS_JSON).unwrap();
        let tokens = DesignTokens::from_json(TOKENS_JSON).unwrap();
        let round_trip: TitlebarSearchTokens =
            serde_json::from_value(serde_json::to_value(tokens.titlebar_search).unwrap()).unwrap();
        assert_eq!(
            (
                round_trip.button_size,
                round_trip.min_search_width,
                round_trip.preferred_search_width,
                round_trip.min_drag_width,
            ),
            (
                tokens.titlebar_search.button_size,
                tokens.titlebar_search.min_search_width,
                tokens.titlebar_search.preferred_search_width,
                tokens.titlebar_search.min_drag_width,
            )
        );
        json["titlebarSearch"]["preferredSearchWidth"] = serde_json::json!(89);
        assert!(DesignTokens::from_json(&json.to_string()).is_err());
        json["titlebarSearch"]["preferredSearchWidth"] = serde_json::json!(180);
        json["titlebarSearch"]["buttonSize"] = serde_json::json!(0);
        assert!(DesignTokens::from_json(&json.to_string()).is_err());
    }

    #[test]
    fn shared_fixture_maps_through_core_without_image_paths() {
        let model = PreviewModel::from_json(FIXTURE_JSON).unwrap();
        assert!(model
            .records
            .iter()
            .all(|record| record.image_path.is_none()));
        let page = model.query("all", "", 0, 100);
        assert_eq!(page.total, model.len());
        assert_eq!(page.rows.len(), model.len());
        assert!(page.rows.iter().any(|row| row.thumbnail.is_some()));
        assert!(page
            .rows
            .iter()
            .all(|row| row.item.image_size_bytes.is_none()));
        let mut unpinned_seen = false;
        for row in &page.rows {
            if !row.item.is_top {
                unpinned_seen = true;
            }
            assert!(!row.item.is_top || !unpinned_seen);
        }
        for category in ["text", "image", "file", "url"] {
            let page = model.query(category, "", 0, 100);
            assert!(page.total > 0, "Shared fixture must cover {category}");
            assert!(page
                .rows
                .iter()
                .all(|row| cliphost_core::clip_filter::matches_with_text(
                    ClipKind::parse(&row.item.kind),
                    row.item.is_top,
                    Some(&row.preview_text),
                    category,
                )));
        }
    }

    #[test]
    fn real_core_search_handles_chinese_pinyin_and_dto_highlights() {
        let mut model = PreviewModel::from_json(FIXTURE_JSON).unwrap();
        let mut item = model.records[0].clone();
        item.id = "isolated-search".into();
        item.kind = ClipKind::TextClipItem;
        item.title = None;
        item.body_text = Some("剪贴板同步".into());
        item.file_paths.clear();
        item.description.clear();
        model.records = vec![item];
        for query in ["剪贴板", "jiantieban", "jtb"] {
            let page = model.query("text", query, 0, 100);
            assert_eq!(page.total, 1, "Core should match {query}");
            assert_eq!(page.rows[0].item.id, "isolated-search");
            assert!(!page.rows[0].item.highlight_hit_ranges.is_empty());
        }
        assert_eq!(
            model
                .query("text", "definitely_missing_token", 0, 100)
                .total,
            0
        );
        assert_eq!(model.query("image", "剪贴板", 0, 100).total, 0);
    }

    #[test]
    fn stress_is_deterministic_and_paginates_with_no_duplicate_ids() {
        let mut first = PreviewModel::from_json(FIXTURE_JSON).unwrap();
        first.set_stress_count(Some(1_000)).unwrap();
        let mut second = PreviewModel::from_json(FIXTURE_JSON).unwrap();
        second.set_stress_count(Some(1_000)).unwrap();
        assert_eq!(
            serde_json::to_value(&first.records).unwrap(),
            serde_json::to_value(&second.records).unwrap()
        );
        let head = first.query("all", "", 0, 500);
        let tail = first.query("all", "", head.next_skip, 500);
        assert_eq!(head.total, 1_000);
        assert_eq!(head.rows.len(), 500);
        assert!(head.has_more);
        assert_eq!(tail.rows.len(), 500);
        assert!(!tail.has_more);
        let ids: HashSet<&str> = head
            .rows
            .iter()
            .chain(&tail.rows)
            .map(|row| row.item.id.as_str())
            .collect();
        assert_eq!(ids.len(), 1_000);
        assert_eq!(first.query("all", "", 1_000, 100).rows.len(), 0);
        first.set_stress_count(None).unwrap();
        assert_eq!(first.len(), first.fixture.items.len());
    }

    #[test]
    fn fixture_validation_rejects_duplicate_ids_and_unknown_selection() {
        let mut fixture: PreviewFixture = serde_json::from_str(FIXTURE_JSON).unwrap();
        fixture.items.push(fixture.items[0].clone());
        assert!(PreviewModel::from_json(&serde_json::to_string(&fixture).unwrap()).is_err());
        fixture.items.pop();
        fixture.selected_id = "absent".into();
        assert!(PreviewModel::from_json(&serde_json::to_string(&fixture).unwrap()).is_err());
    }
}
