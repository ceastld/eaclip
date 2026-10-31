use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{ClipKind, ClipRecord, DismissBehavior, FavorRecord};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HighlightHitRangeDto {
    pub start: i32,
    pub end: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardItemDto {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub display_text: String,
    pub highlighted_text: String,
    pub highlight_hit_ranges: Vec<HighlightHitRangeDto>,
    pub file_count: i32,
    pub file_paths: Vec<String>,
    pub is_top: bool,
    pub create_time: DateTime<Utc>,
    pub kind: String,
    pub source_process_name: String,
    pub has_details: bool,
    pub image_size_bytes: Option<i64>,
    pub text_length: Option<i32>,
    pub im_rich_origin: Option<String>,
    pub im_rich_segments: Vec<serde_json::Value>,
    pub step_presentation: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardItemDetailDto {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub kind: String,
    pub create_time: DateTime<Utc>,
    pub is_top: bool,
    pub source_process_name: String,
    pub raw_json: String,
    pub revision: String,
    pub file_count: i32,
    pub file_paths: Vec<String>,
    pub body_text: Option<String>,
    pub html_data: Option<String>,
    pub html_source_url: Option<String>,
    pub qq_rich_text: Option<String>,
    pub im_rich_origin: Option<String>,
    pub im_rich_segments: Vec<serde_json::Value>,
    pub text_length: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagedClipItemsResponse {
    pub items: Vec<ClipboardItemDto>,
    pub hits: Vec<serde_json::Value>,
    pub total: i32,
    pub skip: i32,
    pub take: i32,
    pub has_more: bool,
    pub next_skip: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClipListRequest {
    pub query: Option<String>,
    #[serde(default = "default_type")]
    pub r#type: Option<String>,
    pub filter_item: Option<ClipFilterItemDto>,
    #[serde(default)]
    pub skip: i32,
    #[serde(default = "default_take")]
    pub take: i32,
    pub sort_type: Option<i32>,
    pub usage_filter: Option<i32>,
    pub create_time_filter: Option<i32>,
    #[serde(default)]
    pub create_time_filter_start: Option<Value>,
    #[serde(default)]
    pub create_time_filter_end: Option<Value>,
    #[serde(default)]
    pub create_time_filter_date: Option<Value>,
    pub process_filter: Option<ClipProcessFilterSpec>,
    pub projection: Option<String>,
    pub web_session_id: Option<String>,
}

fn default_type() -> Option<String> {
    Some("all".into())
}

fn default_take() -> i32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClipProcessFilterSpec {
    pub include_exe_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ClipFilterItemDto {
    pub id: String,
    pub display_name: String,
    #[serde(default = "default_all")]
    pub r#type: String,
    pub item_type: i32,
    pub star_type: i32,
    pub icon: String,
}

fn default_all() -> String {
    "all".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ClipConfigDto {
    /// Live session flag (C# `IClipDataService.IsMonitorOn`). Not persisted on `ClipConfig.json`.
    pub clip_monitor_on: bool,
    pub using_number_key_to_paste: bool,
    pub max_item_size: i32,
    pub non_pinned_item_count: i32,
    pub pinned_item_count: i32,
    pub allow_duplicate_copy: bool,
    pub show_copy_message: bool,
    pub ui_theme_mode: String,
    /// Interface language preference: system, zh-CN or en.
    #[serde(alias = "UiLanguage")]
    pub ui_language: String,
    pub clip_list_execute_mode: i32,
    pub clip_list_separate_pinned_ui: bool,
    pub auto_sort_after_paste: Option<bool>,
    pub clip_list_image_hover_preview_enabled: bool,
    pub clip_list_image_preview_max_height_px: i32,
    pub clip_list_layout_density: String,
    pub clip_list_display_mode: String,
    pub clip_list_appearance_slot: Option<String>,
    /// C# `ClipConfig.ClipListAppearanceMigrated`. Persisted; omitted from GET DTO.
    #[serde(default)]
    pub clip_list_appearance_migrated: bool,
    #[serde(default)]
    pub clip_list_custom_appearance: Option<ClipListCustomAppearanceDto>,
    pub enable_edge_hide: bool,
    pub dismiss_behavior: String,
    pub enable_no_focus_mode: bool,
    pub run_as_administrator: bool,
    pub run_as_administrator_effective: bool,
    pub process_elevated: bool,
    pub is_debug_configuration: bool,
    pub cold_start_elevation_suppressed: bool,
    pub window_background: Option<String>,
    pub font_foreground: Option<String>,
    pub window_background_image: Option<String>,
    pub window_background_blend: i32,
    #[serde(default = "default_true")]
    pub window_background_frosted_glass: bool,
    #[serde(default, alias = "BlackList")]
    pub black_list: String,
    #[serde(default, alias = "ClearFilterWhileHide")]
    pub clear_filter_while_hide: bool,
    #[serde(default = "default_true", alias = "AutoScrollToTop")]
    pub auto_scroll_to_top: bool,
    /// One-time upgrade to the 2026-10-27 default-on. Persisted; omitted from GET.
    #[serde(default)]
    pub auto_scroll_to_top_default_applied: bool,
    #[serde(default = "default_auto_scroll_to_top_idle_seconds", alias = "AutoScrollToTopIdleSeconds")]
    pub auto_scroll_to_top_idle_seconds: u32,
    #[serde(default = "default_true", alias = "CollectQuickerSteps")]
    pub collect_quicker_steps: bool,
    #[serde(default, alias = "IgnoreWhiteSpace")]
    pub ignore_white_space: bool,
    #[serde(default, alias = "DefaultPasteDelay")]
    pub default_paste_delay: i32,
    #[serde(default, alias = "PlainTextPasteByDefault")]
    pub plain_text_paste_by_default: bool,
    #[serde(default, alias = "StandardPaste")]
    pub standard_paste: bool,
    #[serde(default, alias = "UseWinVShortcut")]
    pub use_win_v_shortcut: bool,
    #[serde(default, alias = "PasteWithNewLine")]
    pub paste_with_new_line: bool,
    #[serde(default = "default_split_max", alias = "SplitLinesMaxCount")]
    pub split_lines_max_count: i32,
    #[serde(default, alias = "ShowWindowLocation")]
    pub show_window_location: String,
    /// C# `ClipConfig.ShowWindowLocation2` (keyboard slot). Persisted; omitted from GET DTO.
    #[serde(default = "default_show_window_location", alias = "ShowWindowLocation2")]
    pub show_window_location2: String,
    /// C# `ClipConfig.ShowWindowLocation3` default enum 0 (`NA`). Persisted; omitted from GET DTO.
    #[serde(default = "default_show_window_location3", alias = "ShowWindowLocation3")]
    pub show_window_location3: String,
    #[serde(default, alias = "UsePerTriggerShowWindowLocation")]
    pub use_per_trigger_show_window_location: bool,
    #[serde(default, alias = "FontFamilyString")]
    pub font_family_string: String,
    #[serde(default = "default_toast_placement", alias = "CopyToastPlacement")]
    pub copy_toast_placement: String,
    #[serde(default, alias = "ClipChangedActionId")]
    pub clip_changed_action_id: String,
    #[serde(default, alias = "TextProcessExpresstion")]
    pub text_process_expression: String,
    #[serde(default, alias = "UseShiftInsertToPasteProc")]
    pub use_shift_insert_to_paste_proc: String,
    #[serde(default, alias = "EnableVvTabTextInstruction")]
    pub enable_vv_tab_text_instruction: bool,
    #[serde(default, alias = "DoNotRemindOnClear")]
    pub do_not_remind_on_clear: bool,
}

fn default_toast_placement() -> String {
    "BottomRight".into()
}

fn default_show_window_location() -> String {
    "MouseRightBottom".into()
}

fn default_show_window_location3() -> String {
    "NA".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipListCustomAppearanceDto {
    pub chrome: String,
    pub layout: String,
    pub show_process: bool,
    pub show_time: bool,
    pub hover_actions: Vec<String>,
}

impl Default for ClipListCustomAppearanceDto {
    fn default() -> Self {
        Self {
            chrome: "hairline".into(),
            layout: "minimal".into(),
            show_process: false,
            show_time: false,
            hover_actions: vec!["primary".into(), "pin".into(), "delete".into()],
        }
    }
}

impl ClipConfigDto {
    pub fn clip_list_execute_mode_name(&self) -> &'static str {
        match self.clip_list_execute_mode {
            2 => "copythendoubleclickpaste",
            3 => "doubleclickonly",
            _ => "singleclickpaste",
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_split_max() -> i32 {
    500
}

fn default_auto_scroll_to_top_idle_seconds() -> u32 {
    60
}

impl Default for ClipConfigDto {
    fn default() -> Self {
        Self {
            clip_monitor_on: true,
            using_number_key_to_paste: true,
            max_item_size: 100,
            non_pinned_item_count: 0,
            pinned_item_count: 0,
            allow_duplicate_copy: false,
            show_copy_message: false,
            ui_theme_mode: "auto".into(),
            ui_language: "system".into(),
            clip_list_execute_mode: 1,
            clip_list_separate_pinned_ui: false,
            auto_sort_after_paste: Some(false),
            clip_list_image_hover_preview_enabled: false,
            clip_list_image_preview_max_height_px: 160,
            clip_list_layout_density: "compact".into(),
            clip_list_display_mode: "standard".into(),
            clip_list_appearance_slot: Some("standard".into()),
            clip_list_appearance_migrated: false,
            clip_list_custom_appearance: Some(ClipListCustomAppearanceDto::default()),
            enable_edge_hide: true,
            dismiss_behavior: DismissBehavior::EdgeHide.as_str().into(),
            enable_no_focus_mode: true,
            run_as_administrator: false,
            run_as_administrator_effective: false,
            process_elevated: false,
            is_debug_configuration: cfg!(debug_assertions),
            cold_start_elevation_suppressed: false,
            window_background: None,
            font_foreground: None,
            window_background_image: None,
            window_background_blend: 70,
            window_background_frosted_glass: true,
            black_list: String::new(),
            clear_filter_while_hide: false,
            auto_scroll_to_top: true,
            auto_scroll_to_top_default_applied: false,
            auto_scroll_to_top_idle_seconds: default_auto_scroll_to_top_idle_seconds(),
            collect_quicker_steps: true,
            ignore_white_space: false,
            default_paste_delay: 0,
            plain_text_paste_by_default: false,
            standard_paste: false,
            use_win_v_shortcut: false,
            paste_with_new_line: false,
            split_lines_max_count: 500,
            show_window_location: default_show_window_location(),
            show_window_location2: default_show_window_location(),
            show_window_location3: default_show_window_location3(),
            use_per_trigger_show_window_location: false,
            font_family_string: String::new(),
            copy_toast_placement: default_toast_placement(),
            clip_changed_action_id: String::new(),
            text_process_expression: String::new(),
            use_shift_insert_to_paste_proc: String::new(),
            enable_vv_tab_text_instruction: false,
            do_not_remind_on_clear: false,
        }
    }
}

/// C# `ClipChangedEventBroadcaster.MaxDeltaItems`.
pub const CLIP_CHANGED_MAX_DELTA_ITEMS: usize = 48;

pub fn event_unix_ts_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// C# `BuildClipDelta`: `{ ts, op: delta|reload, items, ids }`.
pub fn clip_changed_delta(items: &[ClipRecord], remove_ids: &[String]) -> Value {
    let ts = event_unix_ts_ms();
    if items.is_empty() && remove_ids.is_empty()
        || items.len() + remove_ids.len() > CLIP_CHANGED_MAX_DELTA_ITEMS
    {
        return json!({ "ts": ts, "op": "reload" });
    }
    let items: Vec<Value> = items
        .iter()
        .map(|item| serde_json::to_value(clip_to_dto(item, None)).unwrap_or(Value::Null))
        .collect();
    json!({
        "ts": ts,
        "op": "delta",
        "items": items,
        "ids": remove_ids,
    })
}

/// C# `BuildFavorDelta`: `{ ts, op: delta|reload, upsertIds, ids }`.
pub fn favorites_changed_delta(upsert_ids: &[String], remove_ids: &[String]) -> Value {
    let ts = event_unix_ts_ms();
    if upsert_ids.len() + remove_ids.len() > CLIP_CHANGED_MAX_DELTA_ITEMS {
        return json!({ "ts": ts, "op": "reload" });
    }
    json!({
        "ts": ts,
        "op": "delta",
        "upsertIds": upsert_ids,
        "ids": remove_ids,
    })
}

pub fn clip_to_dto(item: &ClipRecord, query: Option<&str>) -> ClipboardItemDto {
    let title = to_preview(&list_primary_label(item), TITLE_PREVIEW_LENGTH);
    let mut summary = to_preview(&list_summary_source(item), SUMMARY_PREVIEW_LENGTH);
    let mut display = resolve_display_text(&title, &summary);
    let (mut highlighted_text, mut highlight_hit_ranges) =
        highlight_list_row(item, &display, query);
    let (mut file_count, mut file_paths) = clip_list_file_meta(item, query);
    let (im_rich_origin, im_rich_segments, im_file_count, im_image_paths) =
        clip_list_im_meta(item);
    if im_file_count > 0 {
        file_count = im_file_count;
        file_paths = im_image_paths;
    }
    let step_presentation = clip_list_step_presentation(item);
    if let Some(step) = step_presentation.as_ref() {
        if let Some(preview) = step.get("previewText").and_then(|v| v.as_str()) {
            if !preview.trim().is_empty() {
                summary = to_preview(preview, SUMMARY_PREVIEW_LENGTH);
                display = resolve_display_text(&title, &summary);
                let (h, r) = highlight_list_row(item, &display, query);
                highlighted_text = h;
                highlight_hit_ranges = r;
            }
        }
    }
    ClipboardItemDto {
        id: item.id.clone(),
        title,
        summary,
        display_text: display.clone(),
        highlighted_text,
        highlight_hit_ranges,
        file_count,
        file_paths,
        is_top: item.is_top,
        create_time: item.create_time,
        kind: item.kind.as_str().into(),
        source_process_name: item.source_process_name.clone(),
        has_details: true,
        image_size_bytes: image_clip_file_size_bytes(item),
        text_length: item.text_length(),
        im_rich_origin,
        im_rich_segments,
        step_presentation,
    }
}

pub fn clip_to_detail(item: &ClipRecord) -> ClipboardItemDetailDto {
    // C# `ToDetailDto`: Title is stored title only (not VisualTitle / list preview).
    let title = item.title.clone().unwrap_or_default();
    let summary = item.csharp_summary();
    let file_paths = clip_detail_file_paths(item);
    let (im_rich_origin, im_rich_segments) = qq_im_meta(
        item.kind == ClipKind::QQRichTextItem,
        &item.source_process_name,
        item.qq_rich_text.as_deref(),
        false,
        false,
    );
    let raw_json = serde_json::to_string(item).unwrap_or_default();
    ClipboardItemDetailDto {
        id: item.id.clone(),
        title,
        summary,
        kind: item.kind.as_str().into(),
        create_time: item.create_time,
        is_top: item.is_top,
        source_process_name: item.source_process_name.clone(),
        revision: record_edit_revision(&raw_json, item.storage_revision.as_deref()),
        raw_json,
        file_count: file_paths.len() as i32,
        file_paths,
        body_text: csharp_detail_body_text(item),
        html_data: csharp_detail_html_data(item),
        html_source_url: csharp_detail_html_source_url(item),
        qq_rich_text: csharp_detail_qq_rich_text(item),
        im_rich_origin,
        im_rich_segments,
        text_length: item.text_length(),
    }
}

/// Same opaque revision contract as C# `ClipItemEditRevision.FromRawJson`.
/// Hash the exact detail snapshot, including metadata changed by another window.
pub fn edit_revision(raw_json: &str) -> String {
    format!("sha256:{}", crate::model::sha256_hex(raw_json.as_bytes()).to_ascii_uppercase())
}

/// Core storage owns the content/structure token. Usage and display projections
/// must not turn into editor conflicts. Legacy snapshots keep their wire hash.
pub(crate) fn record_edit_revision(raw_json: &str, stored: Option<&str>) -> String {
    stored
        .filter(|token| token.starts_with("core2:"))
        .map(str::to_owned)
        .unwrap_or_else(|| edit_revision(raw_json))
}

/// C# `string.IsNullOrWhiteSpace(htmlItem.HtmlSourceUrl) ? null : HtmlSourceUrl`.
fn whitespace_to_none(value: Option<&str>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty()).map(|s| s.to_string())
}

fn csharp_text_clip_kind(kind: ClipKind) -> bool {
    matches!(
        kind,
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
    )
}

fn csharp_html_clip_kind(kind: ClipKind) -> bool {
    matches!(kind, ClipKind::HtmlClipItem | ClipKind::QQRichTextItem)
}

/// C# `ToDetailDto` only fills `BodyText` when `item is TextClipItem` (Html/QQ inherit).
fn csharp_detail_body_text(item: &ClipRecord) -> Option<String> {
    if csharp_text_clip_kind(item.kind) {
        item.body_text.clone()
    } else {
        None
    }
}

fn csharp_detail_html_data(item: &ClipRecord) -> Option<String> {
    if csharp_html_clip_kind(item.kind) {
        item.html_data.clone()
    } else {
        None
    }
}

fn csharp_detail_html_source_url(item: &ClipRecord) -> Option<String> {
    if csharp_html_clip_kind(item.kind) {
        whitespace_to_none(item.html_source_url.as_deref())
    } else {
        None
    }
}

fn csharp_detail_qq_rich_text(item: &ClipRecord) -> Option<String> {
    if item.kind == ClipKind::QQRichTextItem {
        item.qq_rich_text.clone()
    } else {
        None
    }
}

/// C# `TryGetImageClipFileSizeBytes`: ImageClipItem + File.Exists then Length.
fn image_clip_file_size_bytes(item: &ClipRecord) -> Option<i64> {
    if item.kind != ClipKind::ImageClipItem {
        return None;
    }
    let path = item.image_path.as_deref()?.trim();
    if path.is_empty() {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_file() {
        Some(meta.len() as i64)
    } else {
        None
    }
}

const FILE_LIST_PREVIEW_LIMIT: usize = 10;

/// C# `ClipboardItemDtoMapper.BuildFileListForList` + image FullPath list row.
fn clip_list_file_meta(item: &ClipRecord, query: Option<&str>) -> (i32, Vec<String>) {
    if item.kind == ClipKind::ImageClipItem {
        if let Some(path) = item.image_path.as_ref().filter(|s| !s.trim().is_empty()) {
            return (1, vec![path.clone()]);
        }
        return (0, Vec::new());
    }
    if item.kind != ClipKind::FileClipItem {
        return (0, Vec::new());
    }
    let all = keep_non_whitespace_paths(&item.file_paths);
    if all.is_empty() {
        return (0, Vec::new());
    }
    let query = query.map(str::trim).filter(|s| !s.is_empty());
    if let Some(query) = query {
        let matched: Vec<String> = all
            .into_iter()
            .filter(|path| crate::clip_fuzzy::pinyin_search(query, &[path.as_str()]))
            .collect();
        let count = matched.len() as i32;
        (
            count,
            matched.into_iter().take(FILE_LIST_PREVIEW_LIMIT).collect(),
        )
    } else {
        let count = all.len() as i32;
        (count, all.into_iter().take(FILE_LIST_PREVIEW_LIMIT).collect())
    }
}

/// C# `ToDetailDto` FilePaths: FileClipItem only (not ImageClipItem FullPath).
fn clip_detail_file_paths(item: &ClipRecord) -> Vec<String> {
    if item.kind != ClipKind::FileClipItem {
        return Vec::new();
    }
    keep_non_whitespace_paths(&item.file_paths)
}

/// C# `Where(!string.IsNullOrWhiteSpace)` keeps the original string (no Trim).
fn keep_non_whitespace_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| !p.trim().is_empty())
        .cloned()
        .collect()
}

fn clip_list_im_meta(item: &ClipRecord) -> (Option<String>, Vec<Value>, i32, Vec<String>) {
    if item.kind != ClipKind::QQRichTextItem {
        return (None, Vec::new(), 0, Vec::new());
    }
    let Some(raw) = item.qq_rich_text.as_deref() else {
        return (None, Vec::new(), 0, Vec::new());
    };
    let origin = crate::im_rich::resolve_origin(&item.source_process_name, raw);
    let parsed = crate::im_rich::parse_segments(raw, origin);
    let segments = crate::im_rich::limit_for_list_preview(&parsed)
        .iter()
        .map(crate::im_rich::segment_to_dto)
        .collect();
    let image_paths = crate::im_rich::collect_image_paths_for_list(&parsed);
    let file_count = image_paths.len() as i32;
    (
        Some(origin.as_api_token().to_string()),
        segments,
        file_count,
        image_paths,
    )
}

fn clip_list_step_presentation(item: &ClipRecord) -> Option<Value> {
    // C# `TryGetStepPresentation`: empty StepData or empty preview → null.
    if item.kind != ClipKind::StepClipItem {
        return None;
    }
    let data = item.body_text.as_deref().unwrap_or("");
    if data.trim().is_empty() {
        return None;
    }
    let analysis = crate::quicker_steps::analyze(data);
    let has_lines = analysis
        .get("previewLines")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    let has_text = analysis
        .get("previewText")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    if has_lines || has_text {
        Some(analysis)
    } else {
        None
    }
}

/// `list_preview` applies Take(12). `skip_whitespace` matches favor list empty QQ → null origin.
fn qq_im_meta(
    is_qq: bool,
    process: &str,
    raw: Option<&str>,
    list_preview: bool,
    skip_whitespace: bool,
) -> (Option<String>, Vec<Value>) {
    if !is_qq {
        return (None, Vec::new());
    }
    let Some(raw) = raw else {
        return (None, Vec::new());
    };
    if skip_whitespace && raw.trim().is_empty() {
        return (None, Vec::new());
    }
    let origin = crate::im_rich::resolve_origin(process, raw);
    let parsed = crate::im_rich::parse_segments(raw, origin);
    let segments = if list_preview {
        crate::im_rich::limit_for_list_preview(&parsed)
    } else {
        parsed
    };
    (
        Some(origin.as_api_token().to_string()),
        segments.iter().map(crate::im_rich::segment_to_dto).collect(),
    )
}

const TITLE_PREVIEW_LENGTH: usize = 1000;
const SUMMARY_PREVIEW_LENGTH: usize = 1000;
const SEARCH_SNIPPET_LENGTH: usize = 200;
const SEARCH_SNIPPET_LEAD: usize = 40;
const SEARCH_SNIPPET_TAIL: usize = 140;

fn to_preview(value: &str, max_utf16: usize) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let units: Vec<u16> = trimmed.encode_utf16().collect();
    if units.len() <= max_utf16 {
        trimmed.to_string()
    } else {
        format!("{}...", String::from_utf16_lossy(&units[..max_utf16]))
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_slice(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = end.min(units.len());
    let start = start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

fn list_primary_label(item: &ClipRecord) -> String {
    if item.kind == ClipKind::ImageClipItem && item.title.as_deref().unwrap_or("").trim().is_empty() {
        return image_list_file_name(item.image_path.as_deref().unwrap_or(""));
    }
    visual_title_source(item)
}

fn visual_title_source(item: &ClipRecord) -> String {
    item.visual_title()
}

fn list_summary_source(item: &ClipRecord) -> String {
    if item.kind == ClipKind::ImageClipItem && item.title.as_deref().unwrap_or("").trim().is_empty() {
        return image_list_file_name(item.image_path.as_deref().unwrap_or(""));
    }
    item.csharp_summary()
}

/// C# `Path.GetFileName(fp.TrimEnd(DirectorySeparatorChar, AltDirectorySeparatorChar))`.
fn image_list_file_name(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches(['\\', '/']);
    std::path::Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

fn resolve_display_text(title: &str, summary: &str) -> String {
    if !title.trim().is_empty() && !title.eq_ignore_ascii_case("(No title)") {
        title.to_string()
    } else if !summary.trim().is_empty() {
        summary.to_string()
    } else {
        title.to_string()
    }
}

fn highlight_list_row(
    item: &ClipRecord,
    display_text: &str,
    query: Option<&str>,
) -> (String, Vec<HighlightHitRangeDto>) {
    let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) else {
        return (display_text.to_string(), Vec::new());
    };
    let from_display = build_highlight_list_fields(display_text, q);
    if !from_display.1.is_empty() {
        return from_display;
    }
    for candidate in enumerate_full_highlight_candidates(item) {
        if candidate.trim().is_empty() || candidate == display_text {
            continue;
        }
        let result = build_highlight_list_fields(&candidate, q);
        if !result.1.is_empty() {
            return result;
        }
    }
    (display_text.to_string(), Vec::new())
}

fn enumerate_full_highlight_candidates(item: &ClipRecord) -> Vec<String> {
    let mut out = vec![visual_title_source(item), item.csharp_summary()];
    if !item.description.trim().is_empty() {
        out.push(item.description.clone());
    }
    if item.kind == ClipKind::ActionClipItem {
        if let Ok(action) = crate::quicker_action::ActionReference::parse(item.body_text.as_deref().unwrap_or("")) {
            if let Some(title) = action.title { out.push(title); }
            if let Some(description) = action.description { out.push(description); }
        }
    }
    // C# `item.Item is TextClipItem` — LinkClipItem is not included.
    if csharp_text_clip_kind(item.kind) {
        if let Some(text) = item.body_text.as_ref().filter(|s| !s.is_empty()) {
            out.push(text.clone());
        }
    }
    out
}

fn collapse_whitespace_for_list_snippet(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

fn build_highlight_list_fields(text: &str, query: &str) -> (String, Vec<HighlightHitRangeDto>) {
    if text.trim().is_empty() {
        return (String::new(), Vec::new());
    }
    if query.trim().is_empty() {
        return (text.to_string(), Vec::new());
    }
    let q = query.trim();
    let compact = collapse_whitespace_for_list_snippet(text);
    let Some((anchor_start, anchor_end)) = list_highlight_anchor(&compact, q) else {
        return (compact, Vec::new());
    };
    let display = if utf16_len(&compact) <= SEARCH_SNIPPET_LENGTH {
        compact
    } else {
        create_snippet_around_union(&compact, anchor_start, anchor_end)
    };
    align_highlight_ranges_to_display(&display, q)
}

fn list_highlight_anchor(text: &str, query: &str) -> Option<(usize, usize)> {
    if let Some(ranges) = crate::clip_fuzzy::try_get_highlight_ranges(text, query) {
        let start = ranges.iter().map(|r| r.0).min().unwrap_or(0).max(0) as usize;
        let end = ranges
            .iter()
            .map(|r| r.1)
            .max()
            .unwrap_or(0)
            .max(start as i32 + 1) as usize;
        let end = end.min(utf16_len(text));
        return Some((start.min(utf16_len(text).saturating_sub(1)), end.max(start + 1)));
    }
    None
}

fn create_snippet_around_union(text: &str, union_start: usize, union_end: usize) -> String {
    let len = utf16_len(text);
    if len <= SEARCH_SNIPPET_LENGTH {
        return text.to_string();
    }
    let clamped_start = union_start.min(len);
    let clamped_end = union_end.clamp(clamped_start, len);
    let mut slice_start = clamped_start.saturating_sub(SEARCH_SNIPPET_LEAD);
    let mut slice_end = (clamped_end + SEARCH_SNIPPET_TAIL).min(len);
    if slice_end - slice_start > SEARCH_SNIPPET_LENGTH {
        slice_end = (slice_start + SEARCH_SNIPPET_LENGTH).min(len);
        if clamped_end > slice_end {
            slice_end = (clamped_end + SEARCH_SNIPPET_TAIL.min(24)).min(len);
            slice_start = slice_end.saturating_sub(SEARCH_SNIPPET_LENGTH);
        }
    }
    if slice_end <= slice_start {
        return text.to_string();
    }
    let slice = utf16_slice(text, slice_start, slice_end);
    let prefix = if slice_start > 0 { "..." } else { "" };
    let suffix = if slice_end < len { "..." } else { "" };
    format!("{prefix}{slice}{suffix}")
}

fn align_highlight_ranges_to_display(display: &str, query: &str) -> (String, Vec<HighlightHitRangeDto>) {
    if let Some(ranges) = crate::clip_fuzzy::try_get_highlight_ranges(display, query) {
        let mapped: Vec<HighlightHitRangeDto> = ranges
            .into_iter()
            .filter(|(s, e)| *s >= 0 && *e > *s && (*e as usize) <= utf16_len(display))
            .map(|(start, end)| HighlightHitRangeDto { start, end })
            .collect();
        if !mapped.is_empty() {
            return (display.to_string(), mapped);
        }
    }
    (display.to_string(), Vec::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavorItemDto {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub summary: String,
    pub kind: String,
    pub is_folder: bool,
    pub children_count: i32,
    pub create_time: DateTime<Utc>,
    pub update_time: DateTime<Utc>,
    pub source_process_name: String,
    pub file_count: i32,
    pub file_paths: Vec<String>,
    pub text_length: Option<i32>,
    pub im_rich_origin: Option<String>,
    pub im_rich_segments: Vec<serde_json::Value>,
}

/// C# `FavorItemDetailResponse.Kind` = `row.Item.GetType().Name`.
pub fn favor_kind_name(item: &FavorRecord) -> String {
    if item.is_folder {
        "FavoriteClipItem".into()
    } else {
        item.kind.as_str().into()
    }
}

/// C# `FavorItemResponse.Kind` = `item.Item.ObjType.ToString()`.
pub fn favor_list_kind(item: &FavorRecord) -> String {
    item.obj_type_name().to_string()
}

pub fn favor_to_dto(item: &FavorRecord) -> FavorItemDto {
    favor_to_list_dto(item, item.children.len() as i32)
}

pub fn favor_to_list_dto(item: &FavorRecord, children_count: i32) -> FavorItemDto {
    let file_paths = if item.kind == ClipKind::FileClipItem {
        keep_non_whitespace_paths(&item.file_paths)
    } else {
        Vec::new()
    };
    let text_like = matches!(
        item.kind,
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
    );
    let (im_rich_origin, im_rich_segments) = favor_list_im_meta(item);
    FavorItemDto {
        id: item.id.clone(),
        parent_id: item.parent_id.clone().unwrap_or_default(),
        title: item.name.clone(),
        summary: item.summary(),
        kind: favor_list_kind(item),
        is_folder: item.is_folder,
        children_count,
        create_time: item.create_time,
        update_time: item.update_time,
        source_process_name: item.source_process_name.clone(),
        file_count: file_paths.len() as i32,
        file_paths,
        text_length: if text_like {
            Some(
                item.body_text
                    .as_deref()
                    .unwrap_or("")
                    .encode_utf16()
                    .count() as i32,
            )
        } else {
            None
        },
        im_rich_origin,
        im_rich_segments,
    }
}

fn favor_list_im_meta(item: &FavorRecord) -> (Option<String>, Vec<Value>) {
    qq_im_meta(
        item.kind == ClipKind::QQRichTextItem,
        &item.source_process_name,
        item.qq_rich_text.as_deref(),
        true,
        true,
    )
}

pub fn favor_to_detail(item: &FavorRecord) -> serde_json::Value {
    let raw_json = serde_json::to_string(item).unwrap_or_default();
    let kind = favor_kind_name(item);
    let step = if item.kind == ClipKind::StepClipItem {
        item.body_text.as_deref().map(crate::quicker_steps::analyze)
    } else {
        None
    };
    let is_action = item.kind == ClipKind::ActionClipItem;
    let action = if is_action {
        crate::quicker_action::ActionReference::parse(item.body_text.as_deref().unwrap_or("")).ok()
    } else { None };
    let (im_rich_origin, im_rich_segments) = qq_im_meta(
        item.kind == ClipKind::QQRichTextItem,
        &item.source_process_name,
        item.qq_rich_text.as_deref(),
        false,
        false,
    );
    serde_json::json!({
        "id": item.id,
        "parentId": item.parent_id.clone().unwrap_or_default(),
        "title": item.name,
        "summary": item.summary(),
        "kind": kind,
        "isFolder": item.is_folder,
        "children": item.children,
        "createTime": item.create_time,
        "updateTime": item.update_time,
        "revision": record_edit_revision(&raw_json, item.storage_revision.as_deref()),
        "rawJson": raw_json,
        "bodyText": if csharp_text_clip_kind(item.kind) { item.body_text.clone() } else { None },
        "htmlData": if csharp_html_clip_kind(item.kind) { item.html_data.clone() } else { None },
        "htmlSourceUrl": if csharp_html_clip_kind(item.kind) {
            whitespace_to_none(item.html_source_url.as_deref())
        } else {
            None
        },
        "qqRichText": if item.kind == ClipKind::QQRichTextItem { item.qq_rich_text.clone() } else { None },
        "filePaths": if item.kind == ClipKind::FileClipItem {
            keep_non_whitespace_paths(&item.file_paths)
        } else {
            Vec::new()
        },
        "linkUrl": if item.kind == ClipKind::LinkClipItem { item.body_text.clone() } else { None },
        "linkTitle": if item.kind == ClipKind::LinkClipItem { Some(item.name.clone()) } else { None },
        "command": if item.kind == ClipKind::CmdClipItem { item.body_text.clone() } else { None },
        "stepData": if item.kind == ClipKind::StepClipItem { item.body_text.clone() } else { None },
        "stepPresentation": step,
        "actionId": action.as_ref().and_then(|value| value.require_id().ok()),
        "actionTitle": if is_action { action.as_ref().and_then(|value| value.title.clone()).or_else(|| Some(item.name.clone())) } else { None },
        "executeParam": if is_action { Some(item.execute_param.clone()) } else { None },
        "actionIcon": action.as_ref().and_then(|value| value.icon.as_deref()).filter(|icon| !icon.trim().is_empty()),
        "imRichOrigin": im_rich_origin,
        "imRichSegments": im_rich_segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    #[test]
    fn edit_revision_matches_csharp_sha256_format() {
        assert_eq!(edit_revision("abc"),
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD");
    }

    fn favor(
        kind: ClipKind,
        is_folder: bool,
        name: &str,
        body: Option<&str>,
        files: Vec<&str>,
    ) -> FavorRecord {
        let now = Utc::now();
        FavorRecord {
            storage_revision: None,
            id: "id".into(),
            parent_id: None,
            is_folder,
            name: name.into(),
            kind,
            body_text: body.map(|s| s.into()),
            file_paths: files.into_iter().map(|s| s.into()).collect(),
            image_path: None,
            children: Vec::new(),
            create_time: now,
            update_time: now + chrono::Duration::seconds(5),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        }
    }

    #[test]
    fn favor_list_and_detail_kinds_match_csharp() {
        let folder = favor(ClipKind::FavoriteClipItem, true, "工作", None, vec![]);
        let list = favor_to_dto(&folder);
        assert_eq!(list.kind, "Favorites");
        assert_eq!(list.summary, "【收藏夹】");
        let detail = favor_to_detail(&folder);
        assert_eq!(detail["kind"], "FavoriteClipItem");
        assert_eq!(detail["summary"], "【收藏夹】");
        assert_eq!(detail["updateTime"], serde_json::to_value(folder.update_time).unwrap());
        assert_ne!(detail["updateTime"], detail["createTime"]);

        let text = favor(ClipKind::TextClipItem, false, "n", Some("hello"), vec![]);
        assert_eq!(favor_to_dto(&text).kind, "Text");
        assert_eq!(favor_to_detail(&text)["kind"], "TextClipItem");
        assert_eq!(favor_to_dto(&text).text_length, Some(5));

        let file = favor(ClipKind::FileClipItem, false, "", Some(r"C:\a.txt"), vec![r"C:\a.txt", "  "]);
        let list = favor_to_dto(&file);
        assert_eq!(list.kind, "File");
        assert_eq!(list.file_paths, vec![r"C:\a.txt"]);
        assert_eq!(list.file_count, 1);
        assert!(list.text_length.is_none());
        assert_eq!(list.summary, format!("{},{}", r"C:\a.txt", "  "));
        let mut spaced = file.clone();
        spaced.file_paths = vec![" C:\\spaced.txt".into(), "  ".into()];
        assert_eq!(favor_to_dto(&spaced).file_paths, vec![" C:\\spaced.txt"]);
        let file_detail = favor_to_detail(&spaced);
        assert!(file_detail["bodyText"].is_null());
        assert!(file_detail["htmlData"].is_null());
        assert_eq!(file_detail["filePaths"], json!([" C:\\spaced.txt"]));

        let link = favor(ClipKind::LinkClipItem, false, "t", Some("https://a"), vec![]);
        let link_detail = favor_to_detail(&link);
        assert!(link_detail["bodyText"].is_null());
        assert_eq!(link_detail["linkUrl"], "https://a");
        assert_eq!(link_detail["linkTitle"], "t");

        let mut action = favor(ClipKind::ActionClipItem, false, "跑", Some("act-id"), vec![]);
        action.execute_param = "p1\np2".into();
        let detail = favor_to_detail(&action);
        assert_eq!(detail["kind"], "ActionClipItem");
        assert_eq!(detail["actionId"], "act-id");
        assert_eq!(detail["actionTitle"], "跑");
        assert_eq!(detail["executeParam"], "p1\np2");
        assert_eq!(detail["summary"], "动作：跑\r\n参数：p1\np2\r\n");
        assert!(detail["actionIcon"].is_null());
        assert_eq!(favor_to_dto(&action).kind, "QuickerAction");

        let cmd = favor(ClipKind::CmdClipItem, false, "echo", Some("echo hi"), vec![]);
        assert_eq!(favor_to_dto(&cmd).summary, "【命令】");
        assert_eq!(favor_to_detail(&cmd)["summary"], "【命令】");

        let step = favor(
            ClipKind::StepClipItem,
            false,
            "steps",
            Some(r#"{"Steps":[{"StepRunnerKey":"sys:assign","inputParams":{"summary":"x=1"}}],"Variables":[{"Key":"x"}]}"#),
            vec![],
        );
        let step_summary = favor_to_dto(&step).summary;
        assert!(step_summary.contains("赋值: x=1"), "{step_summary}");
        assert!(step_summary.contains("变量: x"), "{step_summary}");
    }

    #[test]
    fn clip_and_favor_list_im_preview_match_csharp() {
        let mut texts = String::from("<QQRichEditFormat>");
        for i in 0..13 {
            texts.push_str(&format!(
                r#"<EditElement type="0"><![CDATA[t{i}]]></EditElement>"#
            ));
        }
        for i in 0..6 {
            texts.push_str(&format!(
                r#"<EditElement type="3" filepath="C:\img{i}.png"></EditElement>"#
            ));
        }
        texts.push_str("</QQRichEditFormat>");

        let mut clip = ClipRecord::new_text("qq", "chrome");
        clip.kind = ClipKind::QQRichTextItem;
        clip.title = Some(String::new());
        clip.qq_rich_text = Some(texts.clone());
        let list = clip_to_dto(&clip, None);
        assert_eq!(list.im_rich_origin.as_deref(), Some("qq"));
        assert_eq!(list.im_rich_segments.len(), 12);
        assert_eq!(list.file_count, 4);
        assert_eq!(list.file_paths.len(), 4);
        assert_eq!(list.file_paths[0], r"C:\img0.png");
        assert_eq!(list.im_rich_segments[0]["kind"], "text");
        assert_eq!(list.im_rich_segments[0]["text"], "t0");
        assert!(list.im_rich_segments[0]["path"].is_null());

        let detail = clip_to_detail(&clip);
        assert_eq!(detail.title, "");
        assert_eq!(detail.im_rich_segments.len(), 19);
        assert_eq!(detail.file_count, 0);
        assert!(detail.file_paths.is_empty());

        let mut files = ClipRecord::new_text("files", "t");
        files.kind = ClipKind::FileClipItem;
        files.file_paths = (0..12).map(|i| format!(r"C:\f{i}.txt")).collect();
        files.file_paths.push("  ".into());
        let listed = clip_to_dto(&files, None);
        assert_eq!(listed.file_count, 12);
        assert_eq!(listed.file_paths.len(), 10);
        let queried = clip_to_dto(&files, Some("f11"));
        assert_eq!(queried.file_count, 1);
        assert_eq!(queried.file_paths, vec![r"C:\f11.txt"]);
        let detail_files = clip_to_detail(&files);
        assert_eq!(detail_files.file_count, 12);
        assert_eq!(detail_files.file_paths.len(), 12);

        let mut favor_qq = favor(ClipKind::QQRichTextItem, false, "q", Some("plain"), vec![]);
        favor_qq.qq_rich_text = Some("   ".into());
        let empty = favor_to_dto(&favor_qq);
        assert!(empty.im_rich_origin.is_none());
        assert!(empty.im_rich_segments.is_empty());
        favor_qq.qq_rich_text = Some(texts);
        let favor_list = favor_to_dto(&favor_qq);
        assert_eq!(favor_list.im_rich_segments.len(), 12);
        assert_eq!(favor_to_detail(&favor_qq)["imRichSegments"].as_array().unwrap().len(), 19);
    }

    #[test]
    fn clip_text_length_visual_title_and_html_url_match_csharp() {
        let empty = ClipRecord::new_text("", "t");
        assert_eq!(empty.text_length(), Some(0));
        assert_eq!(clip_to_dto(&empty, None).text_length, Some(0));

        let cjk = ClipRecord::new_text("你好👍", "t");
        assert_eq!(cjk.text_length(), Some(3));

        let mut link = ClipRecord::new_text("https://a", "t");
        link.kind = ClipKind::LinkClipItem;
        assert!(link.text_length().is_none());
        assert!(clip_to_dto(&link, None).text_length.is_none());
        let link_detail = clip_to_detail(&link);
        assert!(link_detail.body_text.is_none());
        assert!(link_detail.html_data.is_none());
        assert!(link_detail.qq_rich_text.is_none());

        let mut file = ClipRecord::new_text("not-exposed", "t");
        file.kind = ClipKind::FileClipItem;
        file.file_paths = vec![r"C:\a.txt".into()];
        file.html_data = Some("<b>x</b>".into());
        let file_detail = clip_to_detail(&file);
        assert!(file_detail.body_text.is_none());
        assert!(file_detail.html_data.is_none());
        assert_eq!(clip_to_dto(&file, None).image_size_bytes, None);
        file.file_paths = vec![" C:\\spaced.txt".into(), "  ".into()];
        assert_eq!(clip_to_detail(&file).file_paths, vec![" C:\\spaced.txt"]);
        assert_eq!(clip_to_dto(&file, None).file_paths, vec![" C:\\spaced.txt"]);

        let mut html = ClipRecord::new_text("hi", "t");
        html.kind = ClipKind::HtmlClipItem;
        html.html_source_url = Some("  ".into());
        assert_eq!(html.text_length(), Some(2));
        assert!(clip_to_detail(&html).html_source_url.is_none());
        html.html_source_url = Some("https://src".into());
        assert_eq!(
            clip_to_detail(&html).html_source_url.as_deref(),
            Some("https://src")
        );

        let mut cmd = ClipRecord::new_text("echo", "t");
        cmd.kind = ClipKind::CmdClipItem;
        cmd.title = None;
        assert_eq!(visual_title_source(&cmd), "【命令】");
        assert_eq!(clip_to_dto(&cmd, None).title, "【命令】");

        let mut action = ClipRecord::new_text("act-id", "t");
        action.kind = ClipKind::ActionClipItem;
        action.title = None;
        assert_eq!(visual_title_source(&action), "");
        assert_eq!(clip_to_dto(&action, None).title, "");
        assert_eq!(clip_to_dto(&action, None).summary, "动作：\r\n参数：");
        action.title = Some("跑".into());
        action.execute_param = "p1\np2".into();
        assert_eq!(visual_title_source(&action), "跑");
        assert_eq!(clip_to_dto(&action, None).title, "跑");
        assert_eq!(clip_to_dto(&action, None).summary, "动作：跑\r\n参数：p1\np2");
        assert_eq!(
            clip_to_detail(&action).summary,
            "动作：跑\r\n参数：p1\np2\r\n"
        );
        action.title = None;
        assert_eq!(visual_title_source(&action), ":p1");

        let mut with_desc = ClipRecord::new_text("visible-body", "t");
        with_desc.description = "desc-only-highlight-token".into();
        let dto = clip_to_dto(&with_desc, Some("desc-only-highlight-token"));
        assert!(
            dto.highlighted_text.contains("desc-only-highlight-token"),
            "C# EnumerateFullHighlightCandidates includes ClipItem.Description"
        );
        assert!(!dto.highlight_hit_ranges.is_empty());
    }

    #[test]
    fn clip_changed_delta_matches_csharp_build_clip_delta() {
        let rec = ClipRecord::new_text("delta-body", "chrome");
        let delta = clip_changed_delta(std::slice::from_ref(&rec), &[]);
        assert_eq!(delta["op"], "delta");
        assert_eq!(delta["items"][0]["id"], rec.id);
        assert_eq!(delta["items"][0]["kind"], "TextClipItem");
        assert!(delta["ids"].as_array().unwrap().is_empty());
        assert!(delta["ts"].as_i64().unwrap() > 0);
        assert_eq!(clip_changed_delta(&[], &[])["op"], "reload");
        let too_many: Vec<String> = (0..=CLIP_CHANGED_MAX_DELTA_ITEMS)
            .map(|i| i.to_string())
            .collect();
        assert_eq!(clip_changed_delta(&[], &too_many)["op"], "reload");
        let favor = favorites_changed_delta(&["a".into()], &["b".into()]);
        assert_eq!(favor["op"], "delta");
        assert_eq!(favor["upsertIds"][0], "a");
        assert_eq!(favor["ids"][0], "b");
    }
}
