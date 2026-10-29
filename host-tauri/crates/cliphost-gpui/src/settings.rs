//! Native settings. The core's schema-driven forms (`settings.form.get` /
//! `settings.form.update`) are rendered as GPUI rows; one `SettingsForm`
//! serves both the settings window and the clipboard list-settings popover.
//! Values are saved per field (the core merges partial values), so a field
//! edit never resubmits the rest of the page.

use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use cliphost_core::{
    host::HostState,
    i18n::{displayed_language, set_language, tr, LanguageMode},
};
use gpui::{
    div, point, prelude::*, px, relative, size, AnyElement, AnyWindowHandle, App, Bounds,
    Context, Entity, FocusHandle, FontWeight, KeyDownEvent, MouseButton, Pixels, ScrollHandle,
    SharedString, Subscription, Task, Timer, WeakEntity, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use crate::crisp_svg::svg;
use crate::smooth_wheel::{self, SmoothWheelExt};
use serde_json::{json, Map, Value};

#[path = "settings_search.rs"]
mod settings_search;
use settings_search::{
    DeferredSettingsSearch, SettingsSearchIndex, SettingsSearchMatch, SettingsSearchQuery,
};
#[path = "settings_visual.rs"]
mod settings_visual;
use settings_visual::SettingsGeometry;
pub(crate) use settings_visual::SettingsPalette;
#[path = "settings_widgets.rs"]
mod settings_widgets;
use settings_widgets as widgets;
#[path = "settings_quick_paste.rs"]
mod quick_paste_help;
#[path = "settings_device_sync.rs"]
mod device_sync;
use device_sync::DeviceSyncSettingsPage;
#[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
#[path = "settings_webdav.rs"]
mod webdav;
#[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
use webdav::WebDavSettingsPage;
#[path = "settings_home.rs"]
mod home;
use home::{HomeEvent, HomeSettingsPage};
#[path = "settings_update.rs"]
mod updates;
use updates::UpdateSettingsPage;
pub(crate) use updates::{authorize_update_exit, run_automatic_update_checks, update_exit_cancelled};
#[path = "settings_list_popover.rs"]
mod settings_list_popover;
#[path = "settings_color.rs"]
mod settings_color;
use settings_color::ArgbColor;
#[path = "settings_choice.rs"]
pub(crate) mod settings_choice;
use settings_choice::{
    SettingsChoiceDismissal, SettingsChoiceEvent, SettingsChoiceOption, SettingsChoicePopup,
};
#[path = "settings_color_picker_model.rs"]
mod settings_color_picker_model;
use settings_color_picker_model::{open_decision, ColorSessionLease, OpenDecision};
#[path = "settings_color_picker.rs"]
mod settings_color_picker;
use settings_color_picker::{
    checker, ColorDismissal, ColorPickerEvent, SettingsColorPopup,
};
#[path = "settings_slider.rs"]
mod settings_slider;
use settings_slider::{SettingsSlider, SliderDomain};
#[path = "settings_super_menu.rs"]
mod super_menu_sections;
use super_menu_sections::SuperMenuSections;

use crate::{
    filters::{FilterTabsEditor, FilterTabsEvent},
    input::{InputColors, SearchEvent, SearchInput},
    management::{LanManagementEvent, LanManagementPage, ManagementPage},
    motion::{self, lerp, mix_hsla, Tweens},
    preview::{DesignTokens, ThemeTokens},
    scrollbar::{ScrollbarState, ScrollbarStyle, ScrollbarTarget},
    text_actions::TextActionsPage,
    view::color,
    window_background::{PreparedBackground, WindowBackgroundOptions},
};

/// Scalar progress tracks of one control; each key is (element id, track).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Track {
    On,
    Thumb,
    Hover,
    Selected,
    Focus,
}

/// `.settings-switch` `background-color/border-color 0.15s ease`.
const SWITCH_COLOR: Duration = Duration::from_millis(150);
/// `.settings-switch-thumb` `transform 0.18s ease`.
const SWITCH_THUMB: Duration = Duration::from_millis(180);

/// The core keeps launch-at-startup and elevation in `homeGlobal`, which its
/// catalog leaves to the web home page; native global shows a system section.
const SYSTEM_PAGE: &str = "homeGlobal";
const DATA_PAGE: &str = "dataLocation";
const DEVICES_PAGE: &str = "devices";
const UPDATE_PAGE: &str = "about";
/// Unified main window pages hosted beside the settings categories.
const HOME_PAGE: &str = "home";
const LAN_PAGE: &str = "lan";
const HELP_PAGE: &str = "help";
/// Navigation entry for the retained text actions manager (`textAction`).
const TEXT_ACTIONS_PAGE: &str = "textActions";

/// Pages rendered by a dedicated native view instead of a schema form.
fn native_settings_page(id: &str) -> bool {
    matches!(id, DATA_PAGE | DEVICES_PAGE | UPDATE_PAGE | HOME_PAGE | HELP_PAGE | TEXT_ACTIONS_PAGE | "user")
}

/// Pages that fill the content area with their own toolbar and scrolling.
fn full_bleed_settings_page(id: &str) -> bool {
    matches!(id, HELP_PAGE | "user")
}

/// Sidebar groups of the unified main window: home, settings, data & support.
fn settings_nav_group(id: &str) -> usize {
    match id {
        HOME_PAGE => 0,
        DATA_PAGE | HELP_PAGE | UPDATE_PAGE => 3,
        _ => 2,
    }
}

fn settings_nav_group_label(group: usize) -> Option<&'static str> {
    match group {
        2 => Some(tr("设置", "Settings")),
        3 => Some(tr("数据与支持", "Data & support")),
        _ => None,
    }
}

/// Search metadata for main window pages, so one search finds tools and settings.
fn main_page_search_schema(id: &str) -> Option<Value> {
    let (label, fields) = match id {
        HOME_PAGE => (tr("首页", "Home"), json!([
            {"key":"quickOpen","label":tr("快捷打开", "Quick open"),"description":"剪贴板 超级面板 超级菜单 书签 文件 窗口 收藏 clipboard panel super menu bookmarks"},
            {"key":"windowBehavior","label":tr("窗口行为", "Window behavior"),"description":"托盘图标 关闭主界面 后台运行 缩小到托盘 退出程序 tray icon close behavior background"},
            {"key":"runtime","label":tr("程序管理", "App controls"),"description":"从 Quicker 打开剪贴板 查看日志 复制日志 重启 退出 运行状态 log restart exit status"}
        ])),
        DEVICES_PAGE => (tr("跨设备", "Cross-device"), json!([
            {"key":"pairing","label":tr("配对设备与连接", "Pair devices and connections"),"description":"局域网 互传 设备 配对码 连接 附近设备 手机 LAN transfer pair devices connections"},
            {"key":"sync","label":tr("剪贴板同步", "Clipboard sync"),"description":"设备同步 文本 图片 自动同步 sync text images automatic"}
        ])),
        TEXT_ACTIONS_PAGE => (tr("文本操作", "Text actions"), json!([
            {"key":"rules","label":tr("管理文本操作", "Manage text actions"),"description":"文本操作 规则 正则 替换 清理 格式 模板 text actions rules regex replace"}
        ])),
        HELP_PAGE => (tr("帮助", "Help"), json!([
            {"key":"docs","label":tr("使用说明与命令参考", "Usage and command reference"),"description":"帮助 文档 说明 命令 快捷键 关于 赞赏 help docs usage commands shortcuts"}
        ])),
        _ => return None,
    };
    Some(json!({"configLabel":label,"fields":fields}))
}

/// Stable field identities, independent of localized schema group labels.
fn super_menu_field_section(key: &str) -> (usize, &'static str, &'static str) {
    match key {
        "ShowSettingItem" | "ShowAdderMenuItem" | "ShowBookMarksMenu" =>
            (1, "items", tr("显示条目", "Visible items")),
        "ShowWindowMenuItem" | "ExpandWindowMenu" | "Grouping" | "ShowWindowThumbnail" =>
            (2, "windows", tr("窗口菜单", "Window menu")),
        _ => (0, "general", tr("常规", "General")),
    }
}

fn super_menu_route_section(config_id: &str) -> Option<&'static str> {
    match config_id {
        "menu_global" => Some("general"),
        "menu_file" => Some("files"),
        "textAction" | TEXT_ACTIONS_PAGE => Some("text-actions"),
        _ => None,
    }
}

fn settings_navigation_id(config_id: &str) -> &str {
    if matches!(config_id, SYSTEM_PAGE | "system") {
        "global"
    } else if matches!(config_id, LAN_PAGE | "deviceSync") {
        DEVICES_PAGE
    } else if matches!(config_id, "menu_file" | "textAction" | TEXT_ACTIONS_PAGE) {
        "menu_global"
    } else {
        config_id
    }
}

/// Text actions retain searchable/deep-link metadata, but are a child of Super menu.
fn sidebar_settings_page(id: &str) -> bool {
    id != TEXT_ACTIONS_PAGE && settings_navigation_id(id) == id
}

fn settings_scope_includes(scope: Option<&str>, config_id: &str) -> bool {
    scope.is_none_or(|scope| settings_navigation_id(scope) == settings_navigation_id(config_id))
}

fn settings_search_label(config_id: &str, label: &str) -> String {
    if config_id == "menu_file" {
        format!("{} · {label}", tr("超级菜单", "Super menu"))
    } else {
        label.to_string()
    }
}

fn index_settings_schema(index: &mut SettingsSearchIndex, id: &str, label: &str, schema: &Value) {
    if id == "menu_file" {
        // The index prefers configLabel over its fallback label. Decorate the
        // metadata itself, while the persisted schema/config ID stays intact.
        let mut metadata = schema.clone();
        metadata["configLabel"] = json!(settings_search_label(id, schema["configLabel"].as_str().unwrap_or(label)));
        index.upsert(id, label, &metadata);
    } else {
        index.upsert(id, label, schema);
    }
}

fn settings_navigation_match_count<'a>(
    nav_id: &str,
    matches: impl IntoIterator<Item = (&'a str, usize)>,
) -> usize {
    matches
        .into_iter()
        .filter(|(id, _)| settings_navigation_id(id) == nav_id)
        .map(|(_, count)| count)
        .sum()
}

/// The unified main window hosts settings, device pairing and help at the
/// confirmed 900 x 640 design size.
fn settings_default_size(height: f32, min_width: f32, min_height: f32) -> (f32, f32) {
    (900.0_f32.max(min_width), height.max(640.0).max(min_height))
}

fn full_width_settings_choice(config_id: &str, key: &str) -> bool {
    config_id == "global" && key == "UiThemeMode"
}

fn ordinary_settings_choice(config_id: &str, key: &str) -> bool {
    !full_width_settings_choice(config_id, key) && !(config_id == "panel" && key == "PanelStyle")
}

fn settings_window_tokens(
    base: &DesignTokens,
    options: &WindowBackgroundOptions,
    has_snapshot: bool,
) -> DesignTokens {
    if has_snapshot {
        crate::window_glass::tokens(base, options)
    } else {
        base.clone()
    }
}

fn settings_nav_icon(id: &str) -> &'static str {
    match id {
        "trigger" => "filter",
        "quickPaste" => "paste",
        "clip" => "clipboard",
        "panel" => "star",
        "user" => "all",
        "menu_global" => "list",
        "menu_file" => "file",
        DATA_PAGE => "folder-open",
        DEVICES_PAGE => "laptop",
        UPDATE_PAGE => "refresh",
        HOME_PAGE => "layout-sidebar",
        TEXT_ACTIONS_PAGE => "text",
        HELP_PAGE => "file",
        "textAction" => "edit",
        SYSTEM_PAGE => "code",
        _ => "settings",
    }
}

/// Data uses its schema only as search metadata; its retained management
/// editor renders the page and owns explicit confirmation/save semantics.
fn searchable_settings_page(id: &str) -> bool {
    !matches!(id, "textAction" | "mobileSync")
}

fn settings_schema_pages(
    pages: &[(String, String)],
    active: Option<&str>,
) -> Vec<(String, String)> {
    pages
        .iter()
        .filter(|(id, _)| {
            searchable_settings_page(id)
                || (id == "textAction" && active.is_some_and(|id| settings_navigation_id(id) == "menu_global"))
        })
        .cloned()
        .collect()
}

/// Language refreshes only replace presentation metadata. Values, focused
/// controls and outstanding save generations belong to the live form.
fn merge_localized_metadata(fields: &mut [Value], localized: &[Value]) {
    for field in fields {
        let Some(source) = localized
            .iter()
            .find(|source| source["key"] == field["key"])
        else {
            continue;
        };
        for property in ["label", "description", "group", "selectionItems"] {
            if let Some(value) = source.get(property) {
                field[property] = value.clone();
            }
        }
    }
}

fn settings_pages(host: &HostState) -> Vec<(String, String)> {
    let mut pages: Vec<_> = host.settings_catalog()["configs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|config| {
            Some((
                config["id"].as_str()?.to_string(),
                config["label"].as_str()?.to_string(),
            ))
        })
        .collect();
    let after_global = pages
        .iter()
        .position(|(id, _)| id == "global")
        .map_or(0, |i| i + 1);
    pages.insert(
        after_global,
        (SYSTEM_PAGE.into(), tr("系统", "System").into()),
    );
    let after_clip = pages.iter().position(|(id, _)| id == "clip").map_or(pages.len(), |i| i + 1);
    pages.insert(after_clip, (DEVICES_PAGE.into(), tr("跨设备", "Cross-device").into()));
    pages.push((HELP_PAGE.into(), tr("帮助", "Help").into()));
    pages.push((UPDATE_PAGE.into(), tr("关于与更新", "About & updates").into()));
    pages.splice(0..0, [
        (HOME_PAGE.into(), tr("首页", "Home").into()),
        (TEXT_ACTIONS_PAGE.into(), tr("文本操作", "Text actions").into()),
    ]);
    // Stable: catalog order is kept inside each sidebar group.
    pages.sort_by_key(|(id, _)| settings_nav_group(id));
    pages
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Switch,
    Choice,
    Number,
    Slider,
    Text,
    Hotkey,
    Folder,
    ImageFile,
    Color,
    Lines,
    /// Category tags (`ClipFilterConfig`) — the shared tag editor.
    Tags,
    /// Edited elsewhere (category tags) or not representable as one line.
    External,
}

enum FormMessage {
    Raw(bool, String),
    Schema(bool, String),
    TagsSaved,
    SaveFailed(String),
}

fn schema_message(form: &Value) -> Option<FormMessage> {
    let message = form["messages"]
        .as_array()?
        .iter()
        .find(|message| message["text"].as_str().is_some())?;
    Some(FormMessage::Schema(
        matches!(message["level"].as_str(), Some("error" | "warning")),
        message["text"].as_str()?.to_string(),
    ))
}

fn refresh_schema_message(message: &mut Option<FormMessage>, form: &Value) {
    // GET forms usually carry no messages. Update responses can carry transient
    // operation outcomes (hotkey warnings, data-location results), which must
    // stay visible until the next user operation replaces them.
    if matches!(message.as_ref(), Some(FormMessage::Schema(_, _))) {
        if let Some(refreshed) = schema_message(form) {
            *message = Some(refreshed);
        }
    }
}

fn kind(field: &Value) -> Kind {
    let editor = field["formEditor"].as_str().unwrap_or("default");
    match (field["inputType"].as_str().unwrap_or(""), editor) {
        (_, "hotkey") => Kind::Hotkey,
        (_, "fileMenuPaths") => Kind::Lines,
        (_, "filterItems") => Kind::Tags,
        (_, "imagefile") => Kind::ImageFile,
        (_, "color") => Kind::Color,
        ("number", "slider") => Kind::Slider,
        ("boolean", _) => Kind::Switch,
        ("enum", _) => Kind::Choice,
        ("number", _) => Kind::Number,
        ("folder", _) => Kind::Folder,
        // A single-line input would fold newlines into spaces on save.
        ("text", _) if field["value"].as_str().is_some_and(|v| v.contains('\n')) => Kind::External,
        ("text", _) => Kind::Text,
        _ => Kind::External,
    }
}

fn text_value(field: &Value) -> String {
    match &field["value"] {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number
            .as_f64()
            .filter(|n| n.fract() == 0.0)
            .map(|n| format!("{n:.0}"))
            .unwrap_or_else(|| number.to_string()),
        other => other.to_string(),
    }
}

fn number_step(field: &Value) -> f64 {
    if let Some(step) = field["step"].as_f64().filter(|step| *step > 0.0) {
        return step;
    }
    let value = field["value"].as_f64().unwrap_or(0.0);
    let range = field["max"]
        .as_f64()
        .zip(field["min"].as_f64())
        .map(|(max, min)| max - min);
    if range.is_some_and(|range| range >= 200.0) || range.is_none() && value >= 100.0 {
        10.0
    } else {
        1.0
    }
}

fn clamp_number(field: &Value, mut number: f64) -> f64 {
    if let Some(min) = field["min"].as_f64() {
        number = number.max(min);
    }
    if let Some(max) = field["max"].as_f64() {
        number = number.min(max);
    }
    number
}

fn number_json(field: &Value, number: f64) -> Value {
    let number = clamp_number(field, number);
    if field["value"].is_i64() || field["value"].is_u64() {
        json!(number.round() as i64)
    } else {
        json!(number)
    }
}

fn stepped_number(field: &Value, draft: &str, direction: f64) -> Option<Value> {
    let current = draft
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())?;
    // NumberField first clamps the live draft, then applies the step. In
    // particular a draft above max must respond immediately to decrement.
    let current = clamp_number(field, current);
    let next = current + direction * number_step(field);
    next.is_finite().then(|| number_json(field, next))
}

fn slider_domain(field: &Value) -> Option<SliderDomain> {
    let min = field["min"].as_f64().filter(|n| n.is_finite()).unwrap_or(0.0);
    let max = field["max"].as_f64().filter(|n| n.is_finite()).unwrap_or(100.0);
    let step = field["step"].as_f64().filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(1.0);
    SliderDomain::new(min, max, step)
}

fn settings_tab_direction(event: &KeyDownEvent) -> Option<bool> {
    let modifiers = &event.keystroke.modifiers;
    (event.keystroke.key.eq_ignore_ascii_case("tab")
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function)
        .then_some(modifiers.shift)
}

fn next_settings_focus(current: Option<usize>, count: usize, reverse: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match current {
        Some(index) if reverse => (index + count - 1) % count,
        Some(index) => (index + 1) % count,
        None if reverse => count - 1,
        None => 0,
    })
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// Stored chords use WPF key names (`Ctrl+Oem3`), shared with C# and the web editor.
fn wpf_key(key: &str) -> Option<String> {
    let key = key.to_ascii_lowercase();
    let named = match key.as_str() {
        "`" => "Oem3",
        "-" => "OemMinus",
        "=" => "OemPlus",
        "[" => "OemOpenBrackets",
        "]" => "OemCloseBrackets",
        "\\" => "OemPipe",
        ";" => "OemSemicolon",
        "'" => "OemQuotes",
        "," => "OemComma",
        "." => "OemPeriod",
        "/" => "OemQuestion",
        "space" => "Space",
        "enter" => "Enter",
        "tab" => "Tab",
        "backspace" => "Back",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.into());
    }
    let bytes = key.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        return Some(key.to_ascii_uppercase());
    }
    if bytes.len() == 1 && bytes[0].is_ascii_digit() {
        return Some(format!("D{key}"));
    }
    if key.len() > 1
        && key.starts_with('f')
        && key[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n))
    {
        return Some(key.to_ascii_uppercase());
    }
    None
}

/// `Some(true)` for an unmodified PageDown, `Some(false)` for PageUp.
fn settings_page_direction(event: &KeyDownEvent) -> Option<bool> {
    if event.keystroke.modifiers.modified() {
        return None;
    }
    match event.keystroke.key.as_str() {
        "pagedown" => Some(true),
        "pageup" => Some(false),
        _ => None,
    }
}

fn hotkey_chord(key: &str, modifiers: &gpui::Modifiers) -> Option<String> {
    let key = wpf_key(key)?;
    let mut parts = Vec::new();
    for (on, name) in [
        (modifiers.control, "Ctrl"),
        (modifiers.alt, "Alt"),
        (modifiers.shift, "Shift"),
        (modifiers.platform, "Win"),
    ] {
        if on {
            parts.push(name.to_string());
        }
    }
    parts.push(key);
    Some(parts.join("+"))
}

fn hotkey_display(chord: &str) -> String {
    chord
        .split('+')
        .map(|part| {
            let part = part.trim();
            match part {
                "Oem3" => "`",
                "OemMinus" => "-",
                "OemPlus" => "=",
                "OemOpenBrackets" => "[",
                "OemCloseBrackets" => "]",
                "OemPipe" => "\\",
                "OemSemicolon" => ";",
                "OemQuotes" => "'",
                "OemComma" => ",",
                "OemPeriod" => ".",
                "OemQuestion" => "/",
                _ if part.len() == 2 && part.starts_with('D') => &part[1..],
                _ => part,
            }
            .to_string()
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// `/main/settings/clip` → `clip`; other main routes are not settings pages.
pub fn settings_page_from_route(route: &str) -> Option<String> {
    let path = route
        .split(['?', '#'])
        .next()
        .unwrap_or(route)
        .trim_end_matches('/');
    // The former management window's routes open the unified main window.
    match path {
        "/main" | "/main/home" => return Some(HOME_PAGE.into()),
        path if path == "/main/sync" || path.starts_with("/main/sync/") => return Some(HOME_PAGE.into()),
        path if path == "/docs" || path.starts_with("/main/docs") => return Some(HELP_PAGE.into()),
        _ => {}
    }
    let rest = path
        .strip_prefix("/main/settings")
        .or_else(|| path.strip_prefix("/settings"))?;
    let page = rest.trim_start_matches('/');
    if page.is_empty() {
        return Some("global".into());
    }
    let page = page.split('/').next().unwrap_or(page);
    // The navigation entry opens the retained manager of the `textAction` form.
    if page.eq_ignore_ascii_case(TEXT_ACTIONS_PAGE) {
        return Some("textAction".into());
    }
    if [DEVICES_PAGE, LAN_PAGE, "deviceSync"].into_iter().any(|known| page.eq_ignore_ascii_case(known)) {
        return Some(DEVICES_PAGE.into());
    }
    if let Some(page) = [UPDATE_PAGE, HOME_PAGE, HELP_PAGE]
        .into_iter().find(|known| page.eq_ignore_ascii_case(known)) {
        return Some(page.into());
    }
    let page = if page.eq_ignore_ascii_case("system") {
        SYSTEM_PAGE
    } else {
        page
    };
    cliphost_core::settings_form::canonical_form_config_id(page).ok()
}

pub struct SettingsForm {
    host: Arc<HostState>,
    tokens: DesignTokens,
    surface_palettes: Option<(SettingsPalette, SettingsPalette)>,
    dark: bool,
    config_id: String,
    only: Option<Vec<&'static str>>,
    compact: bool,
    /// The footer popover has a dedicated compact layout and page projection.
    list_settings_popover: bool,
    fields: Vec<Value>,
    /// Empty fields can be a fully initialized dedicated page. A placeholder
    /// awaiting its first schema must not publish an empty search index.
    schema_initialized: bool,
    /// A persisted config event requests fresh values independently of theme.
    /// Protected fields stay pending without replacing their accepted baseline.
    authoritative_refresh_all: bool,
    authoritative_refresh_keys: HashSet<String>,
    undo_baseline: SettingsUndoBaseline,
    tag_undo_needs_reload: bool,
    tag_undo_owner: u64,
    tag_undo_running: bool,
    tag_undo_recovery: Option<u64>,
    tag_undo_error: Option<String>,
    /// Projection only: hidden fields retain their controls, drafts and writes.
    search_keys: Option<HashSet<String>>,
    super_menu_sections: Option<SuperMenuSections>,
    inputs: HashMap<String, Entity<SearchInput>>,
    sliders: HashMap<String, Entity<SettingsSlider>>,
    slider_search_input: Option<WeakEntity<SearchInput>>,
    color_focus: HashMap<String, FocusHandle>,
    choice_focus: HashMap<String, FocusHandle>,
    choice_anchors: HashMap<String, Arc<Mutex<Option<Bounds<Pixels>>>>>,
    choice_popup: Option<SettingsChoiceSession>,
    /// Mutually exclusive with `choice_popup`. Every site that clears one
    /// clears the other, and none of them commits the popup's draft.
    color_popup: Option<SettingsColorSession>,
    color_anchors: HashMap<String, Arc<Mutex<Option<Bounds<Pixels>>>>>,
    standalone_window: bool,
    lines: HashMap<String, Vec<Entity<SearchInput>>>,
    line_dragging: bool,
    line_drop_target: Option<(String, u64, bool)>,
    capture: Option<String>,
    capture_focus: FocusHandle,
    tag_editor: Option<(Entity<FilterTabsEditor>, Subscription)>,
    text_actions: Option<Entity<TextActionsPage>>,
    message: Option<FormMessage>,
    generation: u64,
    saves: Arc<Mutex<SaveQueue>>,
    failed_saves: HashSet<String>,
    language: LanguageMode,
    _language_watch: Task<()>,
    input_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
    leave_locked: bool,
    leave_epoch: u64,
    pending_pickers: usize,
    folder_pickers: FolderPickerRequests,
    /// Display-only state transitions (hover/focus/selection/switch).
    /// Interior mutability: controls are built from `&self` render helpers.
    motion: RefCell<Tweens<(SharedString, Track)>>,
    hovered: HashSet<SharedString>,
}

struct SettingsChoiceSession {
    key: String,
    popup: Entity<SettingsChoicePopup>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct SettingsLineDrag {
    owner: u64,
    config_id: String,
    generation: u64,
    field: String,
    input: u64,
    title: String,
    background: gpui::Hsla,
    foreground: gpui::Hsla,
}

impl SettingsLineDrag {
    fn belongs_to(&self, owner: u64, config: &str, generation: u64, field: &str) -> bool {
        self.owner == owner && self.config_id == config && self.generation == generation && self.field == field
    }
}

impl Render for SettingsLineDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rsx! {
            <div px={px(10.0)} py={px(6.0)} rounded={px(5.0)} shadow_md bg={self.background}
                text_color={self.foreground} text_size={px(12.0)}>
                {self.title.clone()}
            </div>
        }
    }
}

fn line_reorder_destination(ids: &[u64], source: u64, target: u64, before: bool) -> Option<(usize, usize)> {
    let from = ids.iter().position(|id| *id == source)?;
    let target = ids.iter().position(|id| *id == target)?;
    let mut destination = target + usize::from(!before);
    if from < destination { destination -= 1; }
    (from != destination).then_some((from, destination))
}

struct SettingsColorSession {
    key: String,
    popup: Entity<SettingsColorPopup>,
    _subscription: Subscription,
}

struct SettingsTabRequest {
    reverse: bool,
}

impl gpui::EventEmitter<SettingsTabRequest> for SettingsForm {}

thread_local! {
    // Embedded panel settings consist of two sibling forms. Their parents
    // relinquish keyboard handling while settings are open, so they share a
    // weak, window-local tab sequence without extending those parent APIs.
    static EMBEDDED_SETTINGS_FORMS: RefCell<HashMap<AnyWindowHandle, Vec<WeakEntity<SettingsForm>>>> =
        RefCell::new(HashMap::new());
}

#[derive(Clone)]
struct FolderPickerLease(Arc<AtomicBool>);

impl FolderPickerLease {
    fn current(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    fn retire(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// UI acceptance reserves the field before the observer/provider is polled.
/// Retired leases keep their slot until completion: an already open modal must
/// finish before this field can launch another, even after a form replacement.
#[derive(Default)]
struct FolderPickerRequests {
    pending: HashMap<String, FolderPickerLease>,
}

impl Drop for SettingsForm {
    fn drop(&mut self) {
        self.folder_pickers.retire_all();
    }
}

impl FolderPickerRequests {
    fn is_pending(&self, key: &str) -> bool {
        self.pending.contains_key(key)
    }

    fn browse_enabled(&self, key: &str, leave_locked: bool) -> bool {
        !leave_locked && !self.is_pending(key)
    }

    fn accept(&mut self, key: &str, composing: bool) -> Option<FolderPickerLease> {
        if composing || self.is_pending(key) {
            return None;
        }
        let lease = FolderPickerLease(Arc::new(AtomicBool::new(true)));
        self.pending.insert(key.to_string(), lease.clone());
        Some(lease)
    }

    fn retire_key(&self, key: &str) {
        if let Some(lease) = self.pending.get(key) {
            lease.retire();
        }
    }

    fn retire_all(&self) {
        for lease in self.pending.values() {
            lease.retire();
        }
    }

    /// None means an old completion no longer owns this field's pending slot.
    fn finish(&mut self, key: &str, lease: &FolderPickerLease) -> Option<bool> {
        if !self.owns(key, lease) {
            return None;
        }
        let current = lease.current();
        self.pending.remove(key);
        lease.retire();
        Some(current)
    }

    fn owns(&self, key: &str, lease: &FolderPickerLease) -> bool {
        self.pending
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(&current.0, &lease.0))
    }
}

impl Drop for FolderPickerRequests {
    fn drop(&mut self) {
        self.retire_all();
    }
}

fn run_folder_picker(
    lease: &FolderPickerLease,
    provider: impl FnOnce() -> Result<Value, String>,
) -> Option<Result<Value, String>> {
    lease.current().then(provider)
}

fn selected_picker_path(result: &Value) -> Option<&str> {
    if result["cancelled"].as_bool() == Some(true) {
        return None;
    }
    result["path"]
        .as_str()
        .filter(|path| !path.trim().is_empty())
}

fn retire_system_route(route: &mut Option<String>) {
    if route
        .as_deref()
        .and_then(settings_page_from_route)
        .is_some_and(|page| page == SYSTEM_PAGE)
    {
        *route = None;
    }
}

fn can_normalize_input(draft: &str, accepted: &str, composing: bool, focused: bool) -> bool {
    !composing && !focused && draft == accepted
}

fn raw_lines_match_accepted(draft: &[&str], accepted: &str) -> bool {
    draft.iter().copied().eq(accepted.lines())
}

/// Only acknowledged, idle keys may move their value baseline. Missing keys
/// remain pending; a whole-page response never replaces protected values.
fn authoritative_field_updates<'a>(
    current: &[Value],
    authoritative: &'a [Value],
    ready: &HashSet<String>,
) -> Vec<(usize, String, &'a Value)> {
    current
        .iter()
        .enumerate()
        .filter_map(|(index, field)| {
            let key = field["key"].as_str()?;
            if !ready.contains(key) {
                return None;
            }
            let source = authoritative.iter().find(|source| source["key"] == key)?;
            source.get("value")?;
            Some((index, key.to_string(), source))
        })
        .collect()
}

#[derive(Default)]
struct SettingsUndoBaseline {
    values: HashMap<String, Value>,
}

fn undo_write_required(saved: &Value, baseline: &Value, failed: bool) -> bool {
    failed || !same_value(saved, baseline)
}

fn order_undo_values(
    config_id: &str,
    changes: &mut Vec<(String, Value, bool)>,
    baseline: &SettingsUndoBaseline,
) {
    if config_id != "global"
        || !changes.iter().any(|(key, _, write)| {
            *write
                && matches!(
                    key.as_str(),
                    "WindowBackgroundColor" | "FontForegroundColor"
                )
        })
    {
        return;
    }
    // Core color writes force Custom. Restore the entered theme LAST, even
    // when it was already equal before the color compensation was accepted.
    if let Some((_, _, write)) = changes.iter_mut().find(|(key, _, _)| key == "UiThemeMode") {
        *write = true;
    } else if let Some(value) = baseline.values.get("UiThemeMode") {
        changes.push(("UiThemeMode".into(), value.clone(), true));
    }
    changes.sort_by_key(|(key, _, _)| key == "UiThemeMode");
}

impl SettingsUndoBaseline {
    fn reset(&mut self, fields: &[Value]) {
        self.values = fields
            .iter()
            .filter_map(|field| {
                Some((
                    field["key"].as_str()?.to_string(),
                    field.get("value")?.clone(),
                ))
            })
            .collect();
    }

    /// A clean field updated elsewhere is not a local change to undo. Local
    /// optimistic/saved edits differ from this baseline and retain it.
    fn observe_authoritative(&mut self, key: &str, current: &Value, next: &Value, clean: bool) {
        if clean
            && self
                .values
                .get(key)
                .is_some_and(|baseline| same_value(baseline, current))
        {
            self.values.insert(key.to_string(), next.clone());
        }
    }
}

fn inline_settings_description(config_id: &str, key: &str, compact: bool) -> bool {
    // 普通说明统一收进帮助；此项按产品约定直接说明富文本输出的影响范围。
    !compact && config_id == "clip" && key == "PlainTextPasteByDefault"
}

fn schema_reply_current(reply: u64, current: u64, same_language: bool, closed: bool) -> bool {
    reply == current && same_language && !closed
}

fn search_leave_preflight(composing: bool) -> Result<(), String> {
    if composing {
        Err(tr(
            "请先完成搜索框的输入法组合，再离开设置。",
            "Finish composing in the search box before leaving settings.",
        )
        .into())
    } else {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SchemaApplication {
    Initialize,
    Metadata,
    Defer,
}

fn schema_application(initialized: bool, leave_locked: bool) -> SchemaApplication {
    if initialized {
        SchemaApplication::Metadata
    } else if leave_locked {
        SchemaApplication::Defer
    } else {
        SchemaApplication::Initialize
    }
}

fn index_initialized_form(
    index: &mut SettingsSearchIndex,
    id: &str,
    label: &str,
    initialized: bool,
    fields: &[Value],
) {
    if !searchable_settings_page(id) {
        index.remove(id);
        return;
    }
    if initialized {
        index_settings_schema(index, id, label, &json!({ "fields": fields }));
    }
}

struct SettingsHelp {
    text: String,
    theme: ThemeTokens,
}
impl Render for SettingsHelp {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rsx! {
            <div max_w={px(340.0)} p={px(10.0)} rounded={px(6.0)} border_1
                border_color={color(&self.theme.border)} bg={color(&self.theme.surface)}
                text_size={px(12.0)} line_height={relative(1.45)}
                text_color={color(&self.theme.text)}>
                {self.text.clone()}
            </div>
        }
    }
}
pub(crate) fn settings_help(
    text: String,
    theme: ThemeTokens,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    move |_, cx| {
        cx.new(|_| SettingsHelp {
            text: text.clone(),
            theme: theme.clone(),
        })
        .into()
    }
}

#[derive(Default)]
struct SaveQueue {
    pending: VecDeque<FieldSave>,
    running: bool,
    errors: HashMap<(String, String), String>,
    waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

pub type SettingsCloseResult = tokio::sync::oneshot::Receiver<Result<(), String>>;

struct SettingsLeaveParticipant {
    form: Entity<SettingsForm>,
    generation: u64,
    epoch: u64,
}

/// Owns the locked forms until their accepted writes settle. Await on GPUI's
/// foreground executor, then call finish in the parent's UI update. If the
/// parent cancels before awaiting, call cancel instead of dropping this ticket.
pub struct SettingsFormsLeave {
    participants: Vec<SettingsLeaveParticipant>,
    barriers: Vec<SettingsCloseResult>,
}

pub struct SettingsFormsLeaveResult {
    participants: Vec<SettingsLeaveParticipant>,
    results: Vec<Result<(), String>>,
}

fn same_leave(
    generation: u64,
    epoch: u64,
    current_generation: u64,
    current_epoch: u64,
    locked: bool,
) -> bool {
    locked && generation == current_generation && epoch == current_epoch
}

impl SettingsFormsLeave {
    pub async fn wait(self) -> SettingsFormsLeaveResult {
        let mut results = Vec::with_capacity(self.barriers.len());
        for barrier in self.barriers {
            results.push(
                barrier
                    .await
                    .unwrap_or_else(|_| Err("Settings write worker is unavailable.".into())),
            );
        }
        SettingsFormsLeaveResult {
            participants: self.participants,
            results,
        }
    }

    /// Cancel disposal; accepted writes still run, and controls become editable.
    pub fn cancel(self, cx: &mut App) {
        for participant in self.participants {
            participant.form.update(cx, |form, cx| {
                if same_leave(
                    participant.generation,
                    participant.epoch,
                    form.generation,
                    form.leave_epoch,
                    form.leave_locked,
                ) {
                    form.set_leave_locked(false, cx);
                }
            });
        }
    }
}

impl SettingsFormsLeaveResult {
    /// Returns Ok only if every original form may now be disposed. On Err keep
    /// all forms mounted: failed ones show feedback and all current controls unlock.
    /// A late result never unlocks a newer leave or a newly loaded form.
    pub fn finish(self, cx: &mut App) -> Result<(), String> {
        let mut errors = Vec::new();
        for (participant, result) in self.participants.into_iter().zip(self.results) {
            let result = participant.form.update(cx, |form, cx| {
                if !same_leave(
                    participant.generation,
                    participant.epoch,
                    form.generation,
                    form.leave_epoch,
                    form.leave_locked,
                ) {
                    return Err("Settings leave request is no longer current.".into());
                }
                let result = result.and_then(|()| {
                    if form.pending_writes() {
                        Err(
                            "Settings writes changed after the leave request. Keep the form open."
                                .into(),
                        )
                    } else {
                        form.preflight_inputs(cx)?;
                        form.persistence_error().map_or(Ok(()), Err)
                    }
                });
                if let Err(error) = &result {
                    form.message = Some(FormMessage::SaveFailed(error.clone()));
                }
                form.set_leave_locked(false, cx);
                result
            });
            if let Err(error) = result {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }
}

static SETTINGS_WRITE_QUEUES: OnceLock<Mutex<Vec<Arc<Mutex<SaveQueue>>>>> = OnceLock::new();

fn queue_error(queue: &SaveQueue) -> Option<String> {
    (!queue.errors.is_empty()).then(|| {
        queue
            .errors
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn finish_queue(
    queue: &mut SaveQueue,
) -> (
    Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
    Result<(), String>,
) {
    queue.running = false;
    (
        std::mem::take(&mut queue.waiters),
        queue_error(queue).map_or(Ok(()), Err),
    )
}

fn queue_barrier(queue: &Arc<Mutex<SaveQueue>>) -> SettingsCloseResult {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    match queue.lock() {
        Ok(mut state) if state.running || !state.pending.is_empty() => state.waiters.push(sender),
        Ok(state) => {
            let _ = sender.send(queue_error(&state).map_or(Ok(()), Err));
        }
        Err(_) => {
            let _ = sender.send(Err("Settings write queue is unavailable.".into()));
        }
    }
    receiver
}

/// Await after saving/closing live settings surfaces and before App::quit.
/// Accepted writes remain registered even if their GPUI owner was destroyed.
pub async fn flush_settings_writes() -> Result<(), String> {
    let queues = SETTINGS_WRITE_QUEUES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "Settings write queue registry is unavailable.".to_string())?
        .clone();
    let mut errors = Vec::new();
    for queue in &queues {
        match queue_barrier(queue).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(_) => errors.push("Settings write worker is unavailable.".into()),
        }
    }
    if errors.is_empty() {
        if let Ok(mut registered) = SETTINGS_WRITE_QUEUES.get_or_init(Default::default).lock() {
            registered.retain(|queue| Arc::strong_count(queue) > 2);
        }
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

pub(crate) fn form_save_error(form: &Value) -> Option<String> {
    let errors: Vec<_> = form["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["level"].as_str() == Some("error"))
        .filter_map(|message| message["text"].as_str())
        .map(str::to_string)
        .collect();
    (!errors.is_empty()).then(|| errors.join("\n"))
}

struct FieldSave {
    config_id: String,
    key: String,
    value: Value,
    generation: u64,
    snapshot: Vec<Value>,
}

/// Preserve edits made after the request snapshot; returned whole-page values
/// may have been read before newer fields were changed locally.
fn merge_save_values(form: &mut Value, current: &[Value], submitted: &[Value]) {
    if let Some(fields) = form["fields"].as_array_mut() {
        for field in fields {
            let key = &field["key"];
            if let Some(local) = current.iter().find(|candidate| candidate["key"] == *key) {
                let baseline = submitted.iter().find(|candidate| candidate["key"] == *key);
                if baseline.is_none_or(|baseline| baseline["value"] != local["value"]) {
                    field["value"] = local["value"].clone();
                }
            }
        }
    }
}

fn merge_protected_reply_values(form: &mut Value, current: &[Value], protected: &HashSet<String>) {
    if let Some(fields) = form["fields"].as_array_mut() {
        for field in fields {
            if field["key"]
                .as_str()
                .is_some_and(|key| protected.contains(key))
            {
                if let Some(local) = current.iter().find(|local| local["key"] == field["key"]) {
                    field["value"] = local["value"].clone();
                }
            }
        }
    }
}

impl SettingsForm {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        only: Option<Vec<&'static str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        EMBEDDED_SETTINGS_FORMS.with(|forms| {
            let mut forms = forms.borrow_mut();
            forms.retain(|_, entries| {
                entries.retain(|entry| entry.upgrade().is_some());
                !entries.is_empty()
            });
            forms
                .entry(window.window_handle())
                .or_default()
                .push(cx.entity().downgrade());
        });
        let capture_focus = cx.focus_handle();
        let saves = Arc::new(Mutex::new(SaveQueue::default()));
        SETTINGS_WRITE_QUEUES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .push(saves.clone());
        let blur = cx.on_blur(&capture_focus, window, |this, _, cx| {
            if this.capture.take().is_some() {
                cx.notify();
            }
        });
        let language_watch = cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(250)).await;
            if this
                .update_in(cx, |this, window, cx| {
                    this.refresh_language_schema(cx);
                    this.settle_tag_undo(window, cx);
                    this.refresh_authoritative_values(window, cx);
                })
                .is_err()
            {
                break;
            }
        });
        let list_settings_popover = settings_list_popover::is_list_settings_popover(only.as_deref());
        let search_keys = list_settings_popover.then(|| settings_list_popover::page_keys(true));
        Self {
            host,
            tokens,
            surface_palettes: None,
            dark,
            config_id: String::new(),
            compact: only.is_some(),
            list_settings_popover,
            only,
            fields: Vec::new(),
            schema_initialized: false,
            authoritative_refresh_all: false,
            authoritative_refresh_keys: HashSet::new(),
            undo_baseline: SettingsUndoBaseline::default(),
            tag_undo_needs_reload: false,
            tag_undo_owner: crate::filters::new_filter_tab_write_owner(),
            tag_undo_running: false,
            tag_undo_recovery: None,
            tag_undo_error: None,
            search_keys,
            super_menu_sections: None,
            inputs: HashMap::new(),
            sliders: HashMap::new(),
            slider_search_input: None,
            color_focus: HashMap::new(),
            choice_focus: HashMap::new(),
            choice_anchors: HashMap::new(),
            choice_popup: None,
            color_popup: None,
            color_anchors: HashMap::new(),
            standalone_window: false,
            lines: HashMap::new(),
            line_dragging: false,
            line_drop_target: None,
            capture: None,
            capture_focus,
            tag_editor: None,
            text_actions: None,
            message: None,
            generation: 0,
            saves,
            failed_saves: HashSet::new(),
            language: displayed_language(),
            _language_watch: language_watch,
            input_subscriptions: Vec::new(),
            _subscriptions: vec![blur],
            leave_locked: false,
            leave_epoch: 0,
            pending_pickers: 0,
            folder_pickers: FolderPickerRequests::default(),
            motion: RefCell::new(Tweens::default()),
            hovered: HashSet::new(),
        }
    }

    pub fn config_id(&self) -> &str {
        &self.config_id
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    fn reset_undo_baseline(&mut self, cx: &mut Context<Self>) {
        self.undo_baseline.reset(&self.fields);
        cx.notify();
    }

    fn undo_preflight(&self, cx: &App) -> Result<(), String> {
        if self.leave_locked
            || self.pending_writes()
            || self.capture.is_some()
            || self.composing(cx)
            || self
                .tag_editor
                .as_ref()
                .is_some_and(|(editor, _)| editor.read(cx).has_active_save_or_composition(cx))
            || self
                .text_actions
                .as_ref()
                .is_some_and(|actions| actions.read(cx).has_active_save_or_composition(cx))
        {
            return Err(tr(
                "请先完成输入、选择或保存，再撤销修改。",
                "Finish input, selection or saving before undoing changes.",
            )
            .into());
        }
        if let Some(error) = self
            .tag_editor
            .as_ref()
            .and_then(|(editor, _)| editor.read(cx).save_failure())
        {
            return Err(error.to_string());
        }
        Ok(())
    }

    fn undo_changes(&self, cx: &App) -> Vec<(String, Value, bool)> {
        self.fields
            .iter()
            .filter_map(|field| {
                if kind(field) == Kind::External {
                    return None;
                }
                let key = field["key"].as_str()?;
                let baseline = self.undo_baseline.values.get(key)?;
                let failed = self.failed_saves.contains(key)
                    || self
                        .saves
                        .lock()
                        .unwrap()
                        .errors
                        .contains_key(&(self.config_id.clone(), key.to_string()));
                let write = failed || !same_value(&field["value"], baseline);
                let baseline_text = text_value(&json!({"value":baseline}));
                let draft = self
                    .inputs
                    .get(key)
                    .is_some_and(|input| input.read(cx).text() != baseline_text)
                    || self.lines.get(key).is_some_and(|rows| {
                        let raw: Vec<_> = rows.iter().map(|input| input.read(cx).text()).collect();
                        !raw_lines_match_accepted(&raw, &baseline_text)
                    });
                (write || draft).then(|| (key.to_string(), baseline.clone(), write))
            })
            .collect()
    }

    fn undo_plan(&self, cx: &App) -> Result<Vec<(String, Value, bool)>, String> {
        let changes = self.undo_changes(cx);
        if changes.is_empty() {
            return Ok(changes);
        }
        let form = self
            .host
            .settings_form_get(&json!({"configId":self.config_id}))?;
        let fresh = form["fields"].as_array().ok_or_else(|| {
            tr(
                "设置值无法读取，未撤销当前输入。",
                "Settings could not be read; input was retained.",
            )
            .to_string()
        })?;
        let mut plan: Vec<_> = changes
            .into_iter()
            .map(|(key, target, _)| {
                let saved = fresh
                    .iter()
                    .find(|field| field["key"] == key)
                    .and_then(|field| field.get("value"))
                    .ok_or_else(|| {
                        format!(
                            "{}: {key}",
                            tr(
                                "设置字段已变化，未撤销",
                                "The setting changed; undo was not applied"
                            )
                        )
                    })?;
                let failed = self.failed_saves.contains(&key)
                    || self
                        .saves
                        .lock()
                        .unwrap()
                        .errors
                        .contains_key(&(self.config_id.clone(), key.clone()));
                Ok((
                    key,
                    target.clone(),
                    undo_write_required(saved, &target, failed),
                ))
            })
            .collect::<Result<_, String>>()?;
        order_undo_values(&self.config_id, &mut plan, &self.undo_baseline);
        Ok(plan)
    }

    fn start_tag_undo(
        &mut self,
        target: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let text = target
            .as_str()
            .ok_or_else(|| "Category replacement must be text.".to_string())?
            .to_string();
        let (id, accepted) = crate::filters::submit_filter_tab_replacement(
            self.tag_undo_owner,
            self.tag_undo_recovery,
            self.host.clone(),
            text,
        )?;
        self.tag_undo_running = true;
        self.tag_undo_needs_reload = true;
        self.tag_undo_recovery = Some(id);
        if let Some((editor, _)) = &self.tag_editor {
            editor.update(cx, |editor, cx| editor.set_leave_locked(true, cx));
        }
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = accepted.await.unwrap_or_else(|_| {
                Err(tr(
                    "分类撤销保存线程不可用。",
                    "The category undo writer is unavailable.",
                )
                .into())
            });
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.tag_undo_running = false;
                match result {
                    Ok(()) => {
                        this.tag_undo_recovery = None;
                        this.tag_undo_error = None;
                        this.failed_saves.remove("FilterItems");
                        if this.persistence_error().is_none() {
                            this.message = None;
                        }
                    }
                    Err(error) => {
                        this.tag_undo_error = Some(error.clone());
                        this.failed_saves.insert("FilterItems".into());
                        this.message = Some(FormMessage::SaveFailed(error));
                    }
                }
                this.settle_tag_undo(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        Ok(())
    }

    fn restore_undo(
        &mut self,
        changes: Vec<(String, Value, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if changes.is_empty() {
            return;
        }
        // An explicit rollback retries failed intent. Keep its error visible
        // until a successful receipt; never acknowledge it just by resetting UI.
        let retain_error = self.persistence_error().is_some();
        let previous_message = self.message.take();
        for (key, value, write) in changes {
            if let Some(slider) = self.sliders.get(&key) {
                slider.update(cx, |slider, cx| slider.retire(cx));
            }
            if let Some(field) = self.fields.iter_mut().find(|field| field["key"] == key) {
                field["value"] = value.clone();
            }
            let text = text_value(&json!({"value":value}));
            if let Some(input) = self.inputs.get(&key) {
                input.update(cx, |input, cx| input.set_text(text.clone(), window, cx));
            }
            if let Some(existing) = self.lines.get(&key).cloned() {
                let rows = text
                    .lines()
                    .enumerate()
                    .map(|(index, text)| {
                        if let Some(input) = existing.get(index) {
                            input.update(cx, |input, cx| input.set_text(text.into(), window, cx));
                            input.clone()
                        } else {
                            self.make_input(&key, text.into(), window, cx)
                        }
                    })
                    .collect();
                self.lines.insert(key.clone(), rows);
            }
            if self
                .field(&key)
                .is_some_and(|field| kind(field) == Kind::Tags)
            {
                self.tag_undo_needs_reload = true;
                if let Some((editor, _)) = &self.tag_editor {
                    editor.update(cx, |editor, cx| editor.set_leave_locked(true, cx));
                }
            } else if write {
                self.enqueue_save(&key, value, window, cx);
            }
            self.synchronize_slider(&key, cx);
        }
        if retain_error {
            self.message = previous_message;
        }
        self.settle_tag_undo(window, cx);
        cx.notify();
    }

    fn settle_tag_undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tag_undo_needs_reload || self.leave_locked || self.pending_writes() {
            return;
        }
        if let Some((editor, _)) = &self.tag_editor {
            editor.update(cx, |editor, cx| {
                editor.set_leave_locked(self.tag_undo_error.is_some(), cx);
                if self.tag_undo_error.is_none() {
                    editor.reload(window, cx);
                }
            });
        }
        if self.tag_undo_error.is_none() {
            self.tag_undo_needs_reload = false;
        }
    }

    /// Coalesce persisted config events; the existing watcher applies them
    /// after a field's local edit, capture or accepted write has finished.
    pub fn request_authoritative_refresh(&mut self, cx: &mut Context<Self>) {
        self.authoritative_refresh_all = true;
        cx.notify();
    }

    fn authoritative_field_is_idle(&self, field: &Value, window: &Window, cx: &App) -> bool {
        let key = field["key"].as_str().unwrap_or_default();
        if self.sliders.get(key).is_some_and(|slider| slider.read(cx).is_interacting()) {
            return false;
        }
        if self.failed_saves.contains(key)
            || self.capture.as_deref() == Some(key)
            || self
                .choice_popup
                .as_ref()
                .is_some_and(|session| session.key == key)
            || self
                .color_popup
                .as_ref()
                .is_some_and(|session| session.key == key)
            || self.folder_pickers.is_pending(key)
            || self
                .saves
                .lock()
                .unwrap()
                .errors
                .contains_key(&(self.config_id.clone(), key.to_string()))
        {
            return false;
        }
        if let Some(input) = self.inputs.get(key) {
            let input = input.read(cx);
            return can_normalize_input(
                input.text(),
                &text_value(field),
                input.is_composing(),
                input.focus_handle().is_focused(window),
            );
        }
        if let Some(rows) = self.lines.get(key) {
            // Compare raw rows, including empty rows and whitespace. The
            // normalized save projection must not make an unsaved row clean.
            if rows.iter().any(|input| {
                let input = input.read(cx);
                input.is_composing() || input.focus_handle().is_focused(window)
            }) {
                return false;
            }
            let draft: Vec<_> = rows.iter().map(|input| input.read(cx).text()).collect();
            return raw_lines_match_accepted(&draft, &text_value(field));
        }
        kind(field) != Kind::Tags
    }

    fn refresh_authoritative_values(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.schema_initialized || self.config_id.is_empty() {
            return;
        }
        if self.authoritative_refresh_all {
            self.authoritative_refresh_keys
                .extend(self.fields.iter().filter_map(|field| {
                    // Dedicated explicit-save editors own their independent drafts.
                    (kind(field) != Kind::Tags)
                        .then(|| field["key"].as_str().map(str::to_string))
                        .flatten()
                }));
            self.authoritative_refresh_all = false;
        }
        if self.authoritative_refresh_keys.is_empty() || self.leave_locked || self.pending_writes()
        {
            return;
        }
        let ready: HashSet<String> = self
            .fields
            .iter()
            .filter_map(|field| {
                let key = field["key"].as_str()?;
                (self.authoritative_refresh_keys.contains(key)
                    && self.authoritative_field_is_idle(field, window, cx))
                .then(|| key.to_string())
            })
            .collect();
        if ready.is_empty() {
            return;
        }
        // Read and merge in one UI update. Unlike an async whole-page reply,
        // no UI save can finish between the protection check and this merge.
        let Ok(form) = self
            .host
            .settings_form_get(&json!({ "configId": self.config_id }))
        else {
            return; // Retry later; retain both pending keys and existing errors.
        };
        let Some(fresh) = form["fields"].as_array() else {
            return;
        };
        let mut changed = false;
        for (index, key, source) in authoritative_field_updates(&self.fields, fresh, &ready) {
            self.undo_baseline.observe_authoritative(
                &key,
                &self.fields[index]["value"],
                &source["value"],
                true,
            );
            if !same_value(&self.fields[index]["value"], &source["value"]) {
                if let Some(slider) = self.sliders.get(&key) {
                    slider.update(cx, |slider, cx| slider.retire(cx));
                }
                let text = text_value(source);
                if let Some(input) = self.inputs.get(&key) {
                    input.update(cx, |input, cx| input.set_text(text.clone(), window, cx));
                }
                if let Some(existing) = self.lines.get(&key).cloned() {
                    let rows = text
                        .lines()
                        .enumerate()
                        .map(|(index, text)| {
                            if let Some(input) = existing.get(index) {
                                if input.read(cx).text() != text {
                                    input.update(cx, |input, cx| {
                                        input.set_text(text.into(), window, cx)
                                    });
                                }
                                input.clone()
                            } else {
                                self.make_input(&key, text.into(), window, cx)
                            }
                        })
                        .collect();
                    self.lines.insert(key.clone(), rows);
                }
                // Controls and their accepted value move together. A protected
                // input keeps BOTH old text and old baseline until it is idle.
                self.fields[index]["value"] = source["value"].clone();
                changed = true;
            }
            self.authoritative_refresh_keys.remove(&key);
        }
        if changed {
            self.synchronize_sliders(cx);
            cx.notify();
        }
    }

    /// Theme refresh never rebuilds controls or replaces an editable draft.
    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark == dark {
            return;
        }
        self.refresh_control_theme(dark, cx);
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.refresh_control_theme(dark, cx);
    }

    /// Embedded settings use the same surface roles as the settings window.
    /// The caller supplies resolved theme tokens before any glass derivation.
    pub fn set_window_background_theme(
        &mut self,
        enabled: bool,
        options: WindowBackgroundOptions,
        base_tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        let tokens = settings_window_tokens(&base_tokens, &options, enabled);
        self.surface_palettes = enabled.then(|| {
            (
                SettingsPalette::with_background(
                    base_tokens.theme(false),
                    tokens.theme(false),
                    &options,
                ),
                SettingsPalette::with_background(
                    base_tokens.theme(true),
                    tokens.theme(true),
                    &options,
                ),
            )
        });
        self.set_design_tokens(tokens, dark, cx);
    }

    fn palette(&self, theme: &ThemeTokens) -> SettingsPalette {
        self.surface_palettes.map_or_else(
            || SettingsPalette::new(theme),
            |(light, dark)| if self.dark { dark } else { light },
        )
    }

    fn refresh_control_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        // A color refresh closes the color popover without committing its draft.
        self.color_popup = None;
        self.reset_motion();
        let t = self.tokens.theme(dark);
        let palette = self.palette(t);
        let colors = InputColors {
            // The wrapper paints the control and its hover surface.
            background: gpui::transparent_black(),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: palette.control_border,
            selection: color(&t.tab_active),
            caret: color(&t.accent),
        };
        for input in self.inputs.values().chain(self.lines.values().flatten()) {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        for slider in self.sliders.values() {
            slider.update(cx, |slider, cx| slider.set_colors(t, palette, cx));
        }
        if let Some(actions) = &self.text_actions {
            actions.update(cx, |actions, cx| {
                actions.set_design_tokens(self.tokens.clone(), dark, cx)
            });
        }
        if let Some((editor, _)) = &self.tag_editor {
            editor.update(cx, |editor, cx| {
                editor.set_design_tokens(self.tokens.clone(), dark, cx)
            });
        }
        if let Some(session) = &self.choice_popup {
            session
                .popup
                .update(cx, |popup, cx| popup.update_colors(&t, palette, cx));
        }
        cx.notify();
    }

    fn composing(&self, cx: &App) -> bool {
        self.inputs
            .values()
            .chain(self.lines.values().flatten())
            .any(|input| input.read(cx).is_composing())
    }

    fn projection_busy(&self, cx: &App) -> bool {
        self.line_dragging || self.choice_popup.is_some()
            || self.color_popup.is_some()
            || self.composing(cx)
            || self
                .tag_editor
                .as_ref()
                .is_some_and(|(editor, _)| editor.read(cx).projection_busy(cx))
            || self
                .text_actions
                .as_ref()
                .is_some_and(|actions| actions.read(cx).has_active_save_or_composition(cx))
    }

    pub fn set_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if locked && !self.leave_locked {
            self.leave_epoch = self.leave_epoch.wrapping_add(1);
        }
        self.leave_locked = locked;
        for (key, slider) in &self.sliders {
            let enabled = !locked && self.search_keys.as_ref().is_none_or(|keys| keys.contains(key));
            slider.update(cx, |slider, cx| slider.set_enabled(enabled, cx));
        }
        if locked {
            self.line_dragging = false;
            self.line_drop_target = None;
            self.capture = None;
            self.choice_popup = None;
            self.color_popup = None;
            self.folder_pickers.retire_all();
        }
        for input in self.inputs.values().chain(self.lines.values().flatten()) {
            input.update(cx, |input, cx| input.set_read_only(locked, cx));
        }
        if let Some((editor, _)) = &self.tag_editor {
            editor.update(cx, |editor, cx| {
                editor.set_leave_locked(
                    locked
                        || self.tag_undo_needs_reload
                        || self.tag_undo_running
                        || self.tag_undo_error.is_some(),
                    cx,
                )
            });
        }
        cx.notify();
    }

    fn input_value(&self, field: &Value, cx: &App) -> Result<Option<Value>, String> {
        let key = field["key"].as_str().unwrap_or_default();
        let composing = tr(
            "请先完成输入法组合，再保存或离开。",
            "Finish composing text before saving or leaving.",
        );
        match kind(field) {
            Kind::Lines => {
                let Some(rows) = self.lines.get(key) else {
                    return Ok(None);
                };
                if rows.iter().any(|input| input.read(cx).is_composing()) {
                    return Err(composing.into());
                }
                Ok(Some(json!(rows
                    .iter()
                    .map(|input| input.read(cx).text().trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"))))
            }
            Kind::Number | Kind::Text | Kind::Folder | Kind::ImageFile | Kind::Color => {
                let Some(input) = self.inputs.get(key) else {
                    return Ok(None);
                };
                let input = input.read(cx);
                if input.is_composing() {
                    return Err(composing.into());
                }
                if kind(field) == Kind::Color {
                    // Like the web editor, normalize valid WPF colors while
                    // preserving an invalid raw draft for the existing core
                    // save contract and the chooser's explicit correction.
                    Ok(Some(json!(ArgbColor::parse(input.text())
                        .map(ArgbColor::serialize)
                        .unwrap_or_else(|| input.text().to_string()))))
                } else if kind(field) == Kind::Number {
                    let number = input
                        .text()
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|number| number.is_finite())
                        .ok_or_else(|| {
                            format!(
                                "{}: {}",
                                field["label"].as_str().unwrap_or(key),
                                tr("请输入有效数字。", "Enter a valid number.")
                            )
                        })?;
                    Ok(Some(number_json(field, number)))
                } else {
                    Ok(Some(json!(input.text())))
                }
            }
            _ => Ok(None),
        }
    }

    fn preflight_inputs(&self, cx: &App) -> Result<Vec<(String, Value, Value)>, String> {
        if self.choice_popup.is_some() {
            return Err(tr(
                "请先选择选项或关闭下拉列表，再离开设置。",
                "Choose an option or close the selection list before leaving settings.",
            )
            .into());
        }
        // An open color popover does not block leaving: `set_leave_locked(true)`
        // closes it without committing its draft; the text box keeps its value.
        if self
            .tag_editor
            .as_ref()
            .is_some_and(|(editor, _)| editor.read(cx).has_active_save_or_composition(cx))
        {
            return Err(tr(
                "分类仍在保存或输入法组合中，请完成后再离开。",
                "Categories are still saving or composing. Finish them before leaving.",
            )
            .into());
        }
        if let Some(error) = self
            .tag_editor
            .as_ref()
            .and_then(|(editor, _)| editor.read(cx).save_failure())
        {
            return Err(error.to_string());
        }
        if self
            .text_actions
            .as_ref()
            .is_some_and(|actions| actions.read(cx).has_active_save_or_composition(cx))
        {
            return Err(tr(
                "文本操作仍在保存或输入法组合中，请完成后再离开。",
                "Text action is still saving or composing. Finish it before leaving.",
            )
            .into());
        }
        if self.composing(cx) {
            return Err(tr(
                "请先完成输入法组合，再保存或离开。",
                "Finish composing text before saving or leaving.",
            )
            .into());
        }
        let mut values = Vec::new();
        for field in &self.fields {
            match self.input_value(field, cx) {
                Ok(Some(value)) => values.push((
                    field["key"].as_str().unwrap_or_default().to_string(),
                    value,
                    field["value"].clone(),
                )),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(values)
    }

    /// Read current controls before blur/load/destruction. Explicit category
    /// and text-action drafts are checked for active work, never auto-submitted.
    pub fn commit_all_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let values = match self.preflight_inputs(cx) {
            Ok(values) => values,
            Err(error) => {
                self.message = Some(FormMessage::Raw(true, error));
                cx.notify();
                return false;
            }
        };
        for (key, value, old) in values {
            self.normalize_color_input(&key, &value, window, cx);
            if !same_value(&value, &old) || self.failed_saves.contains(&key) {
                self.save(&key, value, window, cx);
            }
        }
        true
    }

    /// Parent API for one or multiple embedded forms (e.g. panel + global).
    /// Preflight ALL forms before any leave writes, then commit, lock and take
    /// barriers in the same UI turn. Keep the original entities mounted until
    /// `ticket.wait().await.finish(cx)` returns Ok. Err keeps their exact drafts.
    pub fn prepare_leave(
        forms: &[Entity<Self>],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<SettingsFormsLeave, String> {
        let mut seen = HashSet::new();
        let mut prepared = Vec::new();
        for form in forms {
            if !seen.insert(form.entity_id()) {
                continue;
            }
            let values = form.update(cx, |form, cx| {
                let values = if form.leave_locked {
                    Err(tr(
                        "设置正在离开，请等待保存完成。",
                        "Settings are leaving. Wait for saving to finish.",
                    )
                    .into())
                } else if form.pending_pickers > 0 {
                    Err(tr(
                        "请先完成当前选择，再离开设置。",
                        "Finish the current selection before leaving settings.",
                    )
                    .into())
                } else {
                    form.preflight_inputs(cx)
                };
                if let Err(error) = &values {
                    form.message = Some(FormMessage::Raw(true, error.clone()));
                    cx.notify();
                }
                values
            })?;
            prepared.push((form.clone(), values));
        }
        let mut participants = Vec::with_capacity(prepared.len());
        let mut barriers = Vec::with_capacity(prepared.len());
        for (form, values) in prepared {
            let (generation, epoch, barrier) = form.update(cx, |form, cx| {
                for (key, value, old) in values {
                    form.normalize_color_input(&key, &value, window, cx);
                    if !same_value(&value, &old) || form.failed_saves.contains(&key) {
                        form.save(&key, value, window, cx);
                    }
                }
                form.set_leave_locked(true, cx);
                (form.generation, form.leave_epoch, form.write_barrier())
            });
            participants.push(SettingsLeaveParticipant {
                form,
                generation,
                epoch,
            });
            barriers.push(barrier);
        }
        window.blur();
        Ok(SettingsFormsLeave {
            participants,
            barriers,
        })
    }

    fn pending_writes(&self) -> bool {
        let queue = self.saves.lock().unwrap();
        self.pending_pickers > 0
            || self.tag_undo_running
            || queue.running
            || !queue.pending.is_empty()
    }

    /// Barrier for already accepted writes. Capture controls synchronously
    /// with commit_all_inputs before awaiting this from a surface leave.
    pub fn write_barrier(&self) -> SettingsCloseResult {
        queue_barrier(&self.saves)
    }

    pub fn has_pending_saves(&self) -> bool {
        self.pending_writes()
    }

    pub fn save_failure(&self) -> Option<String> {
        self.persistence_error()
    }

    fn persistence_error(&self) -> Option<String> {
        queue_error(&self.saves.lock().unwrap()).or_else(|| self.tag_undo_error.clone())
    }

    pub fn load(&mut self, config_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .host
            .settings_form_get(&json!({ "configId": config_id }));
        self.load_schema(config_id, result, window, cx);
    }

    fn load_schema(
        &mut self,
        config_id: &str,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.retire_sliders(cx);
        if self.config_id != config_id {
            self.authoritative_refresh_all = false;
            self.authoritative_refresh_keys.clear();
        }
        self.folder_pickers.retire_all();
        self.choice_popup = None;
        self.color_popup = None;
        self.reset_motion();
        self.language = displayed_language();
        self.config_id = config_id.to_string();
        self.generation += 1;
        self.line_dragging = false;
        self.line_drop_target = None;
        self.failed_saves.clear();
        self.capture = None;
        self.message = None;
        self.text_actions = if config_id == "textAction" {
            let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
            Some(cx.new(|cx| TextActionsPage::new(host, tokens, dark, window, cx)))
        } else {
            None
        };
        // The dedicated editor owns live drafts even if its empty schema GET
        // fails; retry must not replace that existing editor.
        self.schema_initialized = self.text_actions.is_some();
        match result {
            Ok(form) => {
                self.apply_form(form, true, window, cx);
                self.schema_initialized = true;
            }
            Err(error) => {
                self.fields.clear();
                self.message = Some(FormMessage::Raw(true, error));
            }
        }
        cx.notify();
    }

    fn refresh_language_schema(&mut self, cx: &mut Context<Self>) {
        let current = displayed_language();
        if self.language == current || self.config_id.is_empty() {
            return;
        }
        self.reload_localized_metadata(cx);
    }

    fn reload_localized_metadata(&mut self, cx: &mut Context<Self>) {
        if let Ok(form) = self
            .host
            .settings_form_get(&json!({ "configId": self.config_id }))
        {
            if let Some(fields) = form["fields"].as_array() {
                merge_localized_metadata(&mut self.fields, fields);
            }
            refresh_schema_message(&mut self.message, &form);
            self.language = displayed_language();
            self.refresh_image_placeholders(cx);
            // This intentionally leaves input drafts, lines, capture, tag_editor,
            // generation and pending saves untouched.
            cx.notify();
        }
    }

    fn refresh_image_placeholders(&self, cx: &mut Context<Self>) {
        for field in &self.fields {
            if kind(field) == Kind::ImageFile {
                if let Some(input) = field["key"].as_str().and_then(|key| self.inputs.get(key)) {
                    input.update(cx, |input, cx| {
                        input.set_placeholder(
                            tr(
                                "选择本地图片路径，留空表示不使用背景图",
                                "Choose a local image; leave blank for no background image",
                            ),
                            cx,
                        )
                    });
                }
            }
        }
    }

    fn apply_form(
        &mut self,
        form: Value,
        rebuild: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut fields = form["fields"].as_array().cloned().unwrap_or_default();
        if let Some(only) = &self.only {
            fields.retain(|field| only.iter().any(|key| field["key"] == *key));
            fields.sort_by_key(|field| only.iter().position(|key| field["key"] == *key));
        }
        let previous_fields = std::mem::replace(&mut self.fields, fields);
        if rebuild {
            self.retire_sliders(cx);
            self.inputs.clear();
            self.sliders.clear();
            self.color_focus.clear();
            self.choice_focus.clear();
            self.choice_anchors.clear();
            self.choice_popup = None;
            self.color_popup = None;
            self.reset_motion();
            self.color_anchors.clear();
            self.lines.clear();
            self.input_subscriptions.clear();
            self.tag_editor = None;
            for field in self.fields.clone() {
                let key = field["key"].as_str().unwrap_or_default().to_string();
                match kind(&field) {
                    Kind::Slider => {
                        if let Some(domain) = slider_domain(&field) {
                            let theme = self.tokens.theme(self.dark).clone();
                            let palette = self.palette(&theme);
                            let value = field["value"].as_f64().unwrap_or(0.0);
                            let slider = cx.new(|cx| SettingsSlider::new(
                                domain, value, &theme, palette, window, cx,
                            ));
                            let generation = self.generation;
                            let config_id = self.config_id.clone();
                            let event_key = key.clone();
                            self.input_subscriptions.push(cx.subscribe_in(
                                &slider, window, move |this, slider, event, window, cx| {
                                    let current = this.sliders.get(&event_key)
                                        .is_some_and(|current| current.entity_id() == slider.entity_id());
                                    let visible = this.search_keys.as_ref()
                                        .is_none_or(|keys| keys.contains(&event_key));
                                    let valid = current && visible && !this.leave_locked
                                        && this.generation == generation && this.config_id == config_id
                                        && !this.slider_composition_active(window, cx)
                                        && this.field(&event_key).is_some_and(|field| kind(field) == Kind::Slider)
                                        && slider.read(cx).accepts(event)
                                        && (event.request_focus || slider.read(cx).focus_handle().is_focused(window));
                                    if !valid {
                                        slider.update(cx, |slider, cx| slider.retire(cx));
                                        this.synchronize_slider(&event_key, cx);
                                        return;
                                    }
                                    if event.request_focus {
                                        window.focus(&slider.read(cx).focus_handle());
                                    }
                                    this.accept_number(&event_key, json!(event.value), window, cx);
                                },
                            ));
                            self.sliders.insert(key, slider);
                        }
                    }
                    Kind::Text | Kind::Number | Kind::Folder | Kind::ImageFile | Kind::Color
                        if !self.compact || (self.list_settings_popover && kind(&field) == Kind::Number) =>
                    {
                        let input = self.make_input(&key, text_value(&field), window, cx);
                        if kind(&field) == Kind::Color {
                            self.color_focus
                                .insert(key.clone(), cx.focus_handle().tab_stop(true));
                            self.color_anchors
                                .insert(key.clone(), Arc::new(Mutex::new(None)));
                        }
                        if kind(&field) == Kind::ImageFile {
                            input.update(cx, |input, cx| {
                                input.set_placeholder(
                                    tr(
                                        "选择本地图片路径，留空表示不使用背景图",
                                        "Choose a local image; leave blank for no background image",
                                    ),
                                    cx,
                                )
                            });
                        }
                        self.inputs.insert(key, input);
                    }
                    Kind::Choice if ordinary_settings_choice(&self.config_id, &key) => {
                        self.choice_focus
                            .insert(key.clone(), cx.focus_handle().tab_stop(true));
                        self.choice_anchors.insert(key, Arc::new(Mutex::new(None)));
                    }
                    Kind::Tags if !self.compact => {
                        let (host, tokens, dark) =
                            (self.host.clone(), self.tokens.clone(), self.dark);
                        let editor =
                            cx.new(|cx| FilterTabsEditor::new(host, tokens, dark, window, cx));
                        let subscription =
                            cx.subscribe_in(&editor, window, |this, editor, event, window, cx| {
                                match event {
                                    FilterTabsEvent::Saved => {
                                        if let Ok(form) = this
                                            .host
                                            .settings_form_get(&json!({"configId":this.config_id}))
                                        {
                                            if let Some(value) = form["fields"]
                                                .as_array()
                                                .and_then(|fields| {
                                                    fields
                                                        .iter()
                                                        .find(|field| kind(field) == Kind::Tags)
                                                })
                                                .and_then(|field| field.get("value"))
                                            {
                                                if let Some(field) = this
                                                    .fields
                                                    .iter_mut()
                                                    .find(|field| kind(field) == Kind::Tags)
                                                {
                                                    field["value"] = value.clone();
                                                }
                                            }
                                        }
                                        this.message = Some(FormMessage::TagsSaved);
                                        cx.notify();
                                    }
                                    FilterTabsEvent::Cancelled => {
                                        editor.update(cx, |editor, cx| editor.reload(window, cx));
                                        this.message = None;
                                        cx.notify();
                                    }
                                }
                            });
                        self.tag_editor = Some((editor, subscription));
                    }
                    Kind::Lines => {
                        let rows = text_value(&field)
                            .lines()
                            .map(|line| self.make_input(&key, line.to_string(), window, cx))
                            .collect();
                        self.lines.insert(key, rows);
                    }
                    _ => {}
                }
            }
        } else {
            // Normalized values (clamps, canonical enums) flow back into idle inputs.
            for field in &self.fields {
                let key = field["key"].as_str().unwrap_or_default();
                if let Some(input) = self.inputs.get(key) {
                    let input_state = input.read(cx);
                    let focused = input_state.focus_handle().is_focused(window);
                    let accepted = previous_fields.iter().find(|old| old["key"] == key);
                    if accepted.is_some_and(|old| {
                        can_normalize_input(
                            input_state.text(),
                            &text_value(old),
                            input_state.is_composing(),
                            focused,
                        )
                    }) && input_state.text() != text_value(field)
                    {
                        let text = text_value(field);
                        input.update(cx, |input, cx| input.set_text(text, window, cx));
                    }
                }
            }
        }
        if let Some(message) = schema_message(&form) {
            self.message = Some(message);
        }
        if rebuild {
            self.undo_baseline.reset(&self.fields);
        }
        self.synchronize_sliders(cx);
    }

    fn retire_sliders(&self, cx: &mut Context<Self>) {
        for slider in self.sliders.values() {
            slider.update(cx, |slider, cx| slider.retire(cx));
        }
    }

    fn synchronize_slider(&self, key: &str, cx: &mut Context<Self>) {
        if let (Some(slider), Some(field)) = (self.sliders.get(key), self.field(key)) {
            if let Some(domain) = slider_domain(field) {
                let value = field["value"].as_f64().unwrap_or(0.0);
                slider.update(cx, |slider, cx| slider.synchronize(domain, value, cx));
            }
        }
    }

    fn synchronize_sliders(&self, cx: &mut Context<Self>) {
        for key in self.sliders.keys() {
            self.synchronize_slider(key, cx);
        }
    }

    fn set_search_keys(&mut self, keys: Option<HashSet<String>>, cx: &mut Context<Self>) {
        if self.search_keys != keys {
            self.retire_sliders(cx);
            self.choice_popup = None;
            self.color_popup = None;
            self.search_keys = keys;
            for (key, slider) in &self.sliders {
                let visible = self.search_keys.as_ref().is_none_or(|keys| keys.contains(key));
                slider.update(cx, |slider, cx| slider.set_enabled(visible && !self.leave_locked, cx));
            }
            cx.notify();
        }
    }

    fn make_input(
        &mut self,
        key: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SearchInput> {
        let t = self.tokens.theme(self.dark).clone();
        let palette = self.palette(&t);
        let height = if self.list_settings_popover {
            self.tokens.list_settings_popover.number_height - 2.0
        } else {
            self.tokens.settings.control_height - 2.0
        };
        let horizontal_padding = if self.list_settings_popover { 0.0 } else { 8.0 };
        let input = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(
                InputColors {
                    background: gpui::transparent_black(),
                    foreground: color(&t.text),
                    placeholder: color(&t.text_muted),
                    border: palette.control_border,
                    selection: color(&t.tab_active),
                    caret: color(&t.accent),
                },
                cx,
            );
            input.set_metrics(height, if self.compact { 12.0 } else { 13.0 }, horizontal_padding, cx);
            input.set_placeholder("", cx);
            input.set_double_click_clears(false);
            input.set_text(text, window, cx);
            input
        });
        let commit_key = key.to_string();
        self.input_subscriptions.push(cx.subscribe_in(
            &input,
            window,
            move |this, input, event, window, cx| {
                match event {
                    SearchEvent::Submit => this.commit(&commit_key, window, cx),
                    SearchEvent::Escape => this.revert(&commit_key, window, cx),
                    // Even editing back to the original text retires the picker;
                    // a selected path must not replace a later user decision.
                    SearchEvent::Changed(_) => {
                        if this
                            .inputs
                            .get(&commit_key)
                            .is_some_and(|current| current.entity_id() == input.entity_id())
                        {
                            this.folder_pickers.retire_key(&commit_key);
                        }
                    }
                }
            },
        ));
        let focus = input.read(cx).focus_handle();
        self.input_subscriptions
            .push(cx.observe_in(&input, window, |_, _, _, cx| cx.notify()));
        let blur_key = key.to_string();
        self.input_subscriptions
            .push(cx.on_blur(&focus, window, move |this, window, cx| {
                this.commit(&blur_key, window, cx)
            }));
        input
    }

    fn field(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|field| field["key"] == key)
    }

    fn tab_focus_handles(&self, cx: &App) -> Vec<FocusHandle> {
        let mut handles = Vec::new();
        for field in self.display_fields() {
            let key = field["key"].as_str().unwrap_or_default();
            if self
                .search_keys
                .as_ref()
                .is_some_and(|keys| !keys.contains(key))
            {
                continue;
            }
            if let Some(focus) = self
                .choice_focus
                .get(key)
                .or_else(|| self.color_focus.get(key))
            {
                handles.push(focus.clone());
            }
            if let Some(input) = self.inputs.get(key) {
                handles.push(input.read(cx).focus_handle());
            }
            if let Some(slider) = self.sliders.get(key) {
                handles.push(slider.read(cx).focus_handle());
            }
            if let Some(rows) = self.lines.get(key) {
                handles.extend(rows.iter().map(|input| input.read(cx).focus_handle()));
            }
            if kind(field) == Kind::Tags {
                if let Some((editor, _)) = &self.tag_editor {
                    handles.extend(editor.read(cx).input_focus_handles(cx));
                }
            }
        }
        handles
    }

    fn display_fields(&self) -> Vec<&Value> {
        let mut fields: Vec<_> = self.fields.iter().collect();
        if self.config_id == "menu_global" {
            // GPUI opens super menu settings from its gear, not a trailing row.
            fields.retain(|field| field["key"] != "ShowSettingItem");
        }
        if self.config_id == "menu_global" && self.super_menu_sections.is_some() {
            fields.sort_by_key(|field| super_menu_field_section(field["key"].as_str().unwrap_or_default()).0);
        } else if self.config_id == "menu_file" {
            fields.sort_by_key(|field| field["key"] != "Files");
        }
        fields
    }

    fn composition_active(&self, cx: &App) -> bool {
        self.composing(cx)
            || self
                .tag_editor
                .as_ref()
                .is_some_and(|(editor, _)| editor.read(cx).is_composing(cx))
            || self
                .text_actions
                .as_ref()
                .is_some_and(|actions| actions.read(cx).has_active_save_or_composition(cx))
    }

    fn slider_composition_active(&self, window: &Window, cx: &Context<Self>) -> bool {
        if self.composition_active(cx)
            || self.slider_search_input.as_ref()
                .and_then(WeakEntity::upgrade)
                .is_some_and(|input| input.read(cx).is_composing())
        {
            return true;
        }
        // A pointer may leave the search box or a sibling form while its IME
        // still owns a draft. Never read this already-borrowed form again.
        let peers = EMBEDDED_SETTINGS_FORMS.with(|forms| {
            forms.borrow().get(&window.window_handle()).cloned().unwrap_or_default()
        });
        peers.into_iter().filter_map(|peer| peer.upgrade())
            .filter(|peer| peer.entity_id() != cx.entity_id())
            .any(|peer| peer.read(cx).composition_active(cx))
    }

    fn tab_locked(&self, cx: &App) -> bool {
        self.leave_locked
            || self.composition_active(cx)
            || self.color_popup.is_some()
    }

    fn move_tab_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab_locked(cx) {
            return;
        }
        if self.standalone_window {
            cx.emit(SettingsTabRequest { reverse });
            return;
        }
        let peers = EMBEDDED_SETTINGS_FORMS.with(|forms| {
            forms
                .borrow()
                .get(&window.window_handle())
                .cloned()
                .unwrap_or_default()
        });
        let mut handles = Vec::new();
        for peer in peers.into_iter().filter_map(|peer| peer.upgrade()) {
            if peer.entity_id() == cx.entity_id() {
                handles.extend(self.tab_focus_handles(cx));
            } else {
                let form = peer.read(cx);
                if form.standalone_window {
                    continue;
                }
                if form.tab_locked(cx) {
                    return;
                }
                handles.extend(form.tab_focus_handles(cx));
            }
        }
        let current = handles.iter().position(|handle| handle.is_focused(window));
        if let Some(next) = next_settings_focus(current, handles.len(), reverse) {
            window.focus(&handles[next]);
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.line_dragging && event.keystroke.key == "escape" {
            self.line_dragging = false;
            self.line_drop_target = None;
            cx.stop_active_drag(window);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.composition_active(cx) {
            return;
        }
        if let Some(reverse) = settings_tab_direction(event) {
            window.prevent_default();
            cx.stop_propagation();
            self.move_tab_focus(reverse, window, cx);
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn open_list_settings_choice_visual(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        assert!(self.list_settings_popover && cliphost_core::is_headless());
        if !matches!(key, "DismissBehavior" | "ClipListExecuteMode") {
            return false;
        }
        self.open_settings_choice_visual(key, window, cx)
    }

    /// Debug fixture entry point; uses the production dropdown after its
    /// trigger has completed layout, without synthesizing native input.
    #[cfg(debug_assertions)]
    fn open_settings_choice_visual(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        assert!(cliphost_core::is_headless());
        if !self.field(key).is_some_and(|field| kind(field) == Kind::Choice)
            || self.choice_anchors.get(key).and_then(|anchor| *anchor.lock().unwrap()).is_none()
        {
            return false;
        }
        if self.choice_popup.as_ref().is_some_and(|session| session.key == key) {
            return true;
        }
        self.toggle_choice(key, window, cx);
        self.choice_popup.as_ref().is_some_and(|session| session.key == key)
    }

    fn toggle_choice(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab_locked(cx) {
            return;
        }
        if self
            .choice_popup
            .as_ref()
            .is_some_and(|session| session.key == key)
        {
            self.choice_popup = None;
            if let Some(focus) = self.choice_focus.get(key) {
                window.focus(focus);
            }
            cx.notify();
            return;
        }
        let Some(field) = self
            .field(key)
            .filter(|field| kind(field) == Kind::Choice)
            .cloned()
        else {
            return;
        };
        let Some(anchor) = self
            .choice_anchors
            .get(key)
            .and_then(|anchor| *anchor.lock().unwrap())
        else {
            return;
        };
        let options = field["selectionItems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let key = item["key"].as_str()?.to_string();
                Some(SettingsChoiceOption {
                    name: item["name"].as_str().unwrap_or(&key).to_string(),
                    des: item["des"].as_str().unwrap_or_default().to_string(),
                    key,
                })
            })
            .collect::<Vec<_>>();
        if options.is_empty() {
            return;
        }
        let theme = self.tokens.theme(self.dark).clone();
        let palette = self.palette(&theme);
        let popup = cx.new(|cx| {
            SettingsChoicePopup::new(
                key.to_string(),
                field["value"].as_str().unwrap_or_default().to_string(),
                options,
                &theme,
                palette,
                anchor,
                cx,
            )
            .with_list_settings(
                self.list_settings_popover
                    && matches!(key, "DismissBehavior" | "ClipListExecuteMode"),
            )
        });
        let (generation, leave_epoch, config_id) =
            (self.generation, self.leave_epoch, self.config_id.clone());
        let trigger_key = key.to_string();
        let subscription =
            cx.subscribe_in(&popup, window, move |this, popup, event, window, cx| {
                if this.leave_locked
                    || this.generation != generation
                    || this.leave_epoch != leave_epoch
                    || this.config_id != config_id
                    || !this
                        .choice_popup
                        .as_ref()
                        .is_some_and(|session| session.popup.entity_id() == popup.entity_id())
                    || this
                        .search_keys
                        .as_ref()
                        .is_some_and(|keys| !keys.contains(&trigger_key))
                {
                    return;
                }
                this.choice_popup = None;
                let restore_focus = !matches!(
                    event,
                    SettingsChoiceEvent::Dismissed(SettingsChoiceDismissal::Outside)
                );
                if restore_focus {
                    if let Some(focus) = this.choice_focus.get(&trigger_key) {
                        window.focus(focus);
                    }
                }
                match event {
                    SettingsChoiceEvent::Chosen { key, value } => {
                        let accepted = this.field(key).is_some_and(|field| {
                            kind(field) == Kind::Choice
                                && field["selectionItems"].as_array().is_some_and(|items| {
                                    items
                                        .iter()
                                        .any(|item| item["key"].as_str() == Some(value.as_str()))
                                })
                                && (!same_value(&field["value"], &json!(value))
                                    || this.failed_saves.contains(key))
                        });
                        if accepted {
                            this.save(key, json!(value), window, cx);
                        }
                    }
                    SettingsChoiceEvent::Dismissed(SettingsChoiceDismissal::Tab { reverse }) => {
                        this.move_tab_focus(*reverse, window, cx);
                    }
                    SettingsChoiceEvent::Dismissed(_) => {}
                }
                cx.notify();
            });
        self.color_popup = None;
        self.choice_popup = Some(SettingsChoiceSession {
            key: key.into(),
            popup: popup.clone(),
            _subscription: subscription,
        });
        popup.update(cx, |popup, cx| popup.focus(window, cx));
        cx.notify();
    }

    fn step_number(
        &mut self,
        key: &str,
        direction: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.leave_locked {
            return;
        }
        let Some(field) = self
            .field(key)
            .filter(|field| kind(field) == Kind::Number)
            .cloned()
        else {
            return;
        };
        let draft = if let Some(input) = self.inputs.get(key) {
            let input = input.read(cx);
            if input.is_composing() {
                return;
            }
            input.text().to_string()
        } else {
            text_value(&field)
        };
        if let Some(value) = stepped_number(&field, &draft, direction) {
            self.accept_number(key, value, window, cx);
        }
    }

    fn accept_number(
        &mut self,
        key: &str,
        value: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.leave_locked {
            return;
        }
        let Some(field) = self
            .field(key)
            .filter(|field| matches!(kind(field), Kind::Number | Kind::Slider))
            .cloned()
        else {
            return;
        };
        let Some(number) = value.as_f64().filter(|number| number.is_finite()) else {
            return;
        };
        let value = number_json(&field, number);
        if let Some(input) = self.inputs.get(key) {
            if input.read(cx).is_composing() {
                return;
            }
            let text = text_value(&json!({"value":value}));
            // Keep blur and subsequent clicks on the newly accepted draft.
            input.update(cx, |input, cx| input.set_text(text, window, cx));
        }
        if !same_value(&value, &field["value"]) || self.failed_saves.contains(key) {
            self.enqueue_save(key, value, window, cx);
        }
        self.synchronize_slider(key, cx);
        cx.notify();
    }

    fn revert(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked {
            return;
        }
        self.folder_pickers.retire_key(key);
        if let Some(slider) = self.sliders.get(key) {
            slider.update(cx, |slider, cx| slider.retire(cx));
            self.synchronize_slider(key, cx);
        }
        let Some(field) = self.field(key).cloned() else {
            return;
        };
        if let Some(input) = self.inputs.get(key) {
            input.update(cx, |input, cx| {
                input.set_text(text_value(&field), window, cx)
            });
        }
        if self.lines.contains_key(key) {
            let rows = text_value(&field)
                .lines()
                .map(|line| self.make_input(key, line.to_string(), window, cx))
                .collect();
            self.lines.insert(key.to_string(), rows);
        }
        cx.notify();
    }

    fn commit(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked {
            return;
        }
        self.commit_accepted_input(key, window, cx);
    }

    fn reorder_line(&mut self, field: &str, source: u64, target: u64, before: bool,
        window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked || self.composition_active(cx) || !self.field(field).is_some_and(|f| kind(f) == Kind::Lines) { return; }
        let Some(rows) = self.lines.get_mut(field) else { return; };
        let ids: Vec<_> = rows.iter().map(|input| input.entity_id().as_u64()).collect();
        let Some((from, to)) = line_reorder_destination(&ids, source, target, before) else { return; };
        let input = rows.remove(from);
        rows.insert(to, input);
        self.line_drop_target = None;
        self.commit(field, window, cx);
        cx.notify();
    }

    fn move_line(&mut self, field: &str, input: u64, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rows) = self.lines.get(field) else { return; };
        let Some(from) = rows.iter().position(|row| row.entity_id().as_u64() == input) else { return; };
        let to = from as isize + delta;
        let Some(target) = usize::try_from(to).ok().and_then(|to| rows.get(to)).map(|row| row.entity_id().as_u64()) else { return; };
        self.reorder_line(field, input, target, delta < 0, window, cx);
    }

    // A current file/folder-picker reply commits through the existing write queue.
    // Picker lifecycle checks happen before it can reach this accepted path.
    fn commit_accepted_input(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.field(key).cloned() else {
            return;
        };
        let value = match self.input_value(&field, cx) {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(error) => {
                self.message = Some(FormMessage::Raw(true, error));
                cx.notify();
                return;
            }
        };
        self.normalize_color_input(key, &value, window, cx);
        if same_value(&value, &field["value"]) && !self.failed_saves.contains(key) {
            return;
        }
        self.enqueue_save(key, value, window, cx);
    }

    fn normalize_color_input(
        &self,
        key: &str,
        value: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .field(key)
            .is_some_and(|field| kind(field) == Kind::Color)
        {
            if let (Some(input), Some(text)) = (self.inputs.get(key), value.as_str()) {
                if input.read(cx).text() != text {
                    input.update(cx, |input, cx| input.set_text(text.into(), window, cx));
                }
            }
        }
    }

    fn save(&mut self, key: &str, value: Value, window: &mut Window, cx: &mut Context<Self>) {
        // A MouseDown before locking can deliver Click after the barrier. Do
        // not rely on render's mouse capture to reject that already queued click.
        if self.leave_locked {
            return;
        }
        self.enqueue_save(key, value, window, cx);
    }

    fn enqueue_save(
        &mut self,
        key: &str,
        value: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(field) = self.fields.iter_mut().find(|field| field["key"] == key) {
            field["value"] = value.clone();
        }
        self.synchronize_slider(key, cx);
        self.message = None;
        cx.notify();
        let host = self.host.clone();
        let queue = self.saves.clone();
        {
            let mut saves = queue.lock().unwrap();
            saves.pending.push_back(FieldSave {
                config_id: self.config_id.clone(),
                key: key.to_string(),
                value,
                generation: self.generation,
                snapshot: self.fields.clone(),
            });
            if saves.running {
                return;
            }
            saves.running = true;
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let request = {
                    let mut saves = queue.lock().unwrap();
                    match saves.pending.pop_front() {
                        Some(request) => request,
                        None => {
                            let (waiters, result) = finish_queue(&mut saves);
                            drop(saves);
                            for sender in waiters {
                                let _ = sender.send(result.clone());
                            }
                            break;
                        }
                    }
                };
                let generation = request.generation;
                let saved_key = request.key.clone();
                let error_key = (request.config_id.clone(), saved_key.clone());
                let snapshot = request.snapshot;
                let updates_language = request.config_id == "global" && request.key == "UiLanguage";
                let mut values = Map::new();
                values.insert(request.key, request.value);
                let save_host = host.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        save_host.settings_form_update(
                        &json!({ "configId": request.config_id, "values": Value::Object(values) }),
                    )
                    })
                    .await;
                let result = result.and_then(|form| form_save_error(&form).map_or(Ok(form), Err));
                {
                    let mut state = queue.lock().unwrap();
                    match &result {
                        Ok(_) => {
                            state.errors.remove(&error_key);
                        }
                        Err(error) => {
                            state.errors.insert(error_key, error.clone());
                        }
                    }
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match result {
                        Ok(mut form) => {
                            this.failed_saves.remove(&saved_key);
                            if this.persistence_error().is_none()
                                && matches!(this.message, Some(FormMessage::SaveFailed(_)))
                            {
                                this.message = None;
                            }
                            if updates_language {
                                set_language(this.host.ui_language());
                                cx.refresh_windows();
                            }
                            merge_save_values(&mut form, &this.fields, &snapshot);
                            // A later whole-page receipt must not erase another
                            // failed rollback or a replacement still in the
                            // shared category FIFO/reconciliation phase.
                            let mut protected = this.failed_saves.clone();
                            if this.tag_undo_needs_reload {
                                protected.insert("FilterItems".into());
                            }
                            merge_protected_reply_values(&mut form, &this.fields, &protected);
                            this.apply_form(form, false, window, cx);
                            // A result can have been assembled before another window
                            // changed the language. Fetch current labels without
                            // touching the values accepted by this save.
                            this.reload_localized_metadata(cx);
                        }
                        Err(error) => {
                            this.failed_saves.insert(saved_key);
                            // Retain the rejected draft and subsequent edits so a
                            // filesystem failure does not silently discard input.
                            this.message = Some(FormMessage::SaveFailed(error));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn pick_folder(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_path(key, Kind::Folder, window, cx);
    }

    fn pick_image_file(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_path(key, Kind::ImageFile, window, cx);
    }

    /// Opens the in-window color popover (or confirms it when the same swatch
    /// is clicked again, like an outside click). The text box is only written
    /// when the popover commits.
    fn pick_color(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.color_popup.as_ref().filter(|session| session.key == key) {
            let popup = session.popup.clone();
            popup.update(cx, |popup, cx| popup.confirm(ColorDismissal::Outside, window, cx));
            return;
        }
        let Some(input) = self.inputs.get(key) else {
            return;
        };
        let (initial, composing) = {
            let input = input.read(cx);
            (input.text().to_string(), input.is_composing())
        };
        let is_color = self.field(key).is_some_and(|field| kind(field) == Kind::Color);
        match open_decision(self.leave_locked, is_color, composing) {
            OpenDecision::Ignore => return,
            OpenDecision::RefuseComposing => {
                self.message = Some(FormMessage::Raw(
                    true,
                    tr(
                        "请先完成输入法组合，再选择颜色。",
                        "Finish composing text before selecting a color.",
                    )
                    .into(),
                ));
                cx.notify();
                return;
            }
            OpenDecision::Open => {}
        }
        let Some(anchor) = self
            .color_anchors
            .get(key)
            .and_then(|anchor| *anchor.lock().unwrap())
        else {
            return;
        };
        let recents = self
            .host
            .recent_colors()
            .iter()
            .filter_map(|color| ArgbColor::parse(color))
            .collect::<Vec<_>>();
        let theme = self.tokens.theme(self.dark).clone();
        let palette = self.palette(&theme);
        let field_key = key.to_string();
        let popup = cx.new(|cx| {
            SettingsColorPopup::new(
                field_key.clone(),
                &initial,
                recents,
                &theme,
                palette,
                anchor,
                window,
                cx,
            )
        });
        let lease = ColorSessionLease {
            generation: self.generation,
            leave_epoch: self.leave_epoch,
            config_id: self.config_id.clone(),
        };
        let trigger_key = key.to_string();
        let subscription =
            cx.subscribe_in(&popup, window, move |this, popup, event, window, cx| {
                let current = this
                    .color_popup
                    .as_ref()
                    .is_some_and(|session| session.popup.entity_id() == popup.entity_id());
                if !current {
                    return;
                }
                // Stale or locked sessions close without committing the draft.
                this.color_popup = None;
                if !lease.is_current(
                    this.generation,
                    this.leave_epoch,
                    &this.config_id,
                    this.leave_locked,
                ) || this
                    .search_keys
                    .as_ref()
                    .is_some_and(|keys| !keys.contains(&trigger_key))
                {
                    cx.notify();
                    return;
                }
                let by = match event {
                    ColorPickerEvent::Chosen { by, .. } | ColorPickerEvent::Dismissed(by) => *by,
                };
                // Enter must not leave a focused swatch behind: the key-up of
                // that Enter would click it and reopen the popover.
                match by {
                    ColorDismissal::Escape => {
                        if let Some(focus) = this.color_focus.get(&trigger_key) {
                            window.focus(focus);
                        }
                    }
                    ColorDismissal::Enter => {
                        if let Some(input) = this.inputs.get(&trigger_key) {
                            window.focus(&input.read(cx).focus_handle());
                        }
                    }
                    ColorDismissal::Outside => {}
                }
                if let ColorPickerEvent::Chosen { key, value, .. } = event {
                    this.commit_chosen_color(key, value, window, cx);
                }
                cx.notify();
            });
        // Mutually exclusive with the choice list.
        self.choice_popup = None;
        self.color_popup = Some(SettingsColorSession {
            key: key.into(),
            popup: popup.clone(),
            _subscription: subscription,
        });
        popup.update(cx, |popup, cx| popup.focus(window, cx));
        cx.notify();
    }

    /// The popover committed a changed draft: write it to the text box and use
    /// the ordinary accepted-input path, then remember it as a recent color.
    fn commit_chosen_color(
        &mut self,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.leave_locked
            || !self
                .field(key)
                .is_some_and(|field| kind(field) == Kind::Color)
        {
            return;
        }
        let Some(input) = self.inputs.get(key) else {
            return;
        };
        if input.read(cx).is_composing() {
            return;
        }
        let text = value.to_string();
        input.update(cx, |input, cx| input.set_text(text, window, cx));
        self.commit_accepted_input(key, window, cx);
        let host = self.host.clone();
        let color = value.to_string();
        cx.background_executor()
            .spawn(async move {
                // Recents are a convenience; a failed write never blocks the save.
                let _ = host.push_recent_color(&color);
            })
            .detach();
    }

    fn clear_image_file(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked
            || !self
                .field(key)
                .is_some_and(|field| kind(field) == Kind::ImageFile)
        {
            return;
        }
        let Some(input) = self.inputs.get(key) else {
            return;
        };
        if input.read(cx).is_composing() {
            return;
        }
        // Keep the slot until its modal returns, but prevent its selected path
        // from undoing this later explicit decision, including an empty retry.
        self.folder_pickers.retire_key(key);
        input.update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.commit_accepted_input(key, window, cx);
        cx.notify();
    }

    fn pick_path(&mut self, key: &str, picker: Kind, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked || !self.field(key).is_some_and(|field| kind(field) == picker) {
            return;
        }
        let Some(input) = self.inputs.get(key) else {
            return;
        };
        let initial = input.read(cx).text().to_string();
        let input_id = input.entity_id();
        let composing = input.read(cx).is_composing();
        #[cfg(windows)]
        let owner = match crate::platform::hwnd(window) {
            Ok(hwnd) => hwnd.0 as isize,
            Err(error) => {
                self.message = Some(FormMessage::Raw(true, error));
                cx.notify();
                return;
            }
        };
        #[cfg(not(windows))]
        let owner = 0;
        let request = match self.host.capture_native_picker_window(owner) {
            Ok(request) => request,
            Err(error) => {
                self.message = Some(FormMessage::Raw(true, error));
                cx.notify();
                return;
            }
        };
        let Some(lease) = self.folder_pickers.accept(key, composing) else {
            if composing {
                self.message = Some(FormMessage::Raw(
                    true,
                    if picker == Kind::ImageFile {
                        tr(
                            "请先完成输入法组合，再选择图片。",
                            "Finish composing text before selecting an image.",
                        )
                    } else {
                        tr(
                            "请先完成输入法组合，再选择文件夹。",
                            "Finish composing text before selecting a folder.",
                        )
                    }
                    .into(),
                ));
                cx.notify();
            }
            return;
        };
        self.pending_pickers += 1;
        // Publish admission before spawning: a provider can be queued or slow
        // to create its modal, and Browse must already show its disabled state.
        cx.notify();
        let host = self.host.clone();
        let generation = self.generation;
        let leave_epoch = self.leave_epoch;
        let initial_draft = initial.clone();
        let key = key.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let provider_lease = lease.clone();
            let worker_request = request.clone();
            let picked = crate::native_dialog::run(move || {
                Ok(run_folder_picker(&provider_lease, || {
                    let params = json!({ "initialPath": initial });
                    if picker == Kind::ImageFile {
                        host.pick_image_file_native_window_guarded(
                            &params, &worker_request, || provider_lease.current(),
                        )
                    } else {
                        host.pick_folder_native_window_guarded(
                            &params, &worker_request, || provider_lease.current(),
                        )
                    }
                }))
            })
            .await
            .unwrap_or_else(|error| Some(Err(error)));
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.folder_pickers.owns(&key, &lease) {
                    return;
                }
                let unchanged = this.inputs.get(&key).is_some_and(|input| {
                    if input.entity_id() != input_id {
                        return false;
                    }
                    let input = input.read(cx);
                    !input.is_composing() && input.text() == initial_draft
                });
                if !lease.current()
                    || this.leave_locked
                    || this.generation != generation
                    || this.leave_epoch != leave_epoch
                    || !this.host.native_picker_window_is_current(&request)
                    || !unchanged
                {
                    this.folder_pickers.finish(&key, &lease);
                    this.pending_pickers = this.pending_pickers.saturating_sub(1);
                    cx.notify();
                    return;
                }
                match picked {
                    Some(Ok(result)) => {
                        if let Some(path) = selected_picker_path(&result) {
                            if let Some(input) = this.inputs.get(&key) {
                                let path = path.to_string();
                                input.update(cx, |input, cx| input.set_text(path, window, cx));
                            }
                            this.commit_accepted_input(&key, window, cx);
                        }
                    }
                    Some(Err(error)) => {
                        this.message = Some(FormMessage::Raw(true, error));
                        cx.notify();
                    }
                    None => {}
                }
                // A selected setting is registered in the existing save queue
                // before this picker ceases to keep the leave barrier pending.
                this.folder_pickers.finish(&key, &lease);
                this.pending_pickers = this.pending_pickers.saturating_sub(1);
                cx.notify();
            });
        })
        .detach();
    }

    fn capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked {
            return;
        }
        let Some(key) = self.capture.clone() else {
            return;
        };
        cx.stop_propagation();
        let keystroke = &event.keystroke;
        let name = keystroke.key.as_str();
        if matches!(
            name,
            "shift" | "control" | "alt" | "platform" | "function" | "capslock"
        ) {
            return;
        }
        let modifiers = &keystroke.modifiers;
        let plain = !(modifiers.control || modifiers.alt || modifiers.shift || modifiers.platform);
        if plain && name == "escape" {
            self.capture = None;
            cx.notify();
            return;
        }
        if plain && matches!(name, "backspace" | "delete") {
            self.capture = None;
            self.save(&key, json!(""), window, cx);
            return;
        }
        if let Some(chord) = hotkey_chord(name, modifiers) {
            self.capture = None;
            self.save(&key, json!(chord), window, cx);
        }
    }

    /// Drops every in-flight transition and hover record. Called wherever the
    /// controls are rebuilt, so no tween outlives its element.
    fn reset_motion(&mut self) {
        self.motion.get_mut().clear();
        self.hovered.clear();
    }

    fn tween(&self, id: &SharedString, track: Track, target: f32, duration: Duration) -> f32 {
        self.motion.borrow_mut().value(
            (id.clone(), track),
            target,
            duration,
            motion::EASE,
            Instant::now(),
        )
    }

    fn hover_progress(&self, id: &SharedString, duration: Duration) -> f32 {
        let target = if self.hovered.contains(id) { 1.0 } else { 0.0 };
        self.tween(id, Track::Hover, target, duration)
    }

    fn hover_listener(
        id: SharedString,
        cx: &Context<Self>,
    ) -> Box<dyn Fn(&bool, &mut Window, &mut App)> {
        Box::new(cx.listener(move |this, hovered: &bool, _, cx| {
            let changed = if *hovered {
                this.hovered.insert(id.clone())
            } else {
                this.hovered.remove(&id)
            };
            if changed {
                cx.notify();
            }
        }))
    }

    fn chip(
        &self,
        id: SharedString,
        label: String,
        active: bool,
        segmented: bool,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = &self.tokens.settings;
        let palette = self.palette(t);
        // No explicit rule in SettingsControls.css: `motion::SURFACE`.
        let selected = self.tween(
            &id,
            Track::Selected,
            if active { 1.0 } else { 0.0 },
            motion::SURFACE,
        );
        // A selected chip keeps the accepted GPUI look: no hover wash.
        let hover = self.hover_progress(&id, motion::SURFACE) * (1.0 - selected);
        let base_bg = if segmented {
            mix_hsla(gpui::transparent_black(), palette.control_hover, selected)
        } else {
            mix_hsla(palette.control, palette.selected, selected)
        };
        let base_border = mix_hsla(palette.control_border, palette.selected_border, selected);
        rsx! {
            <div id={id.clone()} min_h={px(s.chip_height)} max_w_full py={px(3.0)}
                px={px(s.chip_padding_x)} flex items_center flex_none rounded={px(8.0)} border_1 />
        }
            .text_size(px(if self.compact { 12.0 } else { 13.0 }))
            .cursor_pointer()
            .bg(mix_hsla(base_bg, palette.control_hover, hover))
            .border_color(mix_hsla(base_border, palette.control_hover_border, hover))
            .when(segmented, |chip| chip.border_color(gpui::transparent_black())
                .flex_1().rounded(px(5.0))
                .when(active, |chip| chip.font_weight(FontWeight::SEMIBOLD)))
            .text_color(mix_hsla(
                color(&t.text_secondary),
                color(if segmented { &t.text } else { &t.accent }),
                selected,
            ))
            .on_hover(Self::hover_listener(id, cx))
            .child(label)
    }

    fn theme_card(
        &self,
        id: SharedString,
        label: String,
        variant: &str,
        active: bool,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let palette = self.palette(t);
        let selected = self.tween(
            &id, Track::Selected, if active { 1.0 } else { 0.0 }, motion::SURFACE,
        );
        let hover = self.hover_progress(&id, motion::SURFACE) * (1.0 - selected);
        // SettingsControls.css: independent cards, not the generic segmented track.
        rsx! {
            <div id={id.clone()} flex_1 min_w_0 flex flex_col gap={px(6.0)} p={px(8.0)}
                rounded={px(8.0)} border_1
                border_color={mix_hsla(
                mix_hsla(palette.control_border, palette.selected_border, selected),
                color(&t.border_secondary), hover,
            )}
                bg={mix_hsla(
                mix_hsla(palette.control, palette.selected, selected),
                palette.control_hover, hover,
            )}
                text_color={color(&t.text)} cursor_pointer />
        }
            .on_hover(Self::hover_listener(id, cx))
            .child(widgets::theme_thumbnail(variant, t))
            .child(rsx! {
                <div text_size={px(12.0)} font_weight={FontWeight::MEDIUM}>
                    {label}
                </div>
            })
    }

    fn small_button(
        &self,
        id: SharedString,
        label: impl Into<SharedString>,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let palette = self.palette(t);
        // No explicit rule in SettingsControls.css: `motion::SURFACE`.
        let hover = self.hover_progress(&id, motion::SURFACE);
        rsx! {
            <div id={id.clone()} h={px(self.tokens.settings.control_height)} px={px(10.0)} flex
                items_center flex_none rounded={px(8.0)} border_1
                border_color={mix_hsla(
                palette.control_border,
                palette.control_hover_border,
                hover,
            )}
                bg={mix_hsla(palette.control, palette.control_hover, hover)} text_size={px(12.0)}
                text_color={color(&t.text_secondary)} cursor_pointer />
        }
            .on_hover(Self::hover_listener(id, cx))
            .child(label.into())
    }

    fn boxed_input(
        &self,
        input: &Entity<SearchInput>,
        t: &ThemeTokens,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let focused = input.read(cx).focus_handle().is_focused(window);
        let palette = self.palette(t);
        let id = SharedString::from(format!("boxed-input-{}", input.entity_id().as_u64()));
        // Focus and hover use the 140ms `motion::FOCUS` token.
        let focus = self.tween(
            &id,
            Track::Focus,
            if focused { 1.0 } else { 0.0 },
            motion::FOCUS,
        );
        let hover = self.hover_progress(&id, motion::FOCUS);
        // Focused is accent whether or not hovered (the accepted GPUI behavior).
        let border = mix_hsla(
            mix_hsla(
                palette.control_border,
                palette.control_hover_border,
                hover,
            ),
            palette.focus_border,
            focus,
        );
        rsx! {
            <div id={id.clone()} flex flex_1 min_w_0 h={px(self.tokens.settings.control_height)}
                rounded={px(8.0)} border_1 border_color={border}
                bg={mix_hsla(palette.control, palette.control_hover, hover)} />
        }
            .on_hover(Self::hover_listener(id, cx))
            .overflow_hidden()
            .child(input.clone())
    }

    fn control(
        &self,
        field: &Value,
        t: &ThemeTokens,
        stacked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = field["key"].as_str().unwrap_or_default().to_string();
        let s = &self.tokens.settings;
        let palette = self.palette(t);
        if key == "AutoScrollToTopIdleSeconds"
            && !self.fields.iter().any(|field| field["key"] == "AutoScrollToTop" && field["value"] == true)
        {
            // No focusable input while disabled; retained drafts remain in the
            // form and return when the automatic behavior is enabled again.
            let height = self.tokens.list_settings_popover.number_height;
            let step = |label| div().size(px(height - 2.0)).flex_none().flex()
                .items_center().justify_center().child(label);
            return div().h(px(height)).flex().items_center().rounded(px(6.0))
                .border_1().border_color(color(&t.border)).bg(palette.page).opacity(0.45)
                .child(step("−"))
                .child(div().w(px(35.0)).text_center().child(text_value(field)))
                .child(step("+")).into_any_element();
        }
        match kind(field) {
            Kind::Switch => {
                if self.list_settings_popover {
                    return self.list_settings_switch(field, t, cx).into_any_element();
                }
                let on = field["value"].as_bool().unwrap_or(false);
                let knob = (s.switch_height - 6.0).max(1.0);
                let id = SharedString::from(format!("switch-{key}"));
                // `.settings-switch`: background/border 0.15s ease, hover border
                // `--border-primary`; thumb `transform 0.18s ease`.
                let target = if on { 1.0 } else { 0.0 };
                let on_progress = self.tween(&id, Track::On, target, SWITCH_COLOR);
                let thumb_progress = self.tween(&id, Track::Thumb, target, SWITCH_THUMB);
                let hover = self.hover_progress(&id, SWITCH_COLOR);
                // 1px border and 2px padding on both sides of the travel box.
                let travel = (s.switch_width - 6.0 - knob).max(0.0);
                let off_border = mix_hsla(palette.control_border, color(&t.border), hover);
                div()
                    .min_w_0()
                    .flex_none()
                    .id(id.clone())
                    .w(px(s.switch_width))
                    .h(px(s.switch_height))
                    .flex_none()
                    .rounded_full()
                    .border_1()
                    .border_color(mix_hsla(off_border, color(&t.accent), on_progress))
                    .p(px(2.0))
                    .flex()
                    .bg(mix_hsla(color(&t.tertiary), color(&t.accent), on_progress))
                    .cursor_pointer()
                    .on_hover(Self::hover_listener(id, cx))
                    .child(
                        rsx! {
                            <div flex_none relative left={px(lerp(0.0, travel, thumb_progress))}
                                size={px(knob)} rounded_full
                                bg={mix_hsla(
                                color(&t.text_muted),
                                gpui::white(),
                                on_progress,
                            )} />
                        },
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.save(&key, json!(!on), window, cx)
                    }))
                    .into_any_element()
            }
            Kind::Choice => {
                if ordinary_settings_choice(&self.config_id, &key) {
                    let list_popover_choice = self.list_settings_popover
                        && matches!(key.as_str(), "DismissBehavior" | "ClipListExecuteMode");
                    let Some(focus) = self.choice_focus.get(&key) else {
                        return self.read_only(text_value(field), t);
                    };
                    let current = field["value"].as_str().unwrap_or_default();
                    let label = field["selectionItems"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|item| {
                            item["key"]
                                .as_str()
                                .is_some_and(|value| value.eq_ignore_ascii_case(current))
                        })
                        .and_then(|item| item["name"].as_str())
                        .unwrap_or(if current.is_empty() { "—" } else { current });
                    let anchor = self.choice_anchors.get(&key).cloned().unwrap();
                    let popup = self
                        .choice_popup
                        .as_ref()
                        .filter(|session| session.key == key)
                        .map(|session| session.popup.clone());
                    let disabled = self.leave_locked || self.composing(cx);
                    let trigger = self
                        .small_button(SharedString::from(format!("choice-trigger-{key}")), "", t, cx)
                        .when(list_popover_choice, |button| {
                            button.h_auto().min_h(px(self.tokens.list_settings_popover.choice_height))
                                .px(px(10.0)).py(px(6.0)).gap(px(8.0)).rounded(px(6.0))
                                .bg(palette.page).border_color(color(&t.border_secondary))
                        })
                        .relative()
                        .w_full()
                        .min_w_0()
                        .justify_between()
                        .track_focus(focus)
                        .focus(move |style| style.border_color(palette.focus_border))
                        .child(
                            rsx! {
                                <div flex_1 min_w_0 />
                            }
                                .when(list_popover_choice, |label| label.line_height(relative(1.4)))
                                .when(!list_popover_choice, |label| label.whitespace_nowrap().text_ellipsis())
                                .child(label.to_string()),
                        )
                        .child(
                            svg()
                                .path(
                                    self.tokens
                                        .icons
                                        .get("collapse")
                                        .cloned()
                                        .unwrap_or_default(),
                                )
                                .size(px(12.0))
                                .flex_none()
                                .text_color(color(&t.text_muted)),
                        )
                        .child(
                            gpui::canvas(
                                move |bounds, _, cx| {
                                    *anchor.lock().unwrap() = Some(bounds);
                                    if let Some(popup) = &popup {
                                        popup.update(cx, |popup, cx| popup.set_anchor(bounds, cx));
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0()
                            .size_full(),
                        )
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                        .when(disabled, |button| button.opacity(0.55).cursor_default())
                        .when(!disabled, |button| {
                            button.on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_choice(&key, window, cx)
                            }))
                        });
                    return trigger.into_any_element();
                }
                let current = field["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let items = field["selectionItems"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if !self.compact && full_width_settings_choice(&self.config_id, &key) {
                    // app-shell-content includes the sidebar; match its 560px breakpoint.
                    let columns = if f32::from(window.viewport_size().width) <= 560.0 { 2 } else { 4 };
                    return rsx! {
                        <div w_full min_w_0 flex flex_col gap={px(10.0)} />
                    }
                        .children(items.chunks(columns).map(|items| {
                            rsx! {
                                <div w_full min_w_0 flex gap={px(10.0)} />
                            }
                                .children(items.iter().map(|item| {
                                    let value = item["key"].as_str().unwrap_or_default().to_string();
                                    let label = item["name"].as_str().unwrap_or(&value).to_string();
                                    let active = value.eq_ignore_ascii_case(&current);
                                    let key = key.clone();
                                    self.theme_card(
                                        SharedString::from(format!("choice-{key}-{value}")),
                                        label, &value.to_ascii_lowercase(), active, t, cx,
                                    ).on_click(cx.listener(move |this, _, window, cx| {
                                        if !active {
                                            this.save(&key, json!(value), window, cx)
                                        }
                                    }))
                                }))
                        })).into_any_element();
                }
                // English option names need the bounded width of the stacked row.
                let choice_row = if self.compact {
                    rsx! {
                        <div flex min_w_0 flex_wrap gap={px(4.0)} />
                    }
                } else {
                    widgets::segmented(palette)
                };
                choice_row
                    .w_full()
                    .when(!self.compact && !stacked, |row| row.justify_end())
                    .children(items.into_iter().map(|item| {
                        let value = item["key"].as_str().unwrap_or_default().to_string();
                        let label = item["name"].as_str().unwrap_or(&value).to_string();
                        let active = value.eq_ignore_ascii_case(&current);
                        let key = key.clone();
                        self.chip(
                            SharedString::from(format!("choice-{key}-{value}")),
                            label,
                            active,
                            !self.compact,
                            t,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if !active {
                                    this.save(&key, json!(value), window, cx)
                                }
                            },
                        ))
                    }))
                    .into_any_element()
            }
            Kind::Slider => {
                if let Some(slider) = self.sliders.get(&key) {
                    let enabled = !self.leave_locked && !self.slider_composition_active(window, cx);
                    slider.update(cx, |slider, cx| slider.set_enabled(enabled, cx));
                    slider.clone().into_any_element()
                } else {
                    self.read_only(text_value(field), t)
                }
            }
            Kind::Number => {
                if self.list_settings_popover {
                    return self.list_settings_number(field, t, window, cx).into_any_element();
                }
                let presets = field["presets"].as_array().cloned().unwrap_or_default();
                let stepper = |direction: f64, label: &'static str| {
                    let key = key.clone();
                    self.small_button(SharedString::from(format!("step-{key}-{label}")), label, t, cx)
                        .w(px(s.control_height - 2.0))
                        .h(px(s.control_height - 2.0))
                        .border_0()
                        .rounded(px(0.0))
                        .justify_center()
                        .px_0()
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.step_number(&key, direction, window, cx);
                        }))
                };
                let middle = match self.inputs.get(&key) {
                    Some(input) => rsx! {
                        <div min_w_0 flex_none w={px(80.0)}>
                            {input.clone()}
                        </div>
                    },
                    None => rsx! {
                        <div w={px(80.0)} flex justify_center text_size={px(12.0)}
                            text_color={color(&t.text)}>
                            {text_value(field)}
                        </div>
                    },
                };
                let focused = self
                    .inputs
                    .get(&key)
                    .is_some_and(|input| input.read(cx).focus_handle().is_focused(window));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .when(!self.compact && !stacked, |col| col.items_end())
                    .child(
                        rsx! {
                            <div flex items_center h={px(s.control_height)} rounded={px(8.0)}
                                border_1 />
                        }
                            .border_color(if focused {
                                color(&t.accent)
                            } else {
                                palette.control_border
                            })
                            .bg(palette.control)
                            .overflow_hidden()
                            .child(stepper(-1.0, "−"))
                            .child(middle)
                            .child(stepper(1.0, "+")),
                    )
                    .when(!presets.is_empty(), |col| {
                        col.child(
                            div().flex().flex_wrap().gap(px(4.0)).children(
                                presets
                                    .into_iter()
                                    .filter_map(|preset| preset.as_f64())
                                    .map(|preset| {
                                        let key = key.clone();
                                        let field = field.clone();
                                        let value = number_json(&field, preset);
                                        let active = same_value(&value, &field["value"]);
                                        self.chip(
                                            SharedString::from(format!("preset-{key}-{preset}")),
                                            text_value(&json!({ "value": value.clone() })),
                                            active,
                                            false,
                                            t,
                                            cx,
                                        )
                                        .on_mouse_down(MouseButton::Left, |_, window, _| {
                                            window.prevent_default()
                                        })
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.accept_number(&key, value.clone(), window, cx)
                                            }),
                                        )
                                    }),
                            ),
                        )
                    })
                    .into_any_element()
            }
            Kind::Color => match (self.inputs.get(&key), self.color_focus.get(&key)) {
                (Some(input), Some(focus)) => {
                    let disabled = self.leave_locked;
                    let anchor = self.color_anchors.get(&key).cloned();
                    let popup = self
                        .color_popup
                        .as_ref()
                        .filter(|session| session.key == key)
                        .map(|session| session.popup.clone());
                    let swatch = rsx! {
                        <div relative size_full flex_none rounded={px(5.0)} overflow_hidden>
                            {checker(10.0, 3.0)}
                        </div>
                    }
                        .when_some(argb_color(input.read(cx).text()), |swatch, fill| {
                            swatch.child(div().absolute().size_full().bg(fill))
                        });
                    rsx! {
                        <div flex items_center gap={px(8.0)} w={px(188.0)} max_w_full min_w_0 />
                    }
                        .child(
                            self.small_button(
                                SharedString::from(format!("pick-color-{key}")),
                                "",
                                t,
                                cx,
                            )
                            .relative()
                            .size(px(32.0))
                            .p_0()
                            .rounded(px(6.0))
                            .tooltip(settings_help(tr("选色…", "Choose color…").into(), t.clone()))
                            .child(swatch)
                            .child(
                                // Anchor for the popover, as for the choice trigger.
                                gpui::canvas(
                                    move |bounds, _, cx| {
                                        if let Some(anchor) = &anchor {
                                            *anchor.lock().unwrap() = Some(bounds);
                                        }
                                        if let Some(popup) = &popup {
                                            popup.update(cx, |popup, cx| {
                                                popup.set_anchor(bounds, cx)
                                            });
                                        }
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            )
                            .track_focus(focus)
                            .focus(move |style| style.border_color(palette.focus_border))
                            .on_mouse_down(MouseButton::Left, |_, window, _| {
                                window.prevent_default()
                            })
                            .when(disabled, |button| button.opacity(0.55).cursor_default())
                            .when(!disabled, |button| {
                                button.on_click(cx.listener(move |this, _, window, cx| {
                                    this.pick_color(&key, window, cx)
                                }))
                            }),
                        )
                        .child(self.boxed_input(input, t, window, cx).h(px(32.0)))
                        .into_any_element()
                }
                _ => self.read_only(text_value(field), t),
            },
            Kind::Text => match self.inputs.get(&key) {
                Some(input) => rsx! {
                    <div flex items_center gap={px(6.0)} w_full min_w_0>
                        {self.boxed_input(input, t, window, cx)}
                    </div>
                }
                    .into_any_element(),
                None => self.read_only(text_value(field), t),
            },
            Kind::Folder | Kind::ImageFile => match self.inputs.get(&key) {
                Some(input) => {
                    let image = kind(field) == Kind::ImageFile;
                    let picking = self.folder_pickers.is_pending(&key);
                    let disabled = !self.folder_pickers.browse_enabled(&key, self.leave_locked);
                    let clear_disabled = self.leave_locked
                        || input.read(cx).is_composing()
                        || (input.read(cx).text().trim().is_empty()
                            && !self.failed_saves.contains(&key));
                    let clear_key = key.clone();
                    rsx! {
                        <div flex flex_wrap items_center gap={px(6.0)} w_full min_w_0>
                            {self.boxed_input(input, t, window, cx).min_w(px(170.0))}
                        </div>
                    }
                        .child(
                            self.small_button(
                                SharedString::from(format!("pick-{key}")),
                                if picking {
                                    tr("选择中…", "Choosing…")
                                } else {
                                    tr("浏览…", "Browse…")
                                },
                                t,
                                cx,
                            )
                            .when(disabled, |button| button.opacity(0.55).cursor_default())
                            .when(!disabled, |button| {
                                button.on_click(cx.listener(move |this, _, window, cx| {
                                    if image {
                                        this.pick_image_file(&key, window, cx)
                                    } else {
                                        this.pick_folder(&key, window, cx)
                                    }
                                }))
                            }),
                        )
                        .when(image, |row| {
                            row.child(
                                self.small_button(
                                    SharedString::from(format!("clear-{clear_key}")),
                                    tr("清除", "Clear"),
                                    t,
                                    cx,
                                )
                                .when(clear_disabled, |button| {
                                    button.opacity(0.55).cursor_default()
                                })
                                .when(!clear_disabled, |button| {
                                    button.on_click(cx.listener(move |this, _, window, cx| {
                                        this.clear_image_file(&clear_key, window, cx)
                                    }))
                                }),
                            )
                        })
                        .into_any_element()
                }
                None => self.read_only(text_value(field), t),
            },
            Kind::Hotkey => {
                let capturing = self.capture.as_deref() == Some(key.as_str());
                let value = text_value(field);
                let label = if capturing {
                    tr(
                        "请按下快捷键（Esc 取消，Backspace 清除）",
                        "Press a shortcut (Esc cancels, Backspace clears)",
                    )
                    .to_string()
                } else if value.is_empty() {
                    tr("未设置", "Not set").to_string()
                } else {
                    hotkey_display(&value)
                };
                let start_key = key.clone();
                let hotkey_id = SharedString::from(format!("hotkey-{key}"));
                let hover = self.hover_progress(&hotkey_id, motion::HOVER);
                rsx! {
                    <div id={hotkey_id.clone()} min_h={px(s.control_height)} w_full min_w_0
                        px={px(10.0)} py={px(5.0)} flex items_center justify_center
                        rounded={px(8.0)} border_1 />
                }
                    .border_color(mix_hsla(
                        if capturing {
                            color(&t.accent)
                        } else {
                            palette.control_border
                        },
                        if capturing {
                            palette.focus_border
                        } else {
                            palette.control_hover_border
                        },
                        hover,
                    ))
                    .bg(mix_hsla(palette.control, palette.control_hover, hover))
                    .text_size(px(12.0))
                    .text_color(color(if capturing { &t.accent } else { &t.text }))
                    .line_height(relative(1.4))
                    .text_center()
                    .on_hover(Self::hover_listener(hotkey_id, cx))
                    .cursor_pointer()
                    .when(capturing, |el| {
                        el.track_focus(&self.capture_focus)
                            .on_key_down(cx.listener(Self::capture_key))
                    })
                    .child(rsx! {
                        <div flex_1 min_w_0>
                            {label}
                        </div>
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.leave_locked {
                            return;
                        }
                        this.capture = Some(start_key.clone());
                        window.focus(&this.capture_focus);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Kind::Lines => {
                let rows = self.lines.get(&key).cloned().unwrap_or_default();
                let add_key = key.clone();
                let can_drag = !self.leave_locked && !self.composition_active(cx);
                rsx! {
                    <div flex flex_col gap={px(4.0)} w_full />
                }
                    .children(rows.iter().enumerate().map(|(index, input)| {
                        let key = key.clone();
                        let input_id = input.entity_id().as_u64();
                        let drag_key = key.clone();
                        let drop_key = key.clone();
                        let up_key = key.clone();
                        let down_key = key.clone();
                        let drag = SettingsLineDrag {
                            owner: cx.entity_id().as_u64(), config_id: self.config_id.clone(),
                            generation: self.generation, field: key.clone(), input: input_id,
                            title: if input.read(cx).text().trim() == "---" { tr("分隔线", "Separator").into() }
                                else { input.read(cx).text().chars().take(80).collect() },
                            background: color(&t.surface), foreground: color(&t.text),
                        };
                        let drag_owner = cx.weak_entity();
                        let drop = self.line_drop_target.as_ref().filter(|(field, id, _)| field == &key && *id == input_id).map(|(_, _, before)| *before);
                        rsx! {
                            <div id={SharedString::from(format!("line-row-{key}-{input_id}"))}
                                flex items_center gap={px(4.0)} />
                        }
                            .when(drop == Some(true), |row| row.border_t_2().border_color(color(&t.accent)))
                            .when(drop == Some(false), |row| row.border_b_2().border_color(color(&t.accent)))
                            .on_drag_move(cx.listener(move |this, event: &gpui::DragMoveEvent<SettingsLineDrag>, _, cx| {
                                if !event.bounds.contains(&event.event.position) || this.leave_locked || this.composition_active(cx) { return; }
                                let source = event.dragged_item().downcast_ref::<SettingsLineDrag>().expect("settings line drag");
                                if !source.belongs_to(cx.entity_id().as_u64(), &this.config_id, this.generation, &drag_key) { return; }
                                let before = event.event.position.y < event.bounds.top() + event.bounds.size.height / 2.0;
                                let target = Some((drag_key.clone(), input_id, before));
                                if this.line_drop_target != target { this.line_drop_target = target; cx.notify(); }
                            }))
                            .on_drop(cx.listener(move |this, drag: &SettingsLineDrag, window, cx| {
                                this.line_dragging = false;
                                if !drag.belongs_to(cx.entity_id().as_u64(), &this.config_id, this.generation, &drop_key) { return; }
                                let Some((_, _, before)) = this.line_drop_target.take().filter(|(field, id, _)| field == &drop_key && *id == input_id) else { return; };
                                this.reorder_line(&drop_key, drag.input, input_id, before, window, cx);
                            }))
                            .child(
                                self.small_button(SharedString::from(format!("line-grip-{key}-{input_id}")), "⠿", t, cx)
                                    .cursor(gpui::CursorStyle::OpenHand)
                                    .when(can_drag, |handle| handle.on_drag(drag, move |drag: &SettingsLineDrag, _, _, cx| {
                                        let _ = drag_owner.update(cx, |this, cx| {
                                            if !this.leave_locked && !this.composition_active(cx) && drag.generation == this.generation {
                                                this.line_dragging = true;
                                                this.line_drop_target = None;
                                                cx.notify();
                                            }
                                        });
                                        cx.new(|_| drag.clone())
                                    })),
                            )
                            .child(rsx! {
                                <div flex_1 min_w_0>
                                    {self.boxed_input(input, t, window, cx)}
                                </div>
                            })
                            .child(self.small_button(SharedString::from(format!("line-up-{key}-{input_id}")), "↑", t, cx)
                                .when(index == 0, |button| button.opacity(0.4))
                                .on_click(cx.listener(move |this, _, window, cx| this.move_line(&up_key, input_id, -1, window, cx))))
                            .child(self.small_button(SharedString::from(format!("line-down-{key}-{input_id}")), "↓", t, cx)
                                .when(index + 1 == rows.len(), |button| button.opacity(0.4))
                                .on_click(cx.listener(move |this, _, window, cx| this.move_line(&down_key, input_id, 1, window, cx))))
                            .child(
                                self.small_button(
                                    SharedString::from(format!("line-remove-{key}-{input_id}")),
                                    tr("删除", "Remove"),
                                    t,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        if this.leave_locked {
                                            return;
                                        }
                                        if let Some(rows) = this.lines.get_mut(&key) {
                                            if let Some(index) = rows.iter().position(|row| row.entity_id().as_u64() == input_id) {
                                                rows.remove(index);
                                            }
                                        }
                                        this.commit(&key, window, cx);
                                        cx.notify();
                                    },
                                )),
                            )
                    }))
                    .child(
                        div().flex().child(
                            self.small_button(
                                SharedString::from(format!("line-add-{key}")),
                                tr("添加一行", "Add a row"),
                                t,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if this.leave_locked {
                                        return;
                                    }
                                    let input =
                                        this.make_input(&add_key, String::new(), window, cx);
                                    input.read(cx).focus_handle().focus(window);
                                    this.lines.entry(add_key.clone()).or_default().push(input);
                                    cx.notify();
                                },
                            )),
                        ),
                    )
                    .into_any_element()
            }
            Kind::Tags => match &self.tag_editor {
                Some((editor, _)) => rsx! {
                    <div w_full min_w_0 flex_none>
                        {editor.clone()}
                    </div>
                }
                    .into_any_element(),
                None => self.read_only(
                    tr(
                        "在分类栏「自定义」中编辑",
                        "Edit using Customize in the category bar",
                    )
                    .into(),
                    t,
                ),
            },
            Kind::External => self.read_only(text_value(field), t),
        }
    }

    fn read_only(&self, text: String, t: &ThemeTokens) -> AnyElement {
        rsx! {
            <div max_w={px(self.tokens.settings.control_width)} text_size={px(12.0)}
                text_color={color(&t.text_muted)} text_ellipsis overflow_hidden>
                {text}
            </div>
        }
            .into_any_element()
    }

    fn message_text(&self) -> Option<(bool, String)> {
        Some(match self.message.as_ref()? {
            FormMessage::Raw(error, text) | FormMessage::Schema(error, text) => {
                (*error, text.clone())
            }
            FormMessage::TagsSaved => {
                (false, tr("分类标签已保存。", "Category tabs saved.").into())
            }
            FormMessage::SaveFailed(error) => (
                true,
                format!("{}{error}", tr("保存失败：", "Save failed: ")),
            ),
        })
    }

    fn row(
        &self,
        field: &Value,
        first: bool,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        if self.list_settings_popover {
            return self.list_settings_row(field, t, window, cx);
        }
        let s = &self.tokens.settings;
        let palette = self.palette(t);
        let description = field["description"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        let geometry = SettingsGeometry::new(f32::from(window.viewport_size().width), s);
        let wide = (self.compact && kind(field) != Kind::Switch)
            || matches!(
                kind(field),
                Kind::Text
                    | Kind::Slider
                    | Kind::Folder
                    | Kind::ImageFile
                    | Kind::Lines
                    | Kind::Tags
            )
            || (geometry.stack_controls && matches!(kind(field), Kind::Choice | Kind::Hotkey | Kind::Number))
            || (kind(field) == Kind::Color && geometry.row_inner_width < 320.0)
            || (kind(field) == Kind::Choice
                && full_width_settings_choice(
                    &self.config_id,
                    field["key"].as_str().unwrap_or_default(),
                ))
            || (kind(field) == Kind::Choice && displayed_language() == LanguageMode::English);
        let inline_description = inline_settings_description(
            &self.config_id,
            field["key"].as_str().unwrap_or_default(),
            self.compact,
        );
        let field_label = field["label"].as_str().unwrap_or_default().to_string();
        // 给标签保留可收缩的宽度，短标题的问号紧跟文字，长标题仍可换行。
        let label_width = crate::view::label_width(
            &field_label,
            if self.compact { 13.0 } else { 14.0 },
            &self.tokens.typography.font_family,
            window,
        ).ceil() + 1.0;
        let title = widgets::label_column(wide)
            .child(
                widgets::label(self.compact, t)
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "field-label-{}-{}",
                                self.config_id,
                                field["key"].as_str().unwrap_or_default()
                            )))
                            .flex_1()
                            .min_w_0()
                            .when(!self.compact, |label| label.max_w(px(label_width)))
                            .child(field_label)
                            .when(self.compact && !description.is_empty(), |label| {
                                label.tooltip(settings_help(description.clone(), t.clone()))
                            }),
                    )
                    .when(!self.compact && !description.is_empty() && !inline_description, |title| {
                        title.child(
                            rsx! {
                                <div id={SharedString::from(format!(
                                    "field-help-{}-{}",
                                    self.config_id,
                                    field["key"].as_str().unwrap_or_default()
                                ))}
                                    size={px(16.0)} flex_none flex items_center justify_center
                                    rounded_full border_1 border_color={palette.control_border}
                                    text_size={px(11.0)} text_color={color(&t.text_muted)}
                                    cursor_pointer>
                                    {"?"}
                                </div>
                            }
                                .tooltip(settings_help(description.clone(), t.clone())),
                        )
                    }),
            )
            .when(!description.is_empty() && inline_description, |col| {
                col.child(
                    widgets::description(palette)
                        .children(
                            description
                                .lines()
                                .map(|line| div().flex_none().child(line.to_string())),
                        ),
                )
            });
        let control = self.control(field, t, wide, window, cx);
        // Dedicated pages only; the compact popover has no row hover.
        let row_id = SharedString::from(format!(
            "settings-row-{}-{}",
            self.config_id,
            field["key"].as_str().unwrap_or_default()
        ));
        let row_hover = if self.compact {
            0.0
        } else {
            self.hover_progress(&row_id, motion::HOVER)
        };
        widgets::row(wide, self.compact, s)
            .id(row_id.clone())
            .bg(mix_hsla(
                gpui::transparent_black(),
                palette.row_hover,
                row_hover,
            ))
            .when(!self.compact, |row| {
                row.on_hover(Self::hover_listener(row_id, cx))
            })
            .when(self.compact && !first, |row| {
                row.border_t_1().border_color(palette.row_border)
            })
            .child(title)
            .child(
                div()
                    .min_w_0()
                    .flex_none()
                    .when(wide, |c| c.w_full())
                    .when(!wide && kind(field) != Kind::Switch, |c| {
                        c.w(px(if kind(field) == Kind::Color {
                            188.0
                        } else {
                            geometry.inline_control_width
                        }))
                    })
                    .child(control),
            )
    }
}

/// `#AARRGGBB` or `#RRGGBB` from WPF color settings.
fn argb_color(value: &str) -> Option<gpui::Hsla> {
    let color = ArgbColor::parse(value)?;
    let mut fill: gpui::Hsla = gpui::rgb(color.rgb()).into();
    fill.a = f32::from(color.a) / 255.0;
    Some(fill)
}

impl Render for SettingsForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.line_dragging = false;
            self.line_drop_target = None;
        }
        if let Some(actions) = &self.text_actions {
            actions.update(cx, |actions, cx| actions.set_theme(self.dark, cx));
            let mut root = rsx! {
                <div w_full flex flex_col gap={px(8.0)} />
            };
            if self.leave_locked {
                root = root.capture_any_mouse_down(|_, _, cx| cx.stop_propagation());
            }
            if let Some((error, text)) = self.message_text() {
                let t = self.tokens.theme(self.dark);
                root = root.child(
                    div()
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_color(color(if error { &t.error } else { &t.text_muted }))
                        .child(text),
                );
            }
            return root.child(actions.clone()).into_any_element();
        }
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark);
        let s = &tokens.settings;
        let palette = self.palette(t);
        let mut root = rsx! {
            <div id={"settings-form"} w_full min_w_0 flex_none flex flex_col />
        }
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.line_dragging && event.keystroke.key == "escape" {
                    this.key_down(event, window, cx);
                }
            }))
            .on_key_down(cx.listener(Self::key_down))
            .gap(px(if self.compact { 0.0 } else { 20.0 }));
        if self.leave_locked {
            root = root
                .capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .child(
                    rsx! {
                        <div text_size={px(12.0)} text_color={color(&t.text_muted)}>
                            {tr("正在保存设置…", "Saving settings…")}
                        </div>
                    },
                );
        }
        if let Some((error, text)) = self.message_text() {
            root = root.child(
                rsx! {
                    <div px={px(12.0)} py={px(8.0)} rounded={px(6.0)} border_1 />
                }
                    .border_color(color(if error { &t.error } else { &t.accent }))
                    .text_size(px(12.0))
                    .text_color(color(if error { &t.error } else { &t.text_secondary }))
                    .child(text),
            );
        }
        let sectioned = self.config_id == "menu_global"
            && self.super_menu_sections.is_some() && self.search_keys.is_none();
        let mut launcher = None;
        if self.config_id == "menu_global" {
            launcher = Some(
                rsx! {
                    <div w_full min_w_0 flex_none flex flex_col items_start gap={px(8.0)} />
                }
                    .child(
                        self.small_button(
                            "settings-test-super-menu".into(),
                            tr("打开超级菜单", "Open Super menu"),
                            t,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.message = this.host.request_super_menu().err().map(|error| {
                                FormMessage::Raw(true, error)
                            });
                            cx.notify();
                        })),
                    )
                    .child(widgets::description(palette).child(tr(
                        "可直接打开测试；独立唤起的全局快捷键可在“触发”设置中修改。",
                        "Open it here to test. Change its global shortcut in Trigger settings.",
                    )))
                .into_any_element(),
            );
        }
        if !sectioned {
            root = root.children(launcher.take());
        } else {
            root = root.gap(px(28.0));
        }
        if self.fields.is_empty() && self.message.is_none() {
            return root.child(
                div()
                    .py(px(24.0))
                    .text_size(px(12.0))
                    .text_color(color(&t.text_muted))
                    .child(if self.config_id == "textAction" {
                        tr("文本操作页尚未迁移到原生界面，可暂用保留的 Tauri 版本。", "Text actions are not available in the native interface yet. Use the retained Tauri version for now.")
                    } else {
                        tr("此页没有可调整的设置。", "There are no settings to adjust on this page.")
                    }),
            ).into_any_element();
        }
        // Groups keep the server's field order; consecutive fields share a card.
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for field in self.display_fields() {
            if self
                .search_keys
                .as_ref()
                .is_some_and(|keys| !keys.contains(field["key"].as_str().unwrap_or_default()))
            {
                continue;
            }
            let group = if sectioned {
                super_menu_field_section(field["key"].as_str().unwrap_or_default()).1.to_string()
            } else {
                field["group"].as_str().unwrap_or_default().to_string()
            };
            match groups.last_mut() {
                Some((name, items)) if *name == group => items.push(field.clone()),
                _ => groups.push((group, vec![field.clone()])),
            }
        }
        for (group, fields) in groups {
            let quick_paste_help = self.config_id == "quickPaste"
                && !self.compact && self.search_keys.is_none()
                && fields.iter().any(|field| field["key"] == "ListLimit");
            if self.list_settings_popover {
                match fields.first().and_then(|field| field["key"].as_str()) {
                    Some("EnableNoFocusMode") => {
                        root = root.child(self.list_settings_caption(tr("窗口", "Window"), t));
                    }
                    Some("ClipListExecuteMode") => {
                        root = root.child(self.list_settings_divider(t))
                            .child(self.list_settings_caption(tr("列表操作", "List actions"), t));
                    }
                    _ => {}
                }
            }
            let rows: Vec<AnyElement> = fields.iter().enumerate()
                .map(|(index, field)| self.row(field, index == 0, t, window, cx).into_any_element())
                .collect();
            if self.compact {
                root = root.child(rsx! {
                    <div w_full min_w_0 flex_none flex flex_col />
                }.children(rows));
            } else if sectioned {
                let title = super_menu_field_section(fields[0]["key"].as_str().unwrap_or_default()).2;
                let mut section = widgets::section(title, s, t).relative()
                    .child(self.super_menu_sections.as_ref().unwrap().section_marker(&group));
                if group == "general" {
                    section = section.children(launcher.take());
                }
                root = root.child(section.child(widgets::card(rows, s, palette)));
            } else {
                root = root.child(widgets::section(group, s, t).child(widgets::card(rows, s, palette)));
            }
            if quick_paste_help {
                root = root.child(quick_paste_help::shortcuts(s, t, palette));
            }
        }
        if self.motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        root.children(
            self.choice_popup
                .as_ref()
                .map(|session| session.popup.clone()),
        )
        .children(self.color_popup.as_ref().map(|session| session.popup.clone()))
        .into_any_element()
    }
}

pub struct SettingsWindow {
    host: Arc<HostState>,
    tokens: DesignTokens,
    base_tokens: DesignTokens,
    background_snapshot: Option<Arc<PreparedBackground>>,
    background_options: WindowBackgroundOptions,
    surface_palettes: Option<(SettingsPalette, SettingsPalette)>,
    dark: bool,
    pages: Vec<(String, String)>,
    active: String,
    form: Entity<SettingsForm>,
    forms: HashMap<String, Entity<SettingsForm>>,
    form_observers: HashMap<String, Subscription>,
    form_tab_subscriptions: HashMap<String, Subscription>,
    data_page: Option<Entity<ManagementPage>>,
    data_observer: Option<Subscription>,
    reopen_page: Option<Entity<crate::reopen::ReopenWindow>>,
    reopen_observer: Option<Subscription>,
    lan_page: Option<Entity<LanManagementPage>>,
    lan_subscriptions: Vec<Subscription>,
    device_sync_page: Option<Entity<DeviceSyncSettingsPage>>,
    device_sync_observer: Option<Subscription>,
    #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
    webdav_page: Option<Entity<WebDavSettingsPage>>,
    #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
    webdav_observer: Option<Subscription>,
    update_page: Option<Entity<UpdateSettingsPage>>,
    update_observer: Option<Subscription>,
    home_page: Option<Entity<HomeSettingsPage>>,
    home_subscriptions: Vec<Subscription>,
    schemas: HashMap<String, Value>,
    search_index: SettingsSearchIndex,
    search_query: SettingsSearchQuery,
    search_results: Vec<SettingsSearchMatch>,
    deferred_search: DeferredSettingsSearch,
    search_input: Entity<SearchInput>,
    _search_subscription: Subscription,
    schema_task: Option<Task<()>>,
    schema_epoch: u64,
    schema_loading: bool,
    schema_complete: bool,
    schema_errors: HashMap<String, String>,
    window_message: Option<String>,
    scroll: ScrollHandle,
    scrollbar: ScrollbarState,
    _language_watch: Task<()>,
    leave: Option<SettingsLeave>,
    leave_task: Option<Task<()>>,
    close_waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
    pending_route: Option<String>,
    pending_system_anchor: bool,
    super_menu_sections: SuperMenuSections,
    super_menu_actions_expanded: bool,
    text_actions_manager: bool,
    closed: bool,
    /// Chrome hover transitions (close, navigation, undo, search); display only.
    motion: RefCell<Tweens<(SharedString, Track)>>,
    hovered: HashSet<SharedString>,
}

#[derive(Clone)]
enum SettingsLeave {
    Select { page: String, section: Option<&'static str> },
    Close { dismiss: bool },
}

impl SettingsWindow {
    /// Read-only metadata for the existing explicit --report QA mode; form
    /// field values and editable drafts are never serialized here.
    pub fn diagnostics(&self, cx: &App) -> Value {
        let matches: Vec<_> = self
            .search_results
            .iter()
            .map(|result| {
                let mut fields: Vec<_> = result.field_keys.iter().cloned().collect();
                fields.sort();
                json!({"configId":result.config_id,"label":result.label,"fieldKeys":fields})
            })
            .collect();
        json!({
            "activeCategory":self.active,
            "textActionsManager":self.text_actions_manager,
            "query":self.search_query.query(),
            "inputQuery":self.search_input.read(cx).text(),
            "scope":self.search_query.scope(),
            "schemaLoading":self.schema_loading,
            "schemaComplete":self.schema_complete,
            "schemaErrors":self.schema_errors,
            "cachedForms":self.forms.len(),
            "dataPageRetained":self.data_page.is_some(),
            "dataPageBusy":self.data_projection_busy(cx),
            "dataPageComposing":self.data_page.as_ref().is_some_and(|page| page.read(cx).is_composing(cx)),
            "devicesPageRetained":self.lan_page.is_some(),
            "deviceSyncPageRetained":self.device_sync_page.is_some(),
            "homePageRetained":self.home_page.is_some(),
            "deviceSync":self.device_sync_page.as_ref().map(|page| page.read(cx).diagnostics()),
            "reopen":self.reopen_page.as_ref().map(|page| page.read(cx).diagnostics(cx)),
            "update":self.update_page.as_ref().map(|page| page.read(cx).diagnostics()),
            "matchCount":self.search_results.iter().map(SettingsSearchMatch::count).sum::<usize>(),
            "matches":matches,
            "pendingWrites":self.pending_writes(cx),
            "searchComposing":self.search_input.read(cx).is_composing(),
            "leaving":self.leave.is_some(),
            "closed":self.closed,
            "canUndo":self.can_undo(cx),
            "message":self.window_message,
            "superMenu":self.super_menu_sections.diagnostics(),
            "formEntityIds":self.forms.iter().map(|(id, form)| (id.clone(), json!(form.entity_id().as_u64()))).collect::<Map<String,Value>>(),
            "superMenuActionsExpanded":self.super_menu_actions_expanded,
            "sidebarPages":self.pages.iter().filter(|(id,_)| sidebar_settings_page(id)).map(|(id,_)| id).collect::<Vec<_>>(),
        })
    }

    /// Only disposable, headless visual fixtures may move the settings
    /// viewport directly. Call opening on a subsequent rendered frame.
    #[cfg(debug_assertions)]
    pub fn set_settings_visual_scroll(&mut self, scroll_y: f32, cx: &mut Context<Self>) {
        assert!(cliphost_core::is_headless());
        assert!(scroll_y.is_finite() && scroll_y >= 0.0);
        self.scroll.set_offset(point(px(0.0), px(-scroll_y)));
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub fn open_settings_choice_visual(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        assert!(cliphost_core::is_headless());
        if self.schema_loading || self.closed || self.leave.is_some() {
            return false;
        }
        self.visible_forms().iter().any(|form| {
            form.update(cx, |form, cx| form.open_settings_choice_visual(key, window, cx))
        })
    }

    #[cfg(debug_assertions)]
    pub fn settings_choice_visual_diagnostics(&self, cx: &App) -> Value {
        assert!(cliphost_core::is_headless());
        let popup = self.visible_forms().iter().find_map(|form| {
            form.read(cx).choice_popup.as_ref().map(|session| {
                session.popup.read(cx).geometry_diagnostics()
            })
        });
        json!({
            "scrollOffsetY": f32::from(self.scroll.offset().y),
            "formsReady": self.visible_forms().iter().all(|form| form.read(cx).schema_initialized)
                && (self.active != DEVICES_PAGE || self.device_sync_page.as_ref().is_some_and(|page| page.read(cx).ready())),
            "popup": popup,
        })
    }

    fn visible_forms(&self) -> Vec<Entity<SettingsForm>> {
        if self.search_query.is_active() {
            self.search_results
                .iter()
                .filter(|result| {
                    settings_scope_includes(self.search_query.scope(), &result.config_id)
                })
                .filter_map(|result| self.forms.get(&result.config_id).cloned())
                .collect()
        } else {
            if native_settings_page(&self.active) {
                return Vec::new();
            }
            let mut forms = vec![self.form.clone()];
            if self.active == "global" {
                if let Some(system) = self.forms.get(SYSTEM_PAGE) {
                    forms.push(system.clone());
                }
            }
            if self.active == "menu_global" {
                if let Some(file) = self.forms.get("menu_file") {
                    forms.push(file.clone());
                }
                if self.super_menu_actions_expanded {
                    if let Some(actions) = self.forms.get("textAction") {
                        forms.push(actions.clone());
                    }
                }
            }
            forms
        }
    }

    fn move_tab_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed
            || self.leave.is_some()
            || self.search_input.read(cx).is_composing()
            || self.data_projection_busy(cx)
            || self
                .visible_forms()
                .iter()
                .any(|form| form.read(cx).tab_locked(cx))
        {
            return;
        }
        let mut handles = vec![self.search_input.read(cx).focus_handle()];
        if !self.search_query.is_active() && self.active == "menu_global" {
            handles.extend(self.super_menu_sections.focus_handles());
        }
        for form in self.visible_forms() {
            handles.extend(form.read(cx).tab_focus_handles(cx));
        }
        if !self.search_query.is_active() && self.active == DATA_PAGE {
            if let Some(page) = &self.data_page {
                handles.push(page.read(cx).data_focus_handle(cx));
            }
        }
        if !self.search_query.is_active() && self.active == "user" {
            if let Some(page) = &self.reopen_page { handles.extend(page.read(cx).focus_handles(cx)); }
        }
        if !self.search_query.is_active() && self.active == DEVICES_PAGE {
            if let Some(page) = &self.lan_page {
                page.update(cx, |page, cx| page.keyboard_navigation(cx));
                handles.extend(page.read(cx).focus_handles(cx));
            }
            if let Some(page) = &self.device_sync_page {
                handles.extend(page.read(cx).focus_handles());
            }
            #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
            if let Some(page) = &self.webdav_page {
                handles.extend(page.read(cx).focus_handles());
            }
        }
        if (!self.search_query.is_active() && self.active == UPDATE_PAGE)
            || self.search_results.iter().any(|item| item.config_id == UPDATE_PAGE && settings_scope_includes(self.search_query.scope(), UPDATE_PAGE)) {
            if let Some(page) = &self.update_page {
                handles.extend(page.read(cx).focus_handles());
            }
        }
        let current = handles.iter().position(|handle| handle.is_focused(window));
        if let Some(next) = next_settings_focus(current, handles.len(), reverse) {
            window.focus(&handles[next]);
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_input.read(cx).is_composing()
            || self.data_page.as_ref().is_some_and(|page| page.read(cx).is_composing(cx))
            || self.lan_page.as_ref().is_some_and(|page| page.read(cx).composition_active(cx))
            || self.reopen_page.as_ref().is_some_and(|page| page.read(cx).is_composing(cx))
            || self
                .visible_forms()
                .iter()
                .any(|form| form.read(cx).composition_active(cx))
        {
            return;
        }
        if self.active == "user" && !self.search_query.is_active() {
            if let Some(page) = &self.reopen_page {
                if page.read(cx).has_overlay() {
                    page.update(cx, |page, cx| page.overlay_key_down(event, window, cx));
                    return;
                }
            }
        }
        if let Some(reverse) = settings_tab_direction(event) {
            window.prevent_default();
            cx.stop_propagation();
            self.move_tab_focus(reverse, window, cx);
            return;
        }
        // Browser-style paging of the content (user 2026-10-15). Sliders and
        // the hotkey recorder consume their own keys; text areas stop them.
        if let Some(down) = settings_page_direction(event) {
            if self.page_keys_blocked(window, cx) {
                return;
            }
            self.cancel_super_menu_section_jump(cx);
            let viewport = f32::from(self.scroll.bounds().size.height);
            let step = smooth_wheel::page_step(viewport);
            let delta = if down { step } else { -step };
            if smooth_wheel::glide_by("settings-content", &self.scroll, delta, window, cx) {
                window.prevent_default();
                cx.stop_propagation();
            }
        }
    }

    /// An open dropdown or colour popup, a focused slider or a hotkey being
    /// recorded keeps PageUp/PageDown from scrolling the settings content.
    fn page_keys_blocked(&self, window: &Window, cx: &App) -> bool {
        self.visible_forms().iter().any(|form| {
            let form = form.read(cx);
            form.choice_popup.is_some()
                || form.color_popup.is_some()
                || form.capture.is_some()
                || form
                    .sliders
                    .values()
                    .any(|slider| slider.read(cx).focus_handle().is_focused(window))
        })
    }

    fn can_undo(&self, cx: &App) -> bool {
        !self.closed
            && self.leave.is_none()
            && (!matches!(self.active.as_str(), DATA_PAGE | DEVICES_PAGE) || self.search_query.is_active())
            && !self.data_projection_busy(cx)
            && !self.schema_loading
            && !self.search_input.read(cx).is_composing()
            && self
                .forms
                .values()
                .all(|form| form.read(cx).undo_preflight(cx).is_ok())
            && self
                .forms
                .values()
                .any(|form| !form.read(cx).undo_changes(cx).is_empty())
    }

    fn undo_to_session_baseline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.leave.is_some() || self.schema_loading
            || (matches!(self.active.as_str(), DATA_PAGE | DEVICES_PAGE) && !self.search_query.is_active()) || self.data_projection_busy(cx)
        {
            return;
        }
        if let Err(error) = search_leave_preflight(self.search_input.read(cx).is_composing()) {
            self.window_message = Some(error);
            cx.notify();
            return;
        }
        let forms = self.all_forms();
        for form in &forms {
            if let Err(error) = form.read(cx).undo_preflight(cx) {
                self.window_message = Some(error);
                cx.notify();
                return;
            }
        }
        // Read every target before changing any draft. One failed read leaves
        // the whole cache untouched; stale protected values are not mistaken
        // for the currently persisted configuration.
        let mut plans = Vec::new();
        for form in forms {
            match form.read(cx).undo_plan(cx) {
                Ok(plan) => plans.push((form, plan)),
                Err(error) => {
                    self.window_message = Some(error);
                    cx.notify();
                    return;
                }
            }
        }
        self.window_message = None;
        // All reads/preflights completed before accepting the single retained
        // category replacement. It uses the same FIFO as every category editor.
        for (form, plan) in &plans {
            if let Some((_, target, _)) = plan.iter().find(|(key, _, write)| {
                *write
                    && form
                        .read(cx)
                        .field(key)
                        .is_some_and(|field| kind(field) == Kind::Tags)
            }) {
                if let Err(error) = form.update(cx, |form, cx| {
                    form.start_tag_undo(target.clone(), window, cx)
                }) {
                    self.window_message = Some(error);
                    cx.notify();
                    return;
                }
            }
        }
        for (form, plan) in plans {
            form.update(cx, |form, cx| form.restore_undo(plan, window, cx));
        }
        cx.notify();
    }

    pub fn request_authoritative_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        for form in self.forms.values() {
            form.update(cx, |form, cx| form.request_authoritative_refresh(cx));
        }
        if let Some(page) = &self.device_sync_page {
            page.update(cx, |page, cx| page.refresh(window, cx));
        }
        #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
        if let Some(page) = &self.webdav_page {
            page.update(cx, |page, cx| page.refresh(window, cx));
        }
        if let Some(page) = &self.reopen_page { page.update(cx, |page, cx| page.refresh_page(cx)); }
        // New categories must not initialize from a pre-event cached value.
        // Existing entities refresh separately without losing their drafts.
        self.schemas.clear();
        self.schema_complete = false;
        if self.schema_loading || self.search_query.is_active() {
            self.start_schema_load(window, cx);
        } else {
            self.schema_epoch = self.schema_epoch.wrapping_add(1);
            self.schema_task = None;
            self.schema_loading = false;
        }
        cx.notify();
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        for form in self.forms.values() {
            form.update(cx, |form, cx| form.set_theme(dark, cx));
        }
        if let Some(page) = &self.data_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark, cx));
        }
        if let Some(page) = &self.reopen_page { page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark, cx)); }
        if let Some(page) = &self.lan_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark, cx));
        }
        if let Some(page) = &self.device_sync_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark,
                self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 }), cx));
        }
        #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
        if let Some(page) = &self.webdav_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark,
                self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 }), cx));
        }
        if let Some(page) = &self.update_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark,
                self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 }), cx));
        }
        if let Some(page) = &self.home_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), dark,
                self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 }), cx));
        }
        self.refresh_search_colors(cx);
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.base_tokens = tokens;
        self.dark = dark;
        self.refresh_window_surfaces(cx);
    }

    pub fn set_window_background(
        &mut self,
        snapshot: Option<Arc<PreparedBackground>>,
        options: WindowBackgroundOptions,
        base_tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        if self.closed {
            return;
        }
        self.background_snapshot = snapshot;
        self.background_options = options;
        self.base_tokens = base_tokens;
        self.dark = dark;
        self.refresh_window_surfaces(cx);
    }

    fn palette(&self, theme: &ThemeTokens) -> SettingsPalette {
        self.surface_palettes.map_or_else(
            || SettingsPalette::new(theme),
            |(light, dark)| if self.dark { dark } else { light },
        )
    }

    fn reset_motion(&mut self) {
        self.motion.get_mut().clear();
        self.hovered.clear();
    }

    fn hover_progress(&self, id: &SharedString) -> f32 {
        let target = if self.hovered.contains(id) { 1.0 } else { 0.0 };
        self.motion.borrow_mut().value(
            (id.clone(), Track::Hover),
            target,
            motion::HOVER,
            motion::EASE,
            Instant::now(),
        )
    }

    fn hover_listener(
        id: SharedString,
        cx: &Context<Self>,
    ) -> Box<dyn Fn(&bool, &mut Window, &mut App)> {
        Box::new(cx.listener(move |this, hovered: &bool, _, cx| {
            let changed = if *hovered {
                this.hovered.insert(id.clone())
            } else {
                this.hovered.remove(&id)
            };
            if changed {
                cx.notify();
            }
        }))
    }

    fn refresh_window_surfaces(&mut self, cx: &mut Context<Self>) {
        self.reset_motion();
        // Every glass derivation starts from the authoritative resolved theme,
        // never from this window's previously faded tokens or palette.
        self.tokens = settings_window_tokens(
            &self.base_tokens,
            &self.background_options,
            self.background_snapshot.is_some(),
        );
        self.surface_palettes = self.background_snapshot.as_ref().map(|_| {
            (
                SettingsPalette::with_background(
                    self.base_tokens.theme(false),
                    self.tokens.theme(false),
                    &self.background_options,
                ),
                SettingsPalette::with_background(
                    self.base_tokens.theme(true),
                    self.tokens.theme(true),
                    &self.background_options,
                ),
            )
        });
        for form in self.forms.values() {
            form.update(cx, |form, cx| {
                form.surface_palettes = self.surface_palettes;
                form.set_design_tokens(self.tokens.clone(), self.dark, cx)
            });
        }
        if let Some(page) = &self.data_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), self.dark, cx));
        }
        if let Some(page) = &self.lan_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), self.dark, cx));
        }
        if let Some(page) = &self.device_sync_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), self.dark,
                self.surface_palettes.map(|palettes| if self.dark { palettes.1 } else { palettes.0 }), cx));
        }
        #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
        if let Some(page) = &self.webdav_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), self.dark,
                self.surface_palettes.map(|palettes| if self.dark { palettes.1 } else { palettes.0 }), cx));
        }
        if let Some(page) = &self.home_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), self.dark,
                self.surface_palettes.map(|palettes| if self.dark { palettes.1 } else { palettes.0 }), cx));
        }
        if let Some(page) = &self.update_page {
            page.update(cx, |page, cx| page.set_design_tokens(self.tokens.clone(), self.dark,
                self.surface_palettes.map(|palettes| if self.dark { palettes.1 } else { palettes.0 }), cx));
        }
        self.refresh_search_colors(cx);
    }

    fn refresh_search_colors(&mut self, cx: &mut Context<Self>) {
        let t = self.tokens.theme(self.dark);
        let palette = self.palette(t);
        self.search_input.update(cx, |input, cx| {
            input.set_colors(
                InputColors {
                    background: palette.control,
                    foreground: color(&t.text),
                    placeholder: color(&t.text_muted),
                    border: palette.control_border,
                    selection: color(&t.tab_active),
                    caret: color(&t.accent),
                },
                cx,
            )
        });
        cx.notify();
    }
    pub fn set_route(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = settings_page_from_route(route) {
            self.retire_system_request();
            self.pending_route = Some(route.to_string());
            self.navigate(&page, window, cx);
            if self.leave.is_none() && self.active == settings_navigation_id(&page) {
                self.apply_pending_route(window, cx);
            }
        }
    }
    /// Open or raise the settings window on `page` (a canonical form id).
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        page: Option<String>,
        cx: &mut App,
    ) -> Result<(), String> {
        let background_qa = cfg!(debug_assertions) && cliphost_core::is_headless();
        if let Some(handle) = existing {
            let raised = handle.update(cx, |this, window, cx| {
                this.set_design_tokens(tokens.clone(), dark, cx);
                if let Some(page) = &page {
                    this.navigate(page, window, cx);
                }
                if background_qa {
                    crate::platform::register_probe_window(window)
                        .and_then(|_| crate::platform::set_no_activate(window, true))
                        .and_then(|_| crate::platform::show_no_activate(window))
                } else {
                    crate::platform::present_main(window, &this.host)
                }
            });
            if let Ok(result) = raised {
                return result;
            }
        }
        let s = &tokens.settings;
        let (width, height) = settings_default_size(s.height, s.min_width, s.min_height);
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let min = size(px(s.min_width), px(s.min_height));
        let handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // Native presentation handles focus after GPUI releases
                    // its borrow, without the framework's synthetic Alt key.
                    focus: false,
                    show: !background_qa,
                    window_min_size: Some(min),
                    ..Default::default()
                },
                move |window, cx| {
                    window.set_window_title("ClipHost");
                    #[cfg(windows)]
                    if let Ok(hwnd) = crate::platform::hwnd(window) {
                        host.set_main_hwnd(hwnd.0 as isize);
                    }
                    if let Err(error) = crate::platform::register_probe_window(window) {
                        log::warn!("settings window probe registration failed: {error}");
                    }
                    let view = cx.new(|cx| Self::new(host, tokens, dark, page, window, cx));
                    let weak = view.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| this.request_user_close(window, cx));
                        false
                    });
                    view
                },
            )
            .map_err(|error| {
                let message = format!("{error:#}");
                log::warn!("settings window creation failed: {message}");
                message
            })?;
        *existing = Some(handle);
        handle
            .update(cx, |this, window, _| {
                if background_qa {
                    let result = crate::platform::register_probe_window(window)
                        .and_then(|_| crate::platform::set_no_activate(window, true))
                        .and_then(|_| crate::platform::show_no_activate(window));
                    if result.is_err() {
                        window.remove_window();
                    }
                    result
                } else {
                    crate::platform::present_main(window, &this.host)
                }
            })
            .map_err(|error| error.to_string())?
    }

    /// Handle the core's `HideMain`.
    pub fn close(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| {
            this.request_leave(SettingsLeave::Close { dismiss: true }, window, cx)
        });
    }

    pub fn save_and_close(handle: WindowHandle<Self>, cx: &mut App) -> SettingsCloseResult {
        Self::save_and_close_with(handle, true, cx)
    }

    /// Root ShowMain replacement waits for this before constructing its new
    /// main HWND. Closing the old settings view must not dismiss the new route.
    pub fn save_and_close_for_replacement(
        handle: WindowHandle<Self>,
        cx: &mut App,
    ) -> SettingsCloseResult {
        Self::save_and_close_with(handle, false, cx)
    }

    fn save_and_close_with(
        handle: WindowHandle<Self>,
        dismiss: bool,
        cx: &mut App,
    ) -> SettingsCloseResult {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        if let Err(error) = handle.update(cx, |this, window, cx| {
            let sender = sender.take().unwrap();
            if this.closed {
                let _ = sender.send(Ok(()));
                return;
            }
            this.close_waiters.push(sender);
            this.request_leave(SettingsLeave::Close { dismiss }, window, cx);
        }) {
            if let Some(sender) = sender {
                let _ = sender.send(Err(format!("Settings window is unavailable: {error}")));
            }
        }
        receiver
    }

    fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        page: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pages = settings_pages(&host);
        let pending_system_anchor = page
            .as_deref()
            .is_some_and(|page| settings_navigation_id(page) == "global" && page != "global");
        let requested_super_section = page.as_deref().and_then(super_menu_route_section);
        let active = page
            .map(|page| settings_navigation_id(&page).to_string())
            .filter(|page| pages.iter().any(|(id, _)| id == page))
            .or_else(|| pages.first().map(|(id, _)| id.clone()))
            .unwrap_or_else(|| "global".into());
        let form_host = host.clone();
        let form_tokens = tokens.clone();
        // Keep the schema form slot for ordinary settings only. Data's schema
        // must never create an auto-saving dataFolder input beside its editor.
        let form_id = if native_settings_page(&active) { "global" } else { &active };
        let form = cx.new(|cx| {
            let mut form = SettingsForm::new(form_host, form_tokens, dark, None, window, cx);
            form.load(form_id, window, cx);
            form
        });
        let search_input = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_placeholder(tr("搜索功能和设置…", "Search tools and settings…"), cx);
            input.set_metrics(tokens.settings.control_height - 2.0, 13.0, 10.0, cx);
            let t = tokens.theme(dark);
            input.set_colors(
                InputColors {
                    background: SettingsPalette::new(t).control,
                    foreground: color(&t.text),
                    placeholder: color(&t.text_muted),
                    border: SettingsPalette::new(t).control_border,
                    selection: color(&t.tab_active),
                    caret: color(&t.accent),
                },
                cx,
            );
            input
        });
        let search_subscription = cx.subscribe_in(
            &search_input,
            window,
            |this, _, event, window, cx| match event {
                SearchEvent::Changed(query) => this.search(query.clone(), window, cx),
                SearchEvent::Escape => this.clear_search(window, cx),
                SearchEvent::Submit => {}
            },
        );
        let mut previous_language = displayed_language();
        let language_watch = cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(250)).await;
            let current = displayed_language();
            let language_changed = current != previous_language;
            if this
                .update_in(cx, |this, window, cx| {
                    if language_changed {
                        this.pages = settings_pages(&this.host);
                        window.set_window_title("ClipHost");
                        for form in this.forms.values() {
                            form.update(cx, |form, cx| form.refresh_language_schema(cx));
                        }
                        if let Some(page) = &this.device_sync_page {
                            page.update(cx, |_, cx| cx.notify());
                        }
                        #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
                        if let Some(page) = &this.webdav_page {
                            page.update(cx, |_, cx| cx.notify());
                        }
                        if let Some(page) = &this.lan_page {
                            page.update(cx, |_, cx| cx.notify());
                        }
                        if let Some(page) = &this.home_page {
                            page.update(cx, |_, cx| cx.notify());
                        }
                        if let Some(page) = &this.update_page {
                            page.update(cx, |_, cx| cx.notify());
                        }
                        this.search_input.update(cx, |input, cx| {
                            input.set_placeholder(tr("搜索功能和设置…", "Search tools and settings…"), cx);
                        });
                        this.start_schema_load(window, cx);
                        cx.notify();
                    }
                    this.drain_deferred_search(window, cx);
                    this.maybe_scroll_to_system(cx);
                    this.sync_devices_page_activity(window, cx);
                })
                .is_err()
            {
                break;
            }
            previous_language = current;
        });
        let mut this = Self {
            host,
            base_tokens: tokens.clone(),
            tokens,
            background_snapshot: None,
            background_options: WindowBackgroundOptions::default(),
            surface_palettes: None,
            dark,
            pages,
            active,
            form: form.clone(),
            forms: HashMap::new(),
            form_observers: HashMap::new(),
            form_tab_subscriptions: HashMap::new(),
            data_page: None,
            data_observer: None,
            reopen_page: None,
            reopen_observer: None,
            lan_page: None,
            lan_subscriptions: Vec::new(),
            device_sync_page: None,
            #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
            webdav_page: None,
            #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
            webdav_observer: None,
            home_page: None,
            home_subscriptions: Vec::new(),
            device_sync_observer: None,
            update_page: None,
            update_observer: None,
            schemas: HashMap::new(),
            search_index: SettingsSearchIndex::default(),
            search_query: SettingsSearchQuery::default(),
            search_results: Vec::new(),
            deferred_search: DeferredSettingsSearch::default(),
            search_input,
            _search_subscription: search_subscription,
            schema_task: None,
            schema_epoch: 0,
            schema_loading: false,
            schema_complete: false,
            schema_errors: HashMap::new(),
            window_message: None,
            scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::default(),
            _language_watch: language_watch,
            leave: None,
            leave_task: None,
            close_waiters: Vec::new(),
            pending_route: None,
            pending_system_anchor,
            super_menu_sections: SuperMenuSections::default(),
            super_menu_actions_expanded: false,
            text_actions_manager: requested_super_section == Some("text-actions"),
            closed: false,
            motion: RefCell::new(Tweens::default()),
            hovered: HashSet::new(),
        };
        let form_id = if native_settings_page(&this.active) { "global" } else { &this.active }.to_string();
        this.retain_form(form_id, form, window, cx);
        let active = this.active.clone();
        this.ensure_native_page(&active, window, cx);
        this.ensure_global_system_form(window, cx);
        this.ensure_super_menu_forms(window, cx);
        if let Some(section) = requested_super_section {
            this.super_menu_actions_expanded = section == "text-actions";
            this.request_super_menu_section(section, cx);
        }
        this.maybe_scroll_to_system(cx);
        this
    }

    fn retain_form(
        &mut self,
        id: String,
        form: Entity<SettingsForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        form.update(cx, |form, _| {
            form.standalone_window = true;
            form.slider_search_input = Some(self.search_input.downgrade());
            if id == "menu_global" {
                form.super_menu_sections = Some(self.super_menu_sections.clone());
            }
        });
        let tab_subscription = cx.subscribe_in(
            &form,
            window,
            |this, _, event: &SettingsTabRequest, window, cx| {
                this.move_tab_focus(event.reverse, window, cx);
            },
        );
        if self.surface_palettes.is_some() {
            form.update(cx, |form, cx| {
                form.surface_palettes = self.surface_palettes;
                form.set_design_tokens(self.tokens.clone(), self.dark, cx);
            });
        }
        let observed_id = id.clone();
        let observer = cx.observe_in(&form, window, move |this, form, window, cx| {
            let label = this
                .pages
                .iter()
                .find(|(id, _)| *id == observed_id)
                .map(|(_, label)| label.as_str())
                .unwrap_or(&observed_id);
            let form_state = form.read(cx);
            index_initialized_form(
                &mut this.search_index,
                &observed_id,
                label,
                form_state.schema_initialized,
                &form_state.fields,
            );
            if this.leave.is_none() {
                this.refresh_projection(window, cx);
                if observed_id == "textAction" {
                    this.apply_pending_route(window, cx);
                }
            }
            this.maybe_scroll_to_system(cx);
            cx.notify();
        });
        if let Some((_, label)) = self.pages.iter().find(|(page, _)| *page == id) {
            let form_state = form.read(cx);
            index_initialized_form(
                &mut self.search_index,
                &id,
                label,
                form_state.schema_initialized,
                &form_state.fields,
            );
        }
        self.forms.insert(id.clone(), form);
        self.form_tab_subscriptions
            .insert(id.clone(), tab_subscription);
        self.form_observers.insert(id, observer);
    }

    fn ensure_global_system_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active != "global" || self.forms.contains_key(SYSTEM_PAGE) {
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let form = cx.new(|cx| {
            let mut form = SettingsForm::new(host, tokens, dark, None, window, cx);
            form.load(SYSTEM_PAGE, window, cx);
            form
        });
        self.retain_form(SYSTEM_PAGE.into(), form, window, cx);
    }

    fn ensure_super_menu_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active != "menu_global" {
            return;
        }
        for id in ["menu_file", "textAction"] {
            if self.forms.contains_key(id) {
                continue;
            }
            let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
            let form = cx.new(|cx| {
                let mut form = SettingsForm::new(host, tokens, dark, None, window, cx);
                form.load(id, window, cx);
                form
            });
            self.retain_form(id.into(), form, window, cx);
        }
    }

    fn ensure_data_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_page.is_some() {
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let page = cx.new(|cx| ManagementPage::new_embedded_data(host, tokens, dark, window, cx));
        self.data_observer = Some(cx.observe_in(&page, window, |this, _, window, cx| {
            this.drain_deferred_search(window, cx);
            cx.notify();
        }));
        self.data_page = Some(page);
    }

    /// Creates or refreshes the dedicated native view while retaining its drafts.
    /// Pairing uses a settings-only entity; conversations stay beside history.
    fn ensure_native_page(&mut self, page: &str, window: &mut Window, cx: &mut Context<Self>) {
        match page {
            "user" => self.ensure_reopen_page(window, cx),
            DEVICES_PAGE => self.ensure_devices_page(window, cx),
            UPDATE_PAGE => self.ensure_update_page(window, cx),
            HOME_PAGE => self.ensure_home_page(window, cx),
            DATA_PAGE | HELP_PAGE => {
                self.ensure_data_page(window, cx);
                let route = match page { HELP_PAGE => "docs", _ => "data" };
                if let Some(management) = &self.data_page {
                    management.update(cx, |management, cx| management.set_route(route, window, cx));
                }
            }
            _ => {}
        }
    }

    fn ensure_reopen_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.reopen_page {
            page.update(cx, |page, cx| { page.set_active(true, cx); page.refresh_page(cx); });
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let page = cx.new(|cx| crate::reopen::ReopenWindow::new_manager(host, tokens, dark, window, cx));
        self.reopen_observer = Some(cx.observe_in(&page, window, |_, _, _, cx| cx.notify()));
        self.reopen_page = Some(page);
    }

    fn ensure_home_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.home_page {
            page.update(cx, |page, cx| page.refresh(window, cx));
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let palette = self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 });
        let page = cx.new(|cx| {
            let mut page = HomeSettingsPage::new(host, tokens.clone(), dark, window, cx);
            page.set_design_tokens(tokens, dark, palette, cx);
            page
        });
        self.home_subscriptions = vec![
            cx.observe_in(&page, window, |_, _, _, cx| cx.notify()),
            cx.subscribe_in(&page, window, |this, _, event: &HomeEvent, window, cx| match event {
                HomeEvent::Open(page) => this.navigate(page, window, cx),
            }),
        ];
        self.home_page = Some(page);
    }

    /// Exit gates new LAN actions in the embedded LAN page.
    pub(crate) fn set_exit_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        if let Some(page) = &self.lan_page {
            page.update(cx, |page, cx| page.set_exit_pending(pending, cx));
        }
        if let Some(page) = &self.data_page {
            page.update(cx, |page, cx| page.set_lan_exit_pending(pending, cx));
        }
    }

    fn ensure_devices_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_device_sync_page(window, cx);
        #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
        self.ensure_webdav_page(window, cx);
        if self.lan_page.is_none() {
            let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
            let active = self.active == DEVICES_PAGE && !self.search_query.is_active() && self.leave.is_none();
            let sync = self.device_sync_page.clone();
            let page = cx.new(|cx| {
                let mut page = LanManagementPage::new_settings(host, tokens, dark, active, window, cx);
                if let Some(sync) = sync {
                    page.set_settings_content(sync.into(), cx);
                }
                page
            });
            self.lan_subscriptions = vec![
                cx.observe_in(&page, window, |_, _, _, cx| cx.notify()),
                cx.subscribe_in(&page, window, |this, _, event: &LanManagementEvent, _, cx| {
                    if let LanManagementEvent::Error(error) = event {
                        this.window_message = Some(error.clone());
                        cx.notify();
                    }
                }),
            ];
            self.lan_page = Some(page);
        }
        self.sync_devices_page_activity(window, cx);
    }

    fn sync_devices_page_activity(&self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
        if let Some(page) = &self.webdav_page {
            let active = !self.closed && self.leave.is_none()
                && self.active == DEVICES_PAGE && !self.search_query.is_active();
            page.update(cx, |page, cx| page.set_active(active, window, cx));
        }
        if let Some(page) = &self.reopen_page {
            let active = !self.closed && self.leave.is_none() && self.active == "user" && !self.search_query.is_active();
            page.update(cx, |page, cx| page.set_active(active, cx));
        }
        if let Some(page) = &self.lan_page {
            let active = !self.closed && self.leave.is_none()
                && self.active == DEVICES_PAGE && !self.search_query.is_active();
            page.update(cx, |page, cx| page.set_active(active, window, cx));
        }
    }

    fn ensure_device_sync_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.device_sync_page {
            page.update(cx, |page, cx| page.refresh(window, cx));
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let palette = self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 });
        let page = cx.new(|cx| {
            let mut page = DeviceSyncSettingsPage::new(host, tokens.clone(), dark, window, cx);
            page.set_design_tokens(tokens, dark, palette, cx);
            page
        });
        self.device_sync_observer = Some(cx.observe_in(&page, window, |_, _, _, cx| cx.notify()));
        self.device_sync_page = Some(page);
    }

    #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
    fn ensure_webdav_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.webdav_page {
            page.update(cx, |page, cx| page.refresh(window, cx));
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let palette = self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 });
        let page = cx.new(|cx| {
            let mut page = WebDavSettingsPage::new(host, tokens.clone(), dark, window, cx);
            page.set_design_tokens(tokens, dark, palette, cx);
            page
        });
        self.webdav_observer = Some(cx.observe_in(&page, window, |_, _, _, cx| cx.notify()));
        self.webdav_page = Some(page);
    }

    pub(crate) fn handle_device_sync_event(&mut self, name: &str, data: &Value,
        window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.device_sync_page {
            page.update(cx, |page, cx| page.on_event(name, data, window, cx));
        }
        if let Some(page) = &self.lan_page {
            page.update(cx, |page, cx| page.on_event(name, data, window, cx));
        }
        if let Some(page) = &self.data_page {
            page.update(cx, |page, cx| page.handle_lan_event(name, data, window, cx));
        }
    }

    fn data_projection_busy(&self, cx: &App) -> bool {
        self.data_page.as_ref().is_some_and(|page| page.read(cx).projection_busy(cx))
            || self.lan_page.as_ref().is_some_and(|page| page.read(cx).composition_active(cx))
    }

    fn ensure_update_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update_page.is_some() { return; }
        let host = self.host.clone();
        let tokens = self.tokens.clone();
        let dark = self.dark;
        let palette = self.surface_palettes.map(|palettes| if dark { palettes.1 } else { palettes.0 });
        let page = cx.new(|cx| {
            let mut page = UpdateSettingsPage::new(host, tokens.clone(), dark, window, cx);
            page.set_design_tokens(tokens, dark, palette, cx);
            page
        });
        self.update_observer = Some(cx.observe_in(&page, window, |_, _, _, cx| cx.notify()));
        self.update_page = Some(page);
    }

    fn lock_data_page(&self, locked: bool, cx: &mut Context<Self>) {
        if let Some(page) = &self.reopen_page { page.update(cx, |page, cx| page.set_leave_locked(locked, cx)); }
        if let Some(page) = &self.lan_page {
            page.update(cx, |page, cx| page.set_leave_locked(locked, cx));
        }
        if let Some(page) = &self.home_page {
            page.update(cx, |page, cx| page.set_leave_locked(locked, cx));
        }
        if let Some(page) = &self.update_page {
            page.update(cx, |page, cx| page.set_leave_locked(locked, cx));
        }
        if let Some(page) = &self.device_sync_page {
            page.update(cx, |page, cx| page.set_leave_locked(locked, cx));
        }
        if let Some(page) = &self.data_page {
            page.update(cx, |page, cx| page.set_data_leave_locked(locked, cx));
        }
    }

    fn retire_system_request(&mut self) {
        self.pending_system_anchor = false;
        retire_system_route(&mut self.pending_route);
    }

    fn maybe_scroll_to_system(&mut self, cx: &App) {
        if !self.pending_system_anchor
            || self.closed
            || self.leave.is_some()
            || self.active != "global"
            || self.search_query.is_active()
            || self.search_input.read(cx).is_composing()
            || self
                .forms
                .values()
                .any(|form| form.read(cx).projection_busy(cx))
        {
            return;
        }
        if ["global", SYSTEM_PAGE].iter().all(|id| {
            self.forms
                .get(*id)
                .is_some_and(|form| form.read(cx).schema_initialized)
        }) {
            // 系统选项位于全局页开头，旧 system/homeGlobal 路由也定位到这里。
            self.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            self.pending_system_anchor = false;
        }
    }

    fn ensure_form(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SettingsForm> {
        debug_assert_ne!(id, DATA_PAGE, "data schema is metadata, not an auto-saving form");
        if let Some(form) = self.forms.get(id) {
            let form = form.clone();
            self.hydrate_cached_form(id, &form, window, cx);
            return form;
        }
        let (host, tokens, dark) = (self.host.clone(), self.tokens.clone(), self.dark);
        let schema = self.schemas.get(id).cloned();
        let error = self.schema_errors.get(id).cloned();
        let form = cx.new(|cx| {
            let mut form = SettingsForm::new(host, tokens, dark, None, window, cx);
            if let Some(schema) = schema {
                form.load_schema(id, Ok(schema), window, cx);
            } else {
                form.config_id = id.into();
                if let Some(error) = error {
                    form.message = Some(FormMessage::Raw(true, error));
                }
            }
            form
        });
        self.retain_form(id.into(), form.clone(), window, cx);
        form
    }

    fn hydrate_cached_form(
        &self,
        id: &str,
        form: &Entity<SettingsForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = form.read(cx);
        if schema_application(state.schema_initialized, state.leave_locked)
            != SchemaApplication::Initialize
        {
            return;
        }
        if let Some(schema) = self.schemas.get(id).cloned() {
            // Keep the original Entity and its subscription; this is the first
            // initialization of a placeholder, not a replacement of a draft.
            form.update(cx, |form, cx| form.load_schema(id, Ok(schema), window, cx));
        }
    }

    fn hydrate_cached_forms(&self, window: &mut Window, cx: &mut Context<Self>) {
        for (id, form) in &self.forms {
            self.hydrate_cached_form(id, form, window, cx);
        }
    }

    /// Schemas are fetched outside render. Results are metadata; established
    /// entities never rebuild on retry, query changes or a language refresh.
    fn start_schema_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.schema_epoch = self.schema_epoch.wrapping_add(1);
        let epoch = self.schema_epoch;
        let language = displayed_language();
        self.schema_loading = true;
        let host = self.host.clone();
        let pages = settings_schema_pages(
            &self.pages,
            (!self.search_query.is_active()).then_some(self.active.as_str()),
        );
        self.schema_task = Some(cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    pages
                        .into_iter()
                        .map(|(id, label)| {
                            let result = if id == UPDATE_PAGE { Ok(updates::search_schema()) }
                                else if let Some(schema) = main_page_search_schema(&id) { Ok(schema) }
                                else { host.settings_form_get(&json!({ "configId": id })) };
                            (id, label, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !schema_reply_current(
                    epoch,
                    this.schema_epoch,
                    language == displayed_language(),
                    this.closed,
                ) {
                    return;
                }
                this.schema_loading = false;
                this.schema_complete = true;
                this.schema_errors.clear();
                for (id, label, result) in results {
                    if !this.pages.iter().any(|(page, _)| *page == id) {
                        this.search_index.remove(&id);
                        continue;
                    }
                    match result {
                        Ok(schema) => {
                            if searchable_settings_page(&id) {
                                index_settings_schema(&mut this.search_index, &id, &label, &schema);
                            } else {
                                this.search_index.remove(&id);
                            }
                            if let Some(form) = this.forms.get(&id) {
                                form.update(cx, |form, cx| {
                                    match schema_application(
                                        form.schema_initialized,
                                        form.leave_locked,
                                    ) {
                                        SchemaApplication::Initialize => {
                                            form.load_schema(&id, Ok(schema.clone()), window, cx)
                                        }
                                        SchemaApplication::Metadata => {
                                            if let Some(fields) = schema["fields"].as_array() {
                                                merge_localized_metadata(&mut form.fields, fields);
                                                form.language = displayed_language();
                                                form.refresh_image_placeholders(cx);
                                                cx.notify();
                                            }
                                        }
                                        // Store below, then hydrate the same Entity
                                        // after a failed leave or successful Select.
                                        SchemaApplication::Defer => {}
                                    }
                                });
                            }
                            this.schemas.insert(id, schema);
                        }
                        Err(error) => {
                            this.schema_errors.insert(id, error);
                        }
                    }
                }
                if this.leave.is_none() {
                    this.refresh_projection(window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn projection_allowed(&mut self, cx: &mut Context<Self>) -> bool {
        let data_busy = self.data_projection_busy(cx);
        let data_error = self.data_page.as_ref()
            .and_then(|page| page.read(cx).preflight_data_leave(cx).err());
        if self.search_input.read(cx).is_composing()
            || data_busy
            || self
                .forms
                .values()
                .any(|form| form.read(cx).projection_busy(cx))
        {
            self.window_message = Some(data_error.unwrap_or_else(|| {
                if data_busy {
                    tr("请等待数据操作完成，再切换搜索结果。", "Wait for the data operation to finish before changing search results.")
                } else {
                    tr(
                    "请先完成输入法组合，再切换搜索结果。",
                    "Finish composing text before changing search results.",
                    )
                }.into()
            }));
            cx.notify();
            return false;
        }
        true
    }

    fn search(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave.is_some() || self.closed {
            return;
        }
        self.retire_system_request();
        if !self.projection_allowed(cx) {
            self.deferred_search.defer_query(query);
            return;
        }
        self.deferred_search.take_when_ready(true);
        if self.search_query.set_query(query) {
            self.window_message = None;
            self.cancel_super_menu_section_jump(cx);
            self.scroll.set_offset(point(px(0.0), px(0.0)));
            if self.search_query.is_active() && !self.schema_complete && !self.schema_loading {
                self.start_schema_load(window, cx);
            }
            self.refresh_projection(window, cx);
        } else {
            // A deferred locale/schema refresh can accompany a query that
            // returned to the same raw text while another field was composing.
            self.window_message = None;
            self.refresh_projection(window, cx);
        }
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave.is_some() || self.closed {
            return;
        }
        self.retire_system_request();
        if !self.projection_allowed(cx) {
            if !self.search_input.read(cx).is_composing() {
                self.deferred_search.defer_query(String::new());
                self.search_input
                    .update(cx, |input, cx| input.set_text(String::new(), window, cx));
            }
            return;
        }
        self.deferred_search.take_when_ready(true);
        self.search_query.clear();
        self.cancel_super_menu_section_jump(cx);
        self.search_input
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.window_message = None;
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        self.refresh_projection(window, cx);
        self.search_input.read(cx).focus_handle().focus(window);
    }

    fn refresh_projection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave.is_some() || self.closed {
            return;
        }
        if self.search_input.read(cx).is_composing()
            || self.data_projection_busy(cx)
            || self
                .forms
                .values()
                .any(|form| form.read(cx).projection_busy(cx))
        {
            self.deferred_search.defer_refresh();
            return;
        }
        let order = self
            .pages
            .iter()
            .filter(|(id, _)| searchable_settings_page(id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let matches = self.search_index.matches(self.search_query.query(), &order);
        if self.search_query.is_active() {
            // Create only the matching editors, using already loaded schemas.
            for item in &matches {
                if item.config_id == UPDATE_PAGE {
                    self.ensure_update_page(window, cx);
                } else if !native_settings_page(&item.config_id) {
                    self.ensure_form(&item.config_id, window, cx);
                }
            }
            for (id, form) in &self.forms {
                let keys = matches
                    .iter()
                    .find(|item| item.config_id == *id)
                    .map(|item| item.field_keys.clone())
                    .unwrap_or_default();
                form.update(cx, |form, cx| form.set_search_keys(Some(keys), cx));
            }
        } else {
            self.ensure_global_system_form(window, cx);
            self.ensure_super_menu_forms(window, cx);
            for form in self.forms.values() {
                form.update(cx, |form, cx| form.set_search_keys(None, cx));
            }
        }
        self.search_results = matches;
        self.sync_devices_page_activity(window, cx);
        cx.notify();
    }

    fn drain_deferred_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deferred_search.is_pending() || self.leave.is_some() || self.closed {
            return;
        }
        let ready = !self.search_input.read(cx).is_composing()
            && !self.data_projection_busy(cx)
            && !self
                .forms
                .values()
                .any(|form| form.read(cx).projection_busy(cx));
        match self.deferred_search.take_when_ready(ready) {
            Some(Some(query)) => self.search(query, window, cx),
            Some(None) => self.refresh_projection(window, cx),
            None => {}
        }
    }

    fn all_forms(&self) -> Vec<Entity<SettingsForm>> {
        let mut forms = self
            .pages
            .iter()
            .filter_map(|(id, _)| self.forms.get(id).cloned())
            .collect::<Vec<_>>();
        // A catalog refresh may hide a previously opened category. Its draft
        // and accepted writes still belong to this window's leave barrier.
        forms.extend(
            self.forms
                .iter()
                .filter(|(id, _)| !self.pages.iter().any(|(page, _)| page == *id))
                .map(|(_, form)| form.clone()),
        );
        forms
    }

    fn pending_writes(&self, cx: &App) -> bool {
        self.forms
            .values()
            .any(|form| form.read(cx).pending_writes())
            || self.data_page.as_ref().is_some_and(|page| page.read(cx).pending_data_writes())
            || self.device_sync_page.as_ref().is_some_and(|page| page.read(cx).pending_writes())
            || self.lan_page.as_ref().is_some_and(|page| page.read(cx).pending_writes())
            || self.reopen_page.as_ref().is_some_and(|page| page.read(cx).pending_writes())
    }

    fn select(&mut self, page: &str, window: &mut Window, cx: &mut Context<Self>) {
        if native_settings_page(page) {
            self.navigate(page, window, cx);
            return;
        }
        if self.search_query.is_active() {
            if self.leave.is_none() && self.projection_allowed(cx) {
                self.retire_system_request();
                self.search_query.set_scope(Some(page.into()));
                self.cancel_super_menu_section_jump(cx);
                self.scroll.set_offset(point(px(0.0), px(0.0)));
                self.refresh_projection(window, cx);
            }
            return;
        }
        self.navigate(page, window, cx);
    }

    fn navigate(&mut self, page: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        if page == TEXT_ACTIONS_PAGE {
            self.navigate("textAction", window, cx);
            return;
        }
        let system_requested = page == SYSTEM_PAGE || page == "system";
        let text_actions_manager = page == "textAction";
        let section = super_menu_route_section(page);
        let page = settings_navigation_id(page);
        if !self.pages.iter().any(|(id, _)| id == page) {
            return;
        }
        if !system_requested {
            self.retire_system_request();
        }
        self.pending_system_anchor = system_requested;
        if self.active == page && self.leave.is_none() && !self.search_query.is_active() {
            if let Some(section) = section {
                if !self.request_super_menu_section(section, cx) {
                    return;
                }
                if section == "text-actions" {
                    self.super_menu_actions_expanded = true;
                }
            }
            self.ensure_native_page(page, window, cx);
            self.ensure_global_system_form(window, cx);
            self.ensure_super_menu_forms(window, cx);
            self.maybe_scroll_to_system(cx);
            self.text_actions_manager = text_actions_manager;
            cx.notify();
            return;
        }
        self.request_leave(SettingsLeave::Select { page: page.into(), section }, window, cx);
    }

    fn apply_pending_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_route.as_deref().and_then(settings_page_from_route).as_deref() == Some("textAction")
            && self.forms.get("textAction").is_none_or(|form| form.read(cx).text_actions.is_none())
        {
            // Its first schema loads asynchronously. Preserve actionId/create
            // until the retained manager can consume the original route.
            return;
        }
        if let Some(route) = self.pending_route.take() {
            let Some(page) = settings_page_from_route(&route) else {
                return;
            };
            if settings_navigation_id(&page) != self.active {
                return;
            }
            if page == SYSTEM_PAGE {
                self.pending_system_anchor = true;
                self.maybe_scroll_to_system(cx);
            }
            if let Some(section) = super_menu_route_section(&page) {
                self.request_super_menu_section(section, cx);
                if page == "textAction" {
                    self.super_menu_actions_expanded = true;
                }
            }
            let routed_form = self.forms.get(&page).cloned().unwrap_or_else(|| self.form.clone());
            routed_form.update(cx, |form, cx| {
                if let Some(actions) = &form.text_actions {
                    actions.update(cx, |actions, cx| actions.set_route(&route, window, cx));
                }
            });
        }
    }

    fn resolve_close(&mut self, result: Result<(), String>) {
        for sender in self.close_waiters.drain(..) {
            let _ = sender.send(result.clone());
        }
    }

    fn retain_save_error(&self, window: &Window) {
        #[cfg(windows)]
        if let Ok(hwnd) = crate::platform::hwnd(window) {
            self.host.retain_native_main_for_save_error(hwnd.0 as isize);
        }
        #[cfg(not(windows))]
        let _ = window;
    }

    fn request_user_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        if self.host.close_behavior() == "ExitApplication" {
            // Keep this window and its live drafts mounted. The root's Exit
            // handler calls save_and_close through the existing global barrier;
            // internal close/replacement must never enqueue another Exit.
            self.host
                .chrome_web_action(&json!({ "kind": "mainHostCommand", "command": "exit" }));
        } else {
            self.request_leave(SettingsLeave::Close { dismiss: true }, window, cx);
        }
    }

    fn request_leave(&mut self, leave: SettingsLeave, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(leave, SettingsLeave::Close { .. }) {
            self.retire_system_request();
        }
        if self.closed {
            self.resolve_close(Ok(()));
            return;
        }
        if matches!(self.leave, Some(SettingsLeave::Close { .. }))
            && matches!(leave, SettingsLeave::Select { .. })
        {
            self.resolve_close(Err(tr(
                "关闭已取消：设置页已切换。",
                "Close cancelled: the settings page changed.",
            )
            .into()));
        }
        if self.leave.is_some() {
            self.leave = Some(leave);
            cx.notify();
            return;
        }
        if let Err(error) = search_leave_preflight(self.search_input.read(cx).is_composing()) {
            self.window_message = Some(error.clone());
            self.retain_save_error(window);
            self.resolve_close(Err(error));
            cx.notify();
            return;
        }
        if self.lan_page.as_ref().is_some_and(|page| page.read(cx).composition_active(cx)) {
            let error = tr("请先完成输入法组合，再离开跨设备设置。", "Finish composing text before leaving Cross-device settings.").to_string();
            self.window_message = Some(error.clone());
            self.retain_save_error(window);
            self.resolve_close(Err(error));
            cx.notify();
            return;
        }
        // Preflight every retained editor before accepting any leave writes,
        // including drafts hidden by the current query or category scope.
        if let Some(page) = &self.reopen_page {
            if let Err(error) = page.update(cx, |page, cx| page.prepare_leave(window, cx)) {
                self.window_message = Some(error.clone());
                self.retain_save_error(window);
                self.resolve_close(Err(error));
                cx.notify();
                return;
            }
        }
        if let Some(error) = self.data_page.as_ref()
            .and_then(|page| page.read(cx).preflight_data_leave(cx).err())
        {
            self.window_message = Some(error.clone());
            self.retain_save_error(window);
            self.resolve_close(Err(error));
            cx.notify();
            return;
        }
        let forms = self.all_forms();
        for form in &forms {
            if let Err(error) = form.read(cx).preflight_inputs(cx) {
                form.update(cx, |form, cx| {
                    form.message = Some(FormMessage::Raw(true, error.clone()));
                    cx.notify();
                });
                self.window_message = Some(error.clone());
                self.retain_save_error(window);
                self.resolve_close(Err(error));
                cx.notify();
                return;
            }
        }
        for form in &forms {
            form.update(cx, |form, cx| {
                form.commit_all_inputs(window, cx);
                form.set_leave_locked(true, cx);
            });
        }
        self.lock_data_page(true, cx);
        self.search_input
            .update(cx, |input, cx| input.set_read_only(true, cx));
        window.blur();
        self.leave = Some(leave);
        self.sync_devices_page_activity(window, cx);
        // Includes accepted explicit Data saves/clears. Keep the UI executor
        // running so their replies can release the flag before closing or
        // resolving a root replacement/restart's original oneshot.
        if !self.pending_writes(cx) {
            self.finish_leave(window, cx);
            return;
        }
        self.leave_task = Some(cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(20)).await;
            let done = this.update_in(cx, |this, window, cx| {
                if this.leave.is_none() {
                    return true;
                }
                if this.pending_writes(cx) {
                    return false;
                }
                this.finish_leave(window, cx);
                true
            });
            if !matches!(done, Ok(false)) {
                break;
            }
        }));
        cx.notify();
    }

    fn finish_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(leave) = self.leave.take() else {
            return;
        };
        self.leave_task = None;
        let forms = self.all_forms();
        let leave_error = forms.iter().find_map(|form| {
            let form = form.read(cx);
            form.preflight_inputs(cx)
                .err()
                .or_else(|| form.persistence_error())
        }).or_else(|| self.data_page.as_ref().and_then(|page| {
            let page = page.read(cx);
            page.preflight_data_leave(cx).err().or_else(|| page.data_persistence_error())
        })).or_else(|| self.device_sync_page.as_ref()
            .and_then(|page| page.read(cx).persistence_error()))
            .or_else(|| self.reopen_page.as_ref().and_then(|page| page.read(cx).persistence_error()))
            .or_else(|| self.lan_page.as_ref()
                .and_then(|page| page.read(cx).settings_persistence_error()));
        if let Some(error) = leave_error {
            self.window_message = Some(error.clone());
            for form in &forms {
                form.update(cx, |form, cx| form.set_leave_locked(false, cx));
            }
            self.lock_data_page(false, cx);
            self.hydrate_cached_forms(window, cx);
            self.search_input
                .update(cx, |input, cx| input.set_read_only(false, cx));
            self.retain_save_error(window);
            self.resolve_close(Err(error));
            self.sync_devices_page_activity(window, cx);
            cx.notify();
            return;
        }
        for form in &forms {
            form.update(cx, |form, cx| form.set_leave_locked(false, cx));
        }
        self.lock_data_page(false, cx);
        self.search_input
            .update(cx, |input, cx| input.set_read_only(false, cx));
        self.window_message = None;
        match leave {
            SettingsLeave::Select { page, section } => {
                self.text_actions_manager = section == Some("text-actions");
                let category_changed = self.active != page;
                self.reset_motion();
                self.hydrate_cached_forms(window, cx);
                self.active = page.clone();
                self.deferred_search.take_when_ready(true);
                self.search_query.clear();
                self.search_input
                    .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                if native_settings_page(&page) {
                    self.ensure_native_page(&page, window, cx);
                } else {
                    self.form = self.ensure_form(&page, window, cx);
                }
                self.ensure_global_system_form(window, cx);
                self.ensure_super_menu_forms(window, cx);
                if category_changed {
                    for form in self.forms.values() {
                        form.update(cx, |form, cx| form.reset_undo_baseline(cx));
                    }
                }
                if !self.schemas.contains_key(&page) && !self.schema_loading {
                    self.start_schema_load(window, cx);
                }
                self.refresh_projection(window, cx);
                self.scroll.set_offset(point(px(0.0), px(0.0)));
                self.cancel_super_menu_section_jump(cx);
                if let Some(section) = section {
                    self.request_super_menu_section(section, cx);
                    if section == "text-actions" {
                        self.super_menu_actions_expanded = true;
                    }
                }
                self.apply_pending_route(window, cx);
                self.maybe_scroll_to_system(cx);
                if page == DATA_PAGE {
                    if let Some(page) = &self.data_page {
                        page.read(cx).data_focus_handle(cx).focus(window);
                    }
                }
            }
            SettingsLeave::Close { dismiss } => {
                self.closed = true;
                self.sync_devices_page_activity(window, cx);
                self.reset_motion();
                if dismiss {
                    self.host.dismiss_native_window("main");
                }
                self.host.set_main_hwnd(0);
                self.resolve_close(Ok(()));
                window.remove_window();
            }
        }
        cx.notify();
    }
}

impl SettingsWindow {
    /// The text actions manager reuses the retained form and its save/IME leave
    /// barrier; it does not create a second editor or persistence session.
    fn text_actions_manager_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.forms.get("textAction").and_then(|form| form.read(cx).text_actions.clone()) {
            Some(actions) => {
                actions.update(cx, |actions, cx| actions.set_embedded(false, cx));
                let t = self.tokens.theme(self.dark);
                rsx! { <div flex_1 min_w_0 min_h_0 flex flex_col overflow_hidden /> }
                    .child(rsx! {
                        <div flex_none min_h={px(38.0)} px={px(self.tokens.settings.content_padding_x)}
                            flex items_center gap={px(6.0)} border_b_1 border_color={self.palette(t).row_border}
                            text_size={px(self.tokens.settings.description_font_size)} text_color={color(&t.text_muted)} />
                    }
                        .child(div().id("settings-text-actions-parent").cursor_pointer()
                            .text_color(color(&t.text_secondary))
                            .child(tr("超级菜单", "Super menu"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.super_menu_actions_expanded = false;
                                this.navigate("menu_global", window, cx);
                                this.request_super_menu_section("text-actions", cx);
                            })))
                        .child(svg().path(self.tokens.icons.get("chevron-right").cloned().unwrap_or_default()).size(px(12.0)))
                        .child(tr("文本操作", "Text actions")))
                    .child(actions)
                    .into_any_element()
            }
            None => div().p(px(24.0)).child(tr("正在读取文本操作…", "Loading text actions…")).into_any_element(),
        }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark);
        let s = &tokens.settings;
        let l = &tokens.layout;
        let palette = self.palette(t);
        let wallpaper_layer = self.background_snapshot.as_ref().map(|image| {
            crate::window_background::render_background_image(image, &self.background_options)
        });
        let wallpaper_tint = self.background_snapshot.as_ref().map(|_| {
            div()
                .absolute()
                .inset_0()
                .bg(crate::window_glass::page_tint(
                    self.base_tokens.theme(self.dark),
                    &self.background_options,
                ))
        });
        let geometry = SettingsGeometry::new(f32::from(window.viewport_size().width), s);
        let close_hover = color(&t.tertiary);
        let item_hover = palette.control_hover;
        let close_id = SharedString::from("settings-close");
        let close_progress = self.hover_progress(&close_id);
        let matches = &self.search_results;
        let searching = self.search_query.is_active();
        // The text actions manager keeps `menu_global` as its form category.
        let nav_active = if self.text_actions_manager { TEXT_ACTIONS_PAGE } else { self.active.as_str() }.to_string();
        let full_bleed = !searching && (self.text_actions_manager || full_bleed_settings_page(&self.active));
        let sidebar_width = geometry.sidebar_width;
        let super_menu_visible = !searching && self.active == "menu_global";
        let padding = if super_menu_visible { 22.0 } else { geometry.content_padding };
        if !super_menu_visible {
            self.super_menu_sections.clear_layout();
        }
        let search_bar = rsx! {
            <div id={"settings-titlebar-search"} w={px(260.0)} ml={px(padding)} min_w_0 flex_none
                flex items_center gap={px(8.0)} />
        }
            // 图标、边框留白和清除按钮也不能冒泡成标题栏拖动。
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                this.search_input.read(cx).focus_handle().focus(window);
                cx.stop_propagation();
            }))
            .child(
                rsx! {
                    <div flex flex_1 min_w_0 gap={px(4.0)} pl={px(10.0)} items_center
                        rounded={px(8.0)} border_1 />
                }
                    .border_color(
                        if self.search_input.read(cx).focus_handle().is_focused(window) {
                            color(&t.accent)
                        } else {
                            palette.control_border
                        },
                    )
                    .bg(palette.control)
                    .overflow_hidden()
                    .child(
                        svg()
                            .path(tokens.icons.get("search").cloned().unwrap_or_default())
                            .size(px(l.chrome_icon_size))
                            .text_color(palette.description),
                    )
                    .child(rsx! {
                        <div flex_1 min_w_0>
                            {self.search_input.clone()}
                        </div>
                    }),
            )
            .when(!self.search_input.read(cx).text().is_empty(), |bar| {
                let clear_id = SharedString::from("settings-search-clear");
                let clear_hover = self.hover_progress(&clear_id);
                bar.child(
                    rsx! {
                        <div id={clear_id.clone()} flex_none px={px(8.0)} py={px(5.0)}
                            rounded={px(5.0)}
                            bg={mix_hsla(
                            gpui::transparent_black(),
                            palette.control_hover,
                            clear_hover,
                        )} />
                    }
                        .on_hover(Self::hover_listener(clear_id, cx))
                        .text_size(px(12.0))
                        .text_color(color(&t.text_secondary))
                        .cursor_pointer()
                        .child(tr("清除", "Clear"))
                        .on_click(cx.listener(|this, _, window, cx| this.clear_search(window, cx))),
                )
            });
        let titlebar = rsx! {
            <div h={px(44.0)} flex_none flex items_center />
        }
            .bg(if self.background_snapshot.is_some() { palette.titlebar } else { palette.page })
            .border_b_1()
            .border_color(palette.sidebar_border)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if event.click_count == 1 {
                        if let Err(error) =
                            crate::platform::begin_host_move(window, &this.host, "main")
                        {
                            this.form.update(cx, |form, cx| {
                                form.message = Some(FormMessage::Raw(true, error));
                                cx.notify();
                            });
                        }
                    }
                }),
            )
            .child(
                rsx! {
                    <div flex flex_none w={px(sidebar_width)} h_full px={px(16.0)} items_center
                        gap={px(8.0)} text_size={px(13.0)} text_color={color(&t.text_secondary)}>
                        {svg()
                            .path(tokens.icons.get("clipboard").cloned().unwrap_or_default())
                            .size(px(l.chrome_icon_size))
                            .text_color(color(&t.accent))}
                        {"ClipHost"}
                    </div>
                },
            )
            .child(search_bar)
            .child(div().flex_1().min_w_0().h_full())
            .child(
                rsx! {
                    <div id={close_id.clone()} size={px(l.chrome_button_size)} mr={px(12.0)}
                        flex_none flex items_center justify_center rounded={px(4.0)}
                        cursor_pointer
                        bg={mix_hsla(
                        gpui::transparent_black(),
                        close_hover,
                        close_progress,
                    )} />
                }
                    .on_hover(Self::hover_listener(close_id, cx))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.request_user_close(window, cx)),
                    )
                    .child(
                        svg()
                            .path(tokens.icons.get("close").cloned().unwrap_or_default())
                            .size(px(l.chrome_icon_size))
                            .text_color(color(&t.text_secondary)),
                    ),
            );
        let sidebar = rsx! {
            <div id={"settings-pages"} w={px(sidebar_width)} flex_none h_full />
        }
            .overflow_y_scroll()
            .smooth_scroll("settings-pages", cx)
            .px(px(s.sidebar_padding))
            .py(px(12.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .bg(if self.background_snapshot.is_some() { palette.sidebar } else { palette.page })
            .border_r_1()
            .border_color(palette.sidebar_border)
            .when(searching, |nav| {
                nav.child(
                    widgets::navigation_row(s)
                        .id("settings-all-results")
                        .when(self.search_query.scope().is_none(), |item| {
                            item.bg(palette.control_hover).child(widgets::navigation_marker(t))
                        })
                        .text_color(color(&t.text))
                        .child(
                            svg()
                                .path(tokens.icons.get("search").cloned().unwrap_or_default())
                                .size(px(l.chrome_icon_size))
                                .text_color(color(&t.accent)),
                        )
                        .child(tr("全部结果", "All results"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.leave.is_none() && this.projection_allowed(cx) {
                                this.search_query.set_scope(None);
                                this.scroll.set_offset(point(px(0.0), px(0.0)));
                                this.refresh_projection(window, cx);
                            }
                        })),
                )
            })
            .children({
                let mut previous_group = None;
                let mut rows = Vec::new();
                for (id, label) in self.pages.iter().filter(|(id, _)| {
                        sidebar_settings_page(id)
                            && (!searching
                                || matches.iter().any(|item| {
                                    settings_navigation_id(&item.config_id) == id.as_str()
                                }))
                    }) {
                    let group = settings_nav_group(id);
                    if !searching && previous_group != Some(group) {
                        if let Some(heading) = settings_nav_group_label(group) {
                            rows.push(rsx! {
                                <div flex_none px={px(10.0)} pb={px(2.0)} text_size={px(11.0)}
                                    line_height={px(18.0)} text_color={color(&t.text_muted)}>
                                    {heading}
                                </div>
                            }
                                .when(previous_group.is_some(), |heading| heading.mt(px(6.0)).pt(px(8.0))
                                    .border_t_1().border_color(palette.row_border))
                                .into_any_element());
                        }
                    }
                    previous_group = Some(group);
                    rows.push({
                        let active = if searching {
                            self.search_query.scope().map(settings_navigation_id)
                                == Some(id.as_str())
                        } else {
                            *id == nav_active
                        };
                        let count = searching.then(|| {
                            settings_navigation_match_count(
                                id,
                                matches
                                    .iter()
                                    .map(|item| (item.config_id.as_str(), item.count())),
                            )
                        });
                        let page = id.clone();
                        let nav_id = SharedString::from(format!("page-{id}"));
                        let nav_hover = if active {
                            0.0
                        } else {
                            self.hover_progress(&nav_id)
                        };
                        widgets::navigation_row(s)
                            .id(nav_id.clone())
                            .when(active, |item| {
                                item.bg(palette.control_hover).text_color(color(&t.text))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(widgets::navigation_marker(t))
                            })
                            .when(!active, |item| {
                                item.text_color(color(&t.text_secondary))
                                    .bg(mix_hsla(
                                        gpui::transparent_black(),
                                        item_hover,
                                        nav_hover,
                                    ))
                                    .on_hover(Self::hover_listener(nav_id, cx))
                            })
                            .child(
                                svg()
                                    .path(
                                        tokens
                                            .icons
                                            .get(settings_nav_icon(id))
                                            .cloned()
                                            .unwrap_or_default(),
                                    )
                                    .size(px(18.0))
                                    .flex_none()
                                    .text_color(color(if active {
                                        &t.accent
                                    } else {
                                        &t.text_muted
                                    })),
                            )
                            .child(rsx! {
                                <div flex_1 min_w_0 whitespace_nowrap>
                                    {label.clone()}
                                </div>
                            })
                            .when_some(count, |row, count| {
                                row.child(
                                    rsx! {
                                        <div flex_none min_w={px(18.0)} px={px(4.0)}
                                            rounded={px(4.0)} bg={palette.control_hover}
                                            text_align={gpui::TextAlign::Center}
                                            text_size={px(11.0)} text_color={color(&t.text_muted)}>
                                            {count.to_string()}
                                        </div>
                                    },
                                )
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select(&page, window, cx)
                            }))
                            .tooltip(settings_help(label.clone(), t.clone()))
                            .into_any_element()
                    });
                    if id == "menu_global" && !searching && self.active == "menu_global" {
                        let active = self.text_actions_manager;
                        rows.push(
                            widgets::navigation_row(s)
                                .id("page-text-actions-child")
                                .ml(px(18.0)).min_h(px(30.0)).h(px(30.0))
                                .text_size(px(s.description_font_size))
                                .text_color(color(if active { &t.text } else { &t.text_secondary }))
                                .when(active, |row| row.bg(palette.control_hover)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(widgets::navigation_marker(t)))
                                .when(!active, |row| row.hover(|style| style.bg(palette.control_hover)))
                                .child(svg().path(tokens.icons.get("text").cloned().unwrap_or_default())
                                    .size(px(14.0)).flex_none().text_color(color(if active { &t.accent } else { &t.text_muted })))
                                .child(tr("文本操作", "Text actions"))
                                .on_click(cx.listener(|this, _, window, cx| this.navigate(TEXT_ACTIONS_PAGE, window, cx)))
                                .into_any_element(),
                        );
                    }
                }
                rows
            });
        let mut flow = widgets::page_column().pb(px(20.0))
            // Give the form Entity and its following guidance one definite
            // width. Percentage-only intrinsic sizing can measure the form
            // at min-content width and place the next card over its rows.
            .when(!searching && matches!(self.active.as_str(), "user" | "menu_global" | DEVICES_PAGE), |flow| {
                flow.w(px((geometry.content_width - 2.0 * (padding - geometry.content_padding)).max(0.0).min(760.0)))
            });
        if !searching && self.active == "menu_global" {
            flow = flow.gap(px(28.0));
        }
        if let Some(message) = &self.window_message {
            flow = flow.child(
                widgets::note(message.clone(), palette)
                    .text_color(color(&t.error)),
            );
        }
        if self.schema_loading {
            flow = flow.child(
                rsx! {
                    <div text_size={px(12.0)} text_color={color(&t.text_muted)}>
                        {tr("正在加载设置…", "Loading settings…")}
                    </div>
                },
            );
        }
        if !self.schema_errors.is_empty() {
            let failed = self
                .pages
                .iter()
                .filter_map(|(id, label)| {
                    self.schema_errors
                        .get(id)
                        .map(|error| format!("{label}: {error}"))
                })
                .collect::<Vec<_>>()
                .join("\n");
            flow = flow.child(
                rsx! {
                    <div flex flex_col gap={px(8.0)} text_size={px(12.0)}
                        text_color={color(&t.error)}>
                        {format!(
                        "{}\n{failed}",
                        tr(
                            "部分设置加载失败，已加载的设置仍可使用。",
                            "Some settings could not load. Loaded settings remain available."
                        )
                    )}
                    </div>
                }
                    .child(
                        rsx! {
                            <div id={"settings-load-retry"} flex cursor_pointer
                                text_color={color(&t.accent)}>
                                {tr("重试", "Retry")}
                            </div>
                        }
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.leave.is_none() && !this.schema_loading {
                                    this.start_schema_load(window, cx);
                                }
                            })),
                    ),
            );
        }
        if searching {
            let visible = matches
                .iter()
                .filter(|item| settings_scope_includes(self.search_query.scope(), &item.config_id))
                .collect::<Vec<_>>();
            let multiple_categories = visible.len() > 1;
            if visible.is_empty() && !self.schema_loading {
                flow = flow.child(
                    rsx! {
                        <div py={px(20.0)} text_size={px(13.0)} text_color={color(&t.text_muted)}>
                            {format!(
                            "{}「{}」",
                            if self.search_query.scope().is_some_and(|scope| {
                                self.pages.iter().any(|(id, _)| id == scope)
                            }) {
                                tr(
                                    "当前分类没有匹配的设置：",
                                    "No matching settings in this category: ",
                                )
                            } else {
                                tr("没有匹配的设置：", "No matching settings: ")
                            },
                            self.search_query.query().trim()
                        )}
                        </div>
                    },
                );
            }
            for item in visible {
                if matches!(item.config_id.as_str(), HOME_PAGE | DEVICES_PAGE | HELP_PAGE | TEXT_ACTIONS_PAGE) {
                    let target = item.config_id.clone();
                    let open_id = SharedString::from(format!("settings-open-result-{target}"));
                    let open_hover = self.hover_progress(&open_id);
                    let fields = main_page_search_schema(&target)
                        .and_then(|schema| schema["fields"].as_array().cloned())
                        .into_iter().flatten()
                        .filter(|field| field["key"].as_str().is_some_and(|key| item.field_keys.contains(key)))
                        .filter_map(|field| field["label"].as_str().map(str::to_string))
                        .collect::<Vec<_>>();
                    flow = flow.child(
                        rsx! {
                            <div id={SharedString::from(format!("settings-result-{target}"))} w_full min_w_0
                                flex_none flex items_center gap={px(12.0)} p={px(s.row_padding_x)}
                                rounded={px(s.card_radius)} border_1
                                border_color={palette.card_border} bg={palette.card} />
                        }
                            .child(svg().path(tokens.icons.get(settings_nav_icon(&target)).cloned().unwrap_or_default())
                                .size(px(18.0)).flex_none().text_color(color(&t.accent)))
                            .child(rsx! {
                                <div flex_1 min_w_0 flex flex_col gap={px(2.0)}>
                                    <div text_size={px(14.0)} font_weight={FontWeight::MEDIUM}
                                        text_color={color(&t.text)}>{item.label.clone()}</div>
                                    <div text_size={px(12.0)} text_color={palette.description}>
                                        {fields.join(" · ")}
                                    </div>
                                </div>
                            })
                            .child(rsx! {
                                <div id={open_id.clone()} h={px(s.control_height)} px={px(10.0)} flex_none
                                    flex items_center rounded={px(8.0)} border_1
                                    border_color={palette.control_border}
                                    bg={mix_hsla(palette.control, palette.control_hover, open_hover)}
                                    text_size={px(12.0)} text_color={color(&t.text_secondary)} cursor_pointer />
                            }
                                .on_hover(Self::hover_listener(open_id, cx))
                                .child(tr("打开", "Open"))
                                .on_click(cx.listener(move |this, _, window, cx| this.navigate(&target, window, cx)))),
                    );
                    continue;
                }
                if item.config_id == UPDATE_PAGE {
                    if let Some(page) = &self.update_page {
                        flow = flow.child(page.clone());
                    }
                    continue;
                }
                if item.config_id == DATA_PAGE {
                    // Search metadata never mounts a generic auto-saving
                    // dataFolder editor. Open the retained data page through
                    // the same save barrier, in this SettingsWindow/HWND.
                    let fields = self.schemas.get(DATA_PAGE)
                        .and_then(|schema| schema["fields"].as_array())
                        .into_iter().flatten()
                        .filter(|field| field["key"].as_str()
                            .is_some_and(|key| item.field_keys.contains(key)))
                        .collect::<Vec<_>>();
                    let open_data_id = SharedString::from("settings-open-data-result");
                    let open_data_hover = self.hover_progress(&open_data_id);
                    flow = flow.child(
                        rsx! {
                            <div id={"settings-result-dataLocation"} w_full min_w_0 flex_none flex
                                flex_col gap={px(10.0)} p={px(s.row_padding_x)}
                                rounded={px(s.card_radius)} border_1
                                border_color={palette.card_border} bg={palette.card}>
                                <div flex_none text_size={px(14.0)}
                                    font_weight={FontWeight::MEDIUM} text_color={color(&t.text)}>
                                    {item.label.clone()}
                                </div>
                            </div>
                        }
                            .children(fields.into_iter().map(|field| {
                                rsx! {
                                    <div flex_none flex flex_col gap={px(4.0)}>
                                        <div text_size={px(13.0)} text_color={color(&t.text)}>
                                            {field["label"].as_str().unwrap_or_default().to_string()}
                                        </div>
                                        <div text_size={px(12.0)} line_height={relative(1.45)}
                                            text_color={palette.description}>
                                            {field["description"].as_str().unwrap_or_default().to_string()}
                                        </div>
                                    </div>
                                }
                            }))
                            .child(rsx! {
                                <div id={open_data_id.clone()} h={px(s.control_height)}
                                    px={px(10.0)} flex_none flex items_center rounded={px(8.0)}
                                    border_1 border_color={palette.control_border}
                                    bg={mix_hsla(palette.control, palette.control_hover, open_data_hover)}
                                    text_size={px(12.0)} text_color={color(&t.text_secondary)}
                                    cursor_pointer />
                            }.on_hover(Self::hover_listener(open_data_id, cx))
                                .child(tr("打开数据管理", "Open data management"))
                                .on_click(cx.listener(|this, _, window, cx| this.navigate(DATA_PAGE, window, cx)))),
                    );
                    continue;
                }
                if let Some(form) = self.forms.get(&item.config_id) {
                    flow = flow.child(
                        rsx! {
                            <div id={SharedString::from(format!(
                                "settings-result-{}",
                                item.config_id
                            ))}
                                w_full min_w_0 flex flex_col gap={px(10.0)} />
                        }
                            .when(multiple_categories, |section| {
                                section.child(
                                    rsx! {
                                        <div text_size={px(14.0)} font_weight={FontWeight::MEDIUM}
                                            text_color={color(&t.text_secondary)}>
                                            {item.label.clone()}
                                        </div>
                                    },
                                )
                            })
                            .child(form.clone()),
                    );
                }
            }
        } else if self.active == UPDATE_PAGE {
            if let Some(page) = &self.update_page {
                flow = flow.child(page.clone());
            }
        } else if self.active == HOME_PAGE {
            if let Some(page) = &self.home_page {
                flow = flow.child(page.clone());
            }
        } else if self.active == DEVICES_PAGE {
            flow = flow.child(rsx! {
                <div w_full min_w_0 flex_none flex flex_col gap={px(6.0)}>
                    <div text_size={px(s.group_title_font_size)} font_weight={FontWeight::SEMIBOLD}>
                        {tr("跨设备", "Cross-device")}
                    </div>
                    <div text_size={px(s.description_font_size)} line_height={relative(1.6)}
                        text_color={color(&t.text_muted)}>
                        {tr("管理此设备与附近设备的连接，互传消息在剪贴板右侧收发。",
                            "Manage connections with nearby devices. Send and receive messages on the right of the clipboard window.")}
                    </div>
                </div>
            });
            if let Some(page) = &self.lan_page {
                flow = flow.child(page.clone());
            }
            #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
            if let Some(page) = &self.webdav_page {
                flow = flow.child(page.clone());
            }
        } else if self.active == DATA_PAGE {
            if let Some(page) = &self.data_page {
                flow = flow.child(page.clone());
            }
        } else {
            if self.active == "global" {
                if let Some(system) = self.forms.get(SYSTEM_PAGE) {
                    flow = flow.child(
                        rsx! {
                            <div id={"settings-system-section"} w_full min_w_0 flex_none>
                                {system.clone()}
                            </div>
                        },
                    );
                }
            }
            flow = flow.child(self.form.clone());
            if self.active == "menu_global" {
                if let Some(file) = self.forms.get("menu_file") {
                    flow = flow.child(
                        widgets::section(tr("文件菜单", "File menu"), s, t).relative()
                            .child(self.super_menu_sections.section_marker("files"))
                            .child(file.clone()),
                    );
                }
                let mut actions_section = widgets::section(tr("文本操作", "Text actions"), s, t)
                    .relative()
                    .child(self.super_menu_sections.section_marker("text-actions"))
                    .child(widgets::description(palette).child(tr(
                        "管理超级菜单中可用的文本操作，编辑后使用保存或取消。",
                        "Manage text actions available in Super menu. Save or cancel edits explicitly.",
                    )));
                if self.super_menu_actions_expanded && self.text_actions_manager {
                    if let Some(actions_form) = self.forms.get("textAction") {
                        let actions = actions_form.read(cx).text_actions.clone();
                        if let Some(actions) = actions {
                            actions.update(cx, |actions, cx| actions.set_embedded(true, cx));
                        }
                        actions_section = actions_section.child(actions_form.clone());
                    }
                } else {
                    actions_section = actions_section.child(
                        div().id("settings-super-menu-manage-text-actions")
                            .h(px(s.control_height)).px(px(10.0)).flex_none().flex().items_center()
                            .rounded(px(8.0)).border_1().border_color(palette.control_border)
                            .bg(palette.control).text_size(px(12.0)).cursor_pointer()
                            .child(tr("管理文本操作", "Manage text actions"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.super_menu_actions_expanded = true;
                                this.navigate("textAction", window, cx);
                            })),
                    );
                }
                flow = flow.child(actions_section);
            }
            if self.active == "user" {
                let help = widgets::row(true, false, s)
                    .child(widgets::description(palette).child(tr(
                        "在“打开”或“另存为”窗口中唤起文件菜单，再选择目标文件夹，即可切换到该目录。常用目录可以在文件菜单设置中维护。",
                        "Open the File menu while an Open or Save As dialog is active, then choose a folder to navigate there. Manage your usual folders in File menu settings.",
                    )))
                    .child(
                        rsx! {
                            <div id={"settings-explorer-file-menu"} min_h={px(s.control_height)}
                                px={px(10.0)} py={px(5.0)} flex_none flex items_center
                                rounded={px(8.0)} border_1 border_color={palette.control_border}
                                bg={palette.control} text_size={px(12.0)}
                                text_color={color(&t.text_secondary)} cursor_pointer />
                        }
                            .hover(|style| style.bg(palette.control_hover))
                            .child(tr("文件菜单设置", "File menu settings"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate("menu_file", window, cx);
                            })),
                    );
                let reopen = widgets::row(true, false, s)
                    .child(widgets::description(palette).child(tr(
                        "最近关闭的文件夹会出现在文件菜单的“打开的文件夹 → 重开”中。关闭记录后，已有历史仍可使用；数量设置会自动保存。",
                        "Recently closed folders appear under Open folders → Reopen in the File menu. Turning recording off keeps existing history available. Limits save automatically.",
                    )));
                flow = flow.child(
                    widgets::section(tr("文件夹跳转与重开", "Folder navigation and reopening"), s, t)
                        .child(widgets::card([help.into_any_element(), reopen.into_any_element()], s, palette)),
                );
            }
        }
        let status_error = self
            .forms
            .values()
            .find_map(|form| form.read(cx).persistence_error());
        let categories_visible = if searching {
            self.search_results.iter().any(|item| {
                item.config_id == "clip"
                    && item.field_keys.contains("FilterItems")
                    && settings_scope_includes(self.search_query.scope(), &item.config_id)
            })
        } else {
            self.active == "clip"
        };
        let status = if let Some(error) = &status_error {
            format!("{}{error}", tr("保存失败：", "Save failed: "))
        } else if self.pending_writes(cx) || self.leave.is_some() {
            tr("正在保存…", "Saving…").into()
        } else if !searching && self.active == DATA_PAGE {
            tr("数据位置需点击保存并确认", "Save and confirm data location changes").into()
        } else if !searching && self.active == "menu_global" && self.super_menu_actions_expanded {
            tr("选项自动保存；文本操作需点击保存", "Options save automatically; save text actions explicitly").into()
        } else if categories_visible {
            tr("分类标签需点击保存", "Save category tabs explicitly").into()
        } else {
            tr("修改后自动保存", "Changes save automatically").into()
        };
        let undo_enabled = self.can_undo(cx);
        let undo_id = SharedString::from("settings-undo");
        let undo_hover = if undo_enabled {
            self.hover_progress(&undo_id)
        } else {
            0.0
        };
        let footer = rsx! {
            <div flex_none min_h={px(44.0)} px={px(padding)} py={px(8.0)} flex items_center
                gap={px(8.0)} border_t_1 border_color={palette.card_border} bg={palette.page}
                text_size={px(12.0)} />
        }
            .text_color(color(if status_error.is_some() { &t.error } else { &t.text_muted }))
            .child(div().size(px(5.0)).rounded_full().bg(color(if status_error.is_some() { &t.error } else if self.pending_writes(cx) || self.leave.is_some() { &t.text_muted } else { &t.accent })))
            .child(rsx! {
                <div flex_1 min_w_0>
                    {status}
                </div>
            })
            .children((!matches!(self.active.as_str(), DATA_PAGE | DEVICES_PAGE) || searching).then(|| rsx! {
                <div id={undo_id.clone()} h={px(26.0)} px={px(9.0)} flex items_center flex_none
                    rounded={px(7.0)} border_1 border_color={palette.control_border}
                    bg={mix_hsla(palette.control, palette.control_hover, undo_hover)}
                    text_color={color(&t.text_secondary)} />
            }
                .when(!undo_enabled, |button| button.opacity(0.45))
                .when(undo_enabled, |button| button.cursor_pointer().on_hover(Self::hover_listener(undo_id, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.undo_to_session_baseline(window, cx))))
                .child(tr("撤销修改", "Undo changes"))
                .tooltip(settings_help(tr("恢复为进入当前分类时的配置，包含已自动保存的修改；切换分类后更新基准。", "Restore settings from when this category was opened, including saved changes. Switching categories resets the baseline.").into(), t.clone()))))
            .child(rsx! {
                <div id={"settings-save-help"} flex_none cursor_pointer>
                    {"ⓘ"}
                </div>
            }
                .tooltip(settings_help(if !searching && self.active == DATA_PAGE {
                    tr("数据位置仅在点击保存并确认后更改。切换分类保留本次设置会话的路径草稿；关闭不会自动提交目录更改。", "Data location changes only after Save and confirmation. Switching categories retains the path draft for this settings session; closing does not submit it.")
                } else if categories_visible {
                    tr("分类标签仅在点击分类编辑器的保存按钮后更新。其余选项自动保存；关闭设置不会自动提交分类草稿。", "Category tabs update only when you click Save in the category editor. Other options save automatically; closing settings does not submit category drafts.")
                } else {
                    tr("每个字段独立保存。输入框失去焦点时保存；切换分类、关闭或退出时等待所有已打开设置保存完成。", "Each field saves separately. Text saves when it loses focus. Switching categories, closing or exiting waits for all opened settings to save.")
                }.into(), t.clone())));
        // Overlay bar shared with the history list (DESIGN 2026-10-14 tokens
        // `scrollbar`): the WebView settings page shows a scrollbar too.
        let scrollbar_owner = cx.weak_entity();
        let content_scrollbar = self.scrollbar.render(
            "settings-content-scrollbar",
            ScrollbarTarget::Scroll(self.scroll.clone()),
            0,
            ScrollbarStyle {
                ink: color(&t.text),
                metrics: tokens.scrollbar,
                always_visible: false,
            },
            std::rc::Rc::new(move |_, cx| {
                let _ = scrollbar_owner.update(cx, |this, cx| this.cancel_super_menu_section_jump(cx));
            }),
        );
        let full_bleed_body = full_bleed.then(|| if self.text_actions_manager {
            self.text_actions_manager_body(cx)
        } else if self.active == "user" {
            self.reopen_page.clone().map_or_else(|| div().into_any_element(), |page| page.into_any_element())
        } else {
            self.data_page.clone().map_or_else(|| div().into_any_element(), |page| page.into_any_element())
        });
        let content = if let Some(body) = full_bleed_body {
            rsx! { <div flex_1 min_w_0 h_full flex flex_col overflow_hidden /> }
                .children(self.window_message.as_ref().map(|message| {
                    widgets::note(message.clone(), palette).py(px(8.0)).text_color(color(&t.error))
                }))
                .child(rsx! { <div flex_1 min_w_0 min_h_0 flex flex_col overflow_hidden /> }.child(body))
        } else { rsx! {
            <div flex_1 min_w_0 h_full flex flex_col />
        }
            .children((!searching && self.active == "menu_global").then(|| self.render_super_menu_tabs(window, cx)))
            .child(
                rsx! {
                    <div relative flex_1 min_w_0 min_h_0 flex flex_col />
                }
                    .child(
                        rsx! {
                            <div id={"settings-content"} flex_1 min_w_0 min_h_0 />
                        }
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .smooth_wheel("settings-content", &self.scroll, cx)
                            .on_scroll_wheel(cx.listener(|this, _, _, cx| this.cancel_super_menu_section_jump(cx)))
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.cancel_super_menu_section_jump(cx)))
                            .px(px(padding))
                            .pt(px(20.0))
                            .pb(px(s.content_padding_y))
                            .children((!searching && self.active == "menu_global").then(|| self.super_menu_scroll_probe(cx)))
                            .child(flow),
                    )
                    .child(content_scrollbar),
            )
            .when(searching || !matches!(self.active.as_str(), HOME_PAGE | DEVICES_PAGE | "user"), |content| content.child(footer)) };
        if self.motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        rsx! {
            <div id={"settings-window"} size_full flex flex_col relative overflow_hidden
                bg={color(&self.base_tokens.theme(self.dark).background)}
                text_color={color(&t.text)} font_family={tokens.typography.font_family.clone()} />
        }
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !(this.text_actions_manager && !this.search_query.is_active() && event.keystroke.key == "tab") {
                    this.key_down(event, window, cx);
                }
            }))
            .children(wallpaper_layer)
            .children(wallpaper_tint)
            .child(titlebar)
            .child(
                rsx! {
                    <div flex flex_1 min_h_0>
                        {sidebar}
                        {content}
                    </div>
                },
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_window_pages_group_navigation_and_join_search() {
        let mut pages: Vec<(String, String)> = [HELP_PAGE, "global", DATA_PAGE, DEVICES_PAGE, HOME_PAGE, UPDATE_PAGE, TEXT_ACTIONS_PAGE, "clip"]
            .into_iter().map(|id| (id.to_string(), id.to_string())).collect();
        pages.sort_by_key(|(id, _)| settings_nav_group(id));
        assert_eq!(
            pages.iter().filter(|(id, _)| sidebar_settings_page(id)).map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![HOME_PAGE, "global", DEVICES_PAGE, "clip", HELP_PAGE, DATA_PAGE, UPDATE_PAGE]
        );
        assert!(!sidebar_settings_page(TEXT_ACTIONS_PAGE));
        assert!(!sidebar_settings_page("textAction"));
        assert!(!sidebar_settings_page(LAN_PAGE));
        for (route, page) in [("/main", HOME_PAGE), ("/main/sync/devices", HOME_PAGE), ("/docs", HELP_PAGE),
            ("/main/settings/devices", DEVICES_PAGE), ("/main/settings/lan", DEVICES_PAGE),
            ("/main/settings/deviceSync", DEVICES_PAGE), ("/settings/DEVICES?from=tray", DEVICES_PAGE),
            ("/main/settings/textActions", "textAction")] {
            assert_eq!(settings_page_from_route(route).as_deref(), Some(page), "{route}");
        }
        for id in [LAN_PAGE, "deviceSync", DEVICES_PAGE] {
            assert_eq!(settings_navigation_id(id), DEVICES_PAGE);
        }
        let mut index = SettingsSearchIndex::default();
        for id in [HOME_PAGE, DEVICES_PAGE, HELP_PAGE, TEXT_ACTIONS_PAGE] {
            assert!(native_settings_page(id) && searchable_settings_page(id));
            index.upsert(id, id, &main_page_search_schema(id).unwrap());
        }
        let order = [HOME_PAGE, DEVICES_PAGE, HELP_PAGE, TEXT_ACTIONS_PAGE].map(String::from).to_vec();
        for (query, page) in [("互传", DEVICES_PAGE), ("同步", DEVICES_PAGE), ("配对码", DEVICES_PAGE),
            ("托盘", HOME_PAGE), ("重启", HOME_PAGE), ("命令", HELP_PAGE), ("正则", TEXT_ACTIONS_PAGE)] {
            assert_eq!(index.matches(query, &order).first().map(|item| item.config_id.as_str()), Some(page), "{query}");
        }
    }

    #[test]
    fn about_update_routes_and_search_use_dedicated_page_metadata() {
        for route in ["/main/settings/about", "/settings/ABOUT?from=tray"] {
            assert_eq!(settings_page_from_route(route).as_deref(), Some(UPDATE_PAGE));
        }
        let schema = updates::search_schema();
        let mut index = SettingsSearchIndex::default();
        index.upsert(UPDATE_PAGE, "About", &schema);
        let order = vec![UPDATE_PAGE.to_string()];
        for query in ["更新", "update", "安装", "数据", "release notes", "Program Files"] {
            let matches = index.matches(query, &order);
            assert_eq!(matches.len(), 1, "{query}");
            assert_eq!(matches[0].config_id, UPDATE_PAGE);
        }
        assert!(searchable_settings_page(UPDATE_PAGE));
    }

    #[test]
    fn super_menu_navigation_combines_counts_without_changing_config_routes() {
        for (config, section) in [("menu_global", "general"), ("menu_file", "files"), ("textAction", "text-actions")] {
            assert_eq!(settings_navigation_id(config), "menu_global");
            assert_eq!(super_menu_route_section(config), Some(section));
            assert_eq!(settings_page_from_route(&format!("/main/settings/{config}")).as_deref(), Some(config));
            assert!(settings_scope_includes(Some("menu_global"), config));
        }
        // `show(Some("textActions"))` and a resolved HTTP deep link must
        // initialize the same retained manager, not a native placeholder page.
        for requested in [TEXT_ACTIONS_PAGE, "textAction"] {
            let resolved = settings_page_from_route(&format!("/main/settings/{requested}")).unwrap();
            assert_eq!(resolved, "textAction");
            assert_eq!(settings_navigation_id(requested), settings_navigation_id(&resolved));
            assert_eq!(settings_navigation_id(requested), "menu_global");
            assert_eq!(super_menu_route_section(requested), Some("text-actions"));
            assert_eq!(super_menu_route_section(requested), super_menu_route_section(&resolved));
        }
        assert_eq!(settings_navigation_id("user"), "user");
        assert!(!settings_scope_includes(Some("menu_global"), "user"));
        assert_eq!(settings_navigation_match_count("menu_global", [("menu_global", 4), ("menu_file", 3), ("clip", 8)]), 7);
        let pages = vec![("menu_global".into(), "超级菜单".into()), ("textAction".into(), "文本操作".into())];
        assert!(settings_schema_pages(&pages, Some("menu_global")).iter().any(|(id, _)| id == "textAction"));
    }

    #[test]
    fn super_menu_field_projection_preserves_every_field_and_separates_window_options() {
        let fields = cliphost_core::extra_settings::menu_global_fields(&Default::default());
        let sections = fields.iter().map(|field| super_menu_field_section(field["key"].as_str().unwrap()).1).collect::<Vec<_>>();
        assert_eq!(sections.iter().filter(|id| **id == "general").count(), 5);
        assert_eq!(sections.iter().filter(|id| **id == "items").count(), 3);
        assert_eq!(sections.iter().filter(|id| **id == "windows").count(), 4);
        assert_eq!(super_menu_field_section("FutureSetting").1, "general");
        assert_eq!(super_menu_field_section("ShowWindowMenuItem").1, "windows");
        assert_eq!(super_menu_field_section("ShowWindowThumbnail").1, "windows");
    }

    #[test]
    fn super_menu_category_search_includes_file_fields_under_their_original_identity() {
        let mut index = SettingsSearchIndex::default();
        index_initialized_form(&mut index, "menu_file", "文件菜单", true, &[
            json!({"key":"Files", "label":"自定义目录", "value":["C:/fixture"]}),
        ]);
        let matches = index.matches(tr("超级菜单", "Super menu"), &["menu_file".into()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].config_id, "menu_file");
        assert!(matches[0].field_keys.contains("Files"));
        let schema = json!({"configLabel":"文件菜单", "fields":[
            {"key":"Files", "label":"自定义目录", "value":["C:/fixture"]}
        ]});
        index_settings_schema(&mut index, "menu_file", "fallback", &schema);
        let matches = index.matches(tr("超级菜单", "Super menu"), &["menu_file".into()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].config_id, "menu_file");
        assert_eq!(schema["configLabel"], "文件菜单");
    }

    #[test]
    fn file_menu_line_reordering_uses_stable_input_identity_after_rows_shift() {
        // Repeated path text and separator rows have distinct entity IDs.
        assert_eq!(line_reorder_destination(&[10, 11, 12, 13], 12, 10, true), Some((2, 0)));
        assert_eq!(line_reorder_destination(&[11, 12, 13], 12, 13, false), Some((1, 2)));
        assert_eq!(line_reorder_destination(&[11, 12, 13], 10, 13, false), None);
        assert_eq!(line_reorder_destination(&[11, 12, 13], 12, 10, true), None);
        assert_eq!(line_reorder_destination(&[11, 12, 13], 12, 12, false), None);
        assert_eq!(line_reorder_destination(&[11, 12, 13], 12, 13, true), None);
    }

    #[test]
    fn file_menu_line_drag_rejects_other_forms_fields_and_reloaded_pages() {
        let drag = SettingsLineDrag { owner: 7, config_id: "fileMenu".into(), generation: 2,
            field: "fileMenuPaths".into(), input: 101, title: "---".into(),
            background: gpui::rgb(0xffffff).into(), foreground: gpui::rgb(0x000000).into() };
        assert!(drag.belongs_to(7, "fileMenu", 2, "fileMenuPaths"));
        assert!(!drag.belongs_to(8, "fileMenu", 2, "fileMenuPaths"));
        assert!(!drag.belongs_to(7, "global", 2, "fileMenuPaths"));
        assert!(!drag.belongs_to(7, "fileMenu", 3, "fileMenuPaths"));
        assert!(!drag.belongs_to(7, "fileMenu", 2, "anotherField"));
    }

    #[test]
    fn slider_schema_uses_a_retained_range_control_and_rejects_a_reversed_domain() {
        let field = json!({
            "inputType":"number", "formEditor":"slider", "value":70,
            "min":0, "max":100, "step":1, "stretchControl":true,
        });
        assert_eq!(kind(&field), Kind::Slider);
        assert_eq!(slider_domain(&field), SliderDomain::new(0.0, 100.0, 1.0));
        assert_eq!(kind(&json!({"inputType":"number", "value":70})), Kind::Number);
        assert!(slider_domain(&json!({"min":100, "max":0, "step":1})).is_none());
    }

    #[test]
    fn numeric_step_uses_the_live_draft_and_clamps_before_applying_delta() {
        let field = json!({"value":10,"min":0,"max":50,"step":1});
        assert_eq!(stepped_number(&field, "25", 1.0), Some(json!(26)));
        assert_eq!(stepped_number(&field, "26", 1.0), Some(json!(27)));
        assert_eq!(stepped_number(&field, "100", -1.0), Some(json!(49)));
        assert_eq!(stepped_number(&field, "-100", 1.0), Some(json!(1)));
        let fractional_step = json!({"value":10,"step":0.5});
        assert_eq!(
            stepped_number(&fractional_step, "25.6", 1.0),
            Some(json!(26))
        );
    }

    #[test]
    fn numeric_step_does_not_accept_empty_invalid_or_nonfinite_drafts() {
        let field = json!({"value":10,"step":1});
        for draft in ["", " ", "-", "invalid", "NaN", "inf", "1e309"] {
            assert_eq!(stepped_number(&field, draft, 1.0), None, "{draft}");
        }
    }

    #[test]
    fn settings_tab_sequence_moves_both_directions_and_wraps_at_window_boundaries() {
        assert_eq!(next_settings_focus(Some(1), 3, false), Some(2));
        assert_eq!(next_settings_focus(Some(2), 3, false), Some(0));
        assert_eq!(next_settings_focus(Some(0), 3, true), Some(2));
        assert_eq!(next_settings_focus(None, 3, false), Some(0));
        assert_eq!(next_settings_focus(None, 3, true), Some(2));
        assert_eq!(next_settings_focus(None, 0, false), None);
    }

    #[test]
    fn ordinary_enums_leave_dedicated_theme_and_panel_style_controls_intact() {
        assert!(ordinary_settings_choice("global", "UiLanguage"));
        assert!(ordinary_settings_choice("trigger", "ShowWindowLocation"));
        assert!(!ordinary_settings_choice("global", "UiThemeMode"));
        assert!(!ordinary_settings_choice("panel", "PanelStyle"));
    }

    #[test]
    fn settings_background_refresh_derives_once_from_base_and_restores_after_failure() {
        let mut base = DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        base.themes.light.surface = "#12345680".into();
        let options = WindowBackgroundOptions {
            path: "C:/new-image-loading.png".into(),
            ..Default::default()
        };
        let first = settings_window_tokens(&base, &options, true);
        let repeated = settings_window_tokens(&base, &options, true);
        assert_eq!(first.themes.light, repeated.themes.light);
        assert_eq!(first.themes.light.surface, "#12345625");
        assert_eq!(base.themes.light.surface, "#12345680");
        // Latest decode failure clears the snapshot even with a nonempty path.
        let restored = settings_window_tokens(&base, &options, false);
        assert_eq!(restored.themes.light, base.themes.light);
        assert_eq!(restored.themes.dark, base.themes.dark);
    }

    #[test]
    fn image_file_schema_uses_path_editor_even_for_an_invalid_multiline_value() {
        let field =
            json!({"inputType":"text", "formEditor":"imagefile", "value":"C:/old\ninvalid"});
        assert_eq!(kind(&field), Kind::ImageFile);
        assert_eq!(
            kind(&json!({"inputType":"text", "value":"C:/image.png"})),
            Kind::Text
        );
        assert_eq!(
            kind(&json!({"inputType":"folder", "value":"C:/folder"})),
            Kind::Folder
        );
    }

    #[test]
    fn image_picker_cancellation_never_saves_a_leftover_or_empty_path() {
        for result in [
            json!({"cancelled":true,"path":"C:/stale.png"}),
            json!({"cancelled":true,"path":null}),
            json!({"cancelled":false,"path":" "}),
            json!({"cancelled":false,"path":null}),
        ] {
            assert!(selected_picker_path(&result).is_none());
        }
        let selected = json!({"cancelled":false,"path":"C:/WindowBackground/图片 😀.png"});
        assert_eq!(
            selected_picker_path(&selected),
            Some("C:/WindowBackground/图片 😀.png")
        );
    }

    #[test]
    fn image_clear_retires_picker_before_start_and_keeps_modal_slot_until_completion() {
        let key = "WindowBackgroundImagePath";
        let mut requests = FolderPickerRequests::default();
        let lease = requests.accept(key, false).unwrap();
        // An explicit Clear retires even when the prior raw value was empty;
        // resetting the text back to its captured value never revives it.
        requests.retire_key(key);
        assert!(
            run_folder_picker(&lease, || panic!("cleared image picker must not start")).is_none()
        );
        assert!(!requests.browse_enabled(key, false));
        assert!(requests.accept(key, false).is_none());
        assert_eq!(requests.finish(key, &lease), Some(false));
        let retry = requests.accept(key, false).unwrap();
        assert_eq!(requests.finish(key, &lease), None);
        assert!(retry.current());
        assert_eq!(requests.finish(key, &retry), Some(true));
    }

    #[test]
    fn retired_system_route_cannot_revive_on_later_global_navigation() {
        for alias in ["homeGlobal", "system", "SYSTEM"] {
            // A failed leave retained this route until a search/new navigation.
            let mut pending = Some(format!("/main/settings/{alias}"));
            retire_system_route(&mut pending);
            assert!(pending.is_none());
            // A new route must be installed after retiring the old request.
            pending = Some("/main/settings/homeGlobal".into());
            assert_eq!(
                pending
                    .as_deref()
                    .and_then(settings_page_from_route)
                    .as_deref(),
                Some(SYSTEM_PAGE)
            );
        }
        let mut action = Some("/main/settings/textAction?actionId=owned-draft".into());
        retire_system_route(&mut action);
        assert!(action.is_some());
    }

    #[test]
    fn main_window_default_preserves_minimum_and_explicit_larger_height() {
        assert_eq!(settings_default_size(580.0, 560.0, 420.0), (900.0, 640.0));
        assert_eq!(settings_default_size(700.0, 560.0, 420.0), (900.0, 700.0));
        assert_eq!(settings_default_size(580.0, 960.0, 650.0), (960.0, 650.0));
    }

    #[test]
    fn merged_global_search_keeps_system_config_identity_and_combines_navigation_count() {
        let hits = vec![("global", 3), (SYSTEM_PAGE, 2), ("clip", 7)];
        assert_eq!(settings_navigation_id(SYSTEM_PAGE), "global");
        assert_eq!(settings_navigation_id("system"), "global");
        assert_eq!(
            settings_navigation_match_count("global", hits.iter().copied()),
            5
        );
        assert_eq!(
            settings_navigation_match_count("global", [(SYSTEM_PAGE, 2)]),
            2
        );
        let scoped: Vec<_> = hits
            .iter()
            .copied()
            .filter(|(id, _)| settings_scope_includes(Some("global"), id))
            .collect();
        assert_eq!(scoped, vec![("global", 3), ("homeGlobal", 2)]);
        assert!(settings_scope_includes(Some(SYSTEM_PAGE), "global"));
        assert!(!settings_scope_includes(Some("clip"), SYSTEM_PAGE));
    }

    #[test]
    fn compact_theme_choice_uses_full_width_without_changing_other_choice_rows() {
        assert!(full_width_settings_choice("global", "UiThemeMode"));
        assert!(!full_width_settings_choice("global", "UiLanguage"));
        assert!(!full_width_settings_choice("trigger", "ShowWindowLocation"));
    }

    #[test]
    fn legacy_system_routes_still_resolve_to_the_original_form_id() {
        assert_eq!(
            settings_page_from_route("/main/settings/homeGlobal").as_deref(),
            Some(SYSTEM_PAGE)
        );
        assert_eq!(
            settings_page_from_route("/main/settings/system").as_deref(),
            Some(SYSTEM_PAGE)
        );
        assert_eq!(
            settings_page_from_route("/settings/SYSTEM?from=old").as_deref(),
            Some(SYSTEM_PAGE)
        );
    }

    #[test]
    fn settings_undo_keeps_saved_edits_until_a_new_category_session() {
        let mut baseline = SettingsUndoBaseline::default();
        baseline.reset(&[json!({"key":"UiThemeMode","value":"light"})]);
        // Local optimistic state is already dark when its persisted event or
        // save receipt arrives. Neither acknowledgement ends the session.
        baseline.observe_authoritative("UiThemeMode", &json!("dark"), &json!("dark"), true);
        assert_eq!(baseline.values["UiThemeMode"], "light");
        let saved = vec![json!({"key":"UiThemeMode","value":"dark"})];
        baseline.reset(&saved); // Successful navigation to another category.
        assert_eq!(baseline.values["UiThemeMode"], "dark");
    }

    #[test]
    fn settings_undo_preserves_foreign_clean_updates_without_rebasing_local_drafts() {
        let mut baseline = SettingsUndoBaseline::default();
        baseline.reset(&[json!({"key":"Font","value":"original"})]);
        baseline.observe_authoritative("Font", &json!("original"), &json!("external"), true);
        assert_eq!(baseline.values["Font"], "external");
        baseline.observe_authoritative("Font", &json!("external"), &json!("later external"), false);
        assert_eq!(baseline.values["Font"], "external"); // Protected raw input.
        baseline.observe_authoritative("Font", &json!("local saved"), &json!("local saved"), true);
        assert_eq!(baseline.values["Font"], "external");
    }

    #[test]
    fn settings_undo_compares_fresh_persistence_and_retries_equal_failed_intent() {
        let target = json!("C:/entered");
        // The focused raw draft kept fields.value at C:/entered while an API
        // changed persistence; undo still requires a compensation write.
        assert!(undo_write_required(&json!("C:/api-new"), &target, false));
        assert!(!undo_write_required(&target, &target, false));
        // A failed rollback cannot be acknowledged because its UI is equal.
        assert!(undo_write_required(&target, &target, true));
    }

    #[test]
    fn settings_undo_targets_survive_invalid_number_and_added_blank_line_drafts() {
        let mut baseline = SettingsUndoBaseline::default();
        baseline.reset(&[
            json!({"key":"Count","value":25}),
            json!({"key":"Files","value":"C:/one\r\nD:/二 😀"}),
        ]);
        let number = text_value(&json!({"value":baseline.values["Count"]}));
        assert_ne!("abc", number); // Undo does not parse the invalid draft.
        assert_eq!(number, "25");
        let paths = text_value(&json!({"value":baseline.values["Files"]}));
        assert!(!raw_lines_match_accepted(
            &["C:/one", "D:/二 😀", ""],
            &paths
        ));
        assert_eq!(
            paths.lines().collect::<Vec<_>>(),
            vec!["C:/one", "D:/二 😀"]
        );
    }

    #[test]
    fn settings_undo_restores_theme_after_color_side_effects_even_when_initially_equal() {
        let mut baseline = SettingsUndoBaseline::default();
        baseline.reset(&[
            json!({"key":"UiThemeMode","value":"auto"}),
            json!({"key":"WindowBackgroundColor","value":"#FFFFFFFF"}),
        ]);
        let mut plan = vec![("WindowBackgroundColor".into(), json!("#FFFFFFFF"), true)];
        order_undo_values("global", &mut plan, &baseline);
        assert_eq!(
            plan.last(),
            Some(&("UiThemeMode".into(), json!("auto"), true))
        );
        let mut theme = "auto".to_string();
        for (key, value, _) in plan {
            match key.as_str() {
                "WindowBackgroundColor" => theme = "Custom".into(),
                "UiThemeMode" => theme = value.as_str().unwrap().into(),
                _ => {}
            }
        }
        assert_eq!(theme, "auto");
    }

    #[test]
    fn settings_undo_later_receipts_preserve_failed_color_and_pending_category_targets() {
        let current = vec![
            json!({"key":"WindowBackgroundColor","value":"#FFFFFFFF"}),
            json!({"key":"FilterItems","value":"baseline categories"}),
        ];
        let mut response = json!({"fields":[
            {"key":"WindowBackgroundColor","value":"#FF000000"},
            {"key":"FilterItems","value":"previous categories"},
            {"key":"UiThemeMode","value":"auto"},
        ]});
        let protected = HashSet::from(["WindowBackgroundColor".into(), "FilterItems".into()]);
        merge_protected_reply_values(&mut response, &current, &protected);
        assert_eq!(response["fields"][0]["value"], "#FFFFFFFF");
        assert_eq!(response["fields"][1]["value"], "baseline categories");
        assert_eq!(response["fields"][2]["value"], "auto");
    }

    #[test]
    fn authoritative_theme_refresh_preserves_a_different_fields_draft_baseline() {
        let mut current = vec![
            json!({"key":"UiThemeMode","value":"light","label":"界面主题"}),
            json!({"key":"FontFamilyString","value":"accepted font","label":"字体"}),
        ];
        let fresh = vec![
            json!({"key":"UiThemeMode","value":"dark","label":"Theme"}),
            json!({"key":"FontFamilyString","value":"external font"}),
        ];
        let ready = HashSet::from(["UiThemeMode".to_string()]);
        for (index, _, source) in authoritative_field_updates(&current, &fresh, &ready) {
            current[index]["value"] = source["value"].clone();
        }
        assert_eq!(current[0]["value"], "dark");
        assert_eq!(current[0]["label"], "界面主题");
        assert_eq!(current[1]["value"], "accepted font");
    }

    #[test]
    fn focused_clean_field_keeps_baseline_until_blur_then_reads_latest_value() {
        let current = vec![json!({"key":"Folder","value":"C:/accepted"})];
        let first = vec![json!({"key":"Folder","value":"C:/external-first"})];
        assert!(!can_normalize_input(
            "C:/accepted",
            "C:/accepted",
            false,
            true
        ));
        assert!(authoritative_field_updates(&current, &first, &HashSet::new()).is_empty());
        // An unchanged blur compares with its old baseline, so it has no save.
        assert_eq!(current[0]["value"], "C:/accepted");
        assert!(can_normalize_input(
            "C:/accepted",
            "C:/accepted",
            false,
            false
        ));
        let latest = vec![json!({"key":"Folder","value":"C:/external-latest"})];
        let ready = HashSet::from(["Folder".to_string()]);
        let updates = authoritative_field_updates(&current, &latest, &ready);
        assert_eq!(updates[0].2["value"], "C:/external-latest");
    }

    #[test]
    fn authoritative_refresh_acknowledges_only_successful_idle_keys() {
        let current = vec![
            json!({"key":"Failed","value":"retained failed draft"}),
            json!({"key":"Idle","value":false}),
            json!({"key":"Missing","value":"retained baseline"}),
        ];
        let fresh = vec![
            json!({"key":"Failed","value":"external"}),
            json!({"key":"Idle","value":true}),
            json!({"key":"Missing"}),
        ];
        let ready = HashSet::from(["Idle".to_string(), "Missing".to_string()]);
        let mut pending = HashSet::from([
            "Failed".to_string(),
            "Idle".to_string(),
            "Missing".to_string(),
        ]);
        for (_, key, _) in authoritative_field_updates(&current, &fresh, &ready) {
            pending.remove(&key);
        }
        assert_eq!(
            pending,
            HashSet::from(["Failed".to_string(), "Missing".to_string()])
        );
        assert_eq!(current[0]["value"], "retained failed draft");
    }

    #[test]
    fn raw_line_refresh_does_not_erase_added_empty_rows_or_whitespace_edits() {
        assert!(raw_lines_match_accepted(
            &["原目录 😀", "D:/二"],
            "原目录 😀\r\nD:/二"
        ));
        assert!(!raw_lines_match_accepted(
            &["原目录 😀", "D:/二", ""],
            "原目录 😀\nD:/二"
        ));
        assert!(!raw_lines_match_accepted(&[" C:/one "], "C:/one"));
        assert!(raw_lines_match_accepted(&[], ""));
        assert!(!raw_lines_match_accepted(&[""], ""));
    }

    #[test]
    fn settings_search_includes_data_metadata_and_skips_other_dedicated_schemas() {
        let pages = vec![
            ("global".into(), "全局".into()),
            (SYSTEM_PAGE.into(), "系统".into()),
            ("clip".into(), "剪贴板".into()),
            (DATA_PAGE.into(), "数据管理".into()),
            ("textAction".into(), "文本操作".into()),
            ("mobileSync".into(), "手机同步".into()),
        ];
        let search_pages = settings_schema_pages(&pages, None);
        assert_eq!(
            search_pages
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["global", SYSTEM_PAGE, "clip", DATA_PAGE]
        );
        let text_page = settings_schema_pages(&pages, Some("textAction"));
        assert!(text_page.iter().any(|(id, _)| id == "textAction"));
        assert!(settings_schema_pages(&pages, Some(DATA_PAGE))
            .iter()
            .any(|(id, _)| id == DATA_PAGE));
        let mut index = SettingsSearchIndex::default();
        let fields = vec![json!({"key":"Folder","label":"文件夹"})];
        for (id, label) in &pages {
            index.upsert(id, label, &json!({"fields":fields})); // Simulate a preexisting index.
            index_initialized_form(&mut index, id, label, true, &fields);
        }
        let order = pages.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let hits = index.matches("文件夹", &order);
        assert_eq!(
            hits.iter()
                .map(|item| item.config_id.as_str())
                .collect::<Vec<_>>(),
            vec!["global", SYSTEM_PAGE, "clip", DATA_PAGE]
        );
    }

    #[test]
    fn data_search_matches_page_title_and_location_field_metadata() {
        let mut index = SettingsSearchIndex::default();
        index.upsert(DATA_PAGE, "数据管理", &json!({
            "configLabel":"数据管理",
            "fields":[{
                "key":"dataFolder", "label":"数据文件夹",
                "description":"更改位置不会自动迁移数据；保存时确认并重启。"
            }]
        }));
        for query in ["数据管理", "文件夹", "迁移数据", "dataFolder"] {
            let hits = index.matches(query, &[DATA_PAGE.into()]);
            assert_eq!(hits.len(), 1, "{query}");
            assert_eq!(hits[0].config_id, DATA_PAGE);
            assert_eq!(hits[0].field_keys, HashSet::from(["dataFolder".to_string()]));
        }
        assert_eq!(settings_navigation_id(DATA_PAGE), DATA_PAGE);
        assert!(settings_scope_includes(Some(DATA_PAGE), DATA_PAGE));
    }

    #[test]
    fn folder_picker_busy_feedback_stays_disabled_until_its_lease_settles() {
        let mut requests = FolderPickerRequests::default();
        assert!(requests.browse_enabled("Folder", false));
        assert!(!requests.browse_enabled("Folder", true));
        let lease = requests.accept("Folder", false).unwrap();
        assert!(requests.is_pending("Folder"));
        assert!(!requests.browse_enabled("Folder", false));
        assert!(requests.browse_enabled("OtherFolder", false));
        requests.retire_key("Folder");
        assert!(!requests.browse_enabled("Folder", false));
        assert_eq!(requests.finish("Folder", &lease), Some(false));
        assert!(requests.browse_enabled("Folder", false));
        let retry = requests.accept("Folder", false).unwrap();
        assert_eq!(requests.finish("Folder", &lease), None);
        assert!(!requests.browse_enabled("Folder", false));
        assert_eq!(requests.finish("Folder", &retry), Some(true));
        assert!(requests.browse_enabled("Folder", false));
    }

    #[test]
    fn locked_placeholder_keeps_cached_search_schema_until_same_form_can_initialize() {
        let mut index = SettingsSearchIndex::default();
        let schema = json!({"fields":[{"key":"HistoryLimit","label":"历史上限"}]});
        let order = vec!["clip".into()];
        index.upsert("clip", "剪贴板", &schema);
        assert_eq!(schema_application(false, true), SchemaApplication::Defer);
        index_initialized_form(&mut index, "clip", "剪贴板", false, &[]);
        assert_eq!(index.matches("历史", &order)[0].count(), 1);
        assert_eq!(
            schema_application(false, false),
            SchemaApplication::Initialize
        );
        index_initialized_form(
            &mut index,
            "clip",
            "剪贴板",
            true,
            schema["fields"].as_array().unwrap(),
        );
        assert_eq!(index.matches("历史", &order)[0].count(), 1);
        assert_eq!(schema_application(true, false), SchemaApplication::Metadata);
    }

    #[test]
    fn initialized_empty_or_dirty_form_metadata_never_requests_reinitialization() {
        assert_eq!(schema_application(true, true), SchemaApplication::Metadata);
        assert_eq!(schema_application(true, false), SchemaApplication::Metadata);
        let mut index = SettingsSearchIndex::default();
        index.upsert(
            "textAction",
            "文本操作",
            &json!({"fields":[{"key":"old","label":"旧字段"}]}),
        );
        index_initialized_form(&mut index, "textAction", "文本操作", true, &[]);
        assert!(index.matches("旧字段", &["textAction".into()]).is_empty());
        // A failed first GET remains a placeholder and can hydrate on retry.
        assert_eq!(
            schema_application(false, false),
            SchemaApplication::Initialize
        );
    }

    #[test]
    fn hidden_input_late_normalization_retains_unsubmitted_and_invalid_drafts() {
        assert!(!can_normalize_input(
            "typed after save",
            "accepted",
            false,
            false
        ));
        assert!(!can_normalize_input("7oops", "7", false, false));
        assert!(!can_normalize_input("组合输入", "组合输入", true, false));
        assert!(!can_normalize_input("accepted", "accepted", false, true));
        assert!(can_normalize_input("accepted", "accepted", false, false));
    }

    #[test]
    fn search_schema_late_locale_retry_and_closed_window_replies_are_retired() {
        assert!(schema_reply_current(3, 3, true, false));
        assert!(!schema_reply_current(2, 3, true, false));
        assert!(!schema_reply_current(3, 3, false, false));
        assert!(!schema_reply_current(3, 3, true, true));
    }

    #[test]
    fn search_input_composition_blocks_common_close_replacement_and_exit_preflight() {
        assert!(search_leave_preflight(true).is_err());
        assert!(search_leave_preflight(false).is_ok());
    }

    #[test]
    fn setting_description_defaults_to_help_except_rich_text_output_scope() {
        assert!(!inline_settings_description("global", "UiThemeMode", false));
        assert!(!inline_settings_description(
            "global", "BackgroundColor", false,
        ));
        assert!(inline_settings_description(
            "clip", "PlainTextPasteByDefault", false,
        ));
        assert!(!inline_settings_description(
            "other", "PlainTextPasteByDefault", false,
        ));
    }

    #[test]
    fn list_settings_keeps_descriptions_in_label_hover_help() {
        for (config_id, key) in [
            ("clip", "PlainTextPasteByDefault"),
            ("global", "UiThemeMode"),
        ] {
            assert!(!inline_settings_description(config_id, key, true));
        }
    }

    #[test]
    fn folder_picker_initial_ime_never_reserves_or_starts_provider() {
        let mut requests = FolderPickerRequests::default();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        if let Some(lease) = requests.accept("Folder", true) {
            run_folder_picker(&lease, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"path":"provisional IME must not become initialPath"}))
            });
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(requests.pending.is_empty());
        assert!(requests.accept("Folder", false).is_some());
    }

    #[test]
    fn folder_picker_reserves_key_before_provider_creation_and_rejects_duplicate_browse() {
        let mut requests = FolderPickerRequests::default();
        let lease = requests.accept("DataDirectory", false).unwrap();
        let provider_lease = lease.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            gate.recv().unwrap(); // The first provider has not created its modal yet.
            run_folder_picker(&provider_lease, || {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"path":"C:/selected 😀","cancelled":false}))
            })
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(requests.is_pending("DataDirectory"));
        assert!(!requests.browse_enabled("DataDirectory", false));
        assert!(requests.accept("DataDirectory", false).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let selected = worker.join().unwrap().unwrap().unwrap();
        assert_eq!(selected["path"], "C:/selected 😀");
        assert_eq!(requests.finish("DataDirectory", &lease), Some(true));
        assert!(requests.browse_enabled("DataDirectory", false));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn folder_picker_cancel_and_failure_release_only_their_slot_for_retry() {
        let mut requests = FolderPickerRequests::default();
        for outcome in [
            Ok(json!({"path":null,"cancelled":true})),
            Err("picker failed".into()),
        ] {
            let lease = requests.accept("ImageFolder", false).unwrap();
            assert!(!requests.browse_enabled("ImageFolder", false));
            assert_eq!(run_folder_picker(&lease, || outcome.clone()), Some(outcome));
            assert_eq!(requests.finish("ImageFolder", &lease), Some(true));
            assert!(requests.browse_enabled("ImageFolder", false));
            let retry = requests.accept("ImageFolder", false).unwrap();
            assert_eq!(requests.finish("ImageFolder", &lease), None);
            assert!(requests.accept("ImageFolder", false).is_none());
            assert_eq!(requests.finish("ImageFolder", &retry), Some(true));
        }
    }

    #[test]
    fn folder_picker_leave_replacement_and_dispose_retire_before_provider_starts() {
        for _lifecycle in ["leave", "replacement"] {
            let mut requests = FolderPickerRequests::default();
            let lease = requests.accept("Folder", false).unwrap();
            requests.retire_all(); // Same invalidation used by set_leave_locked/load.
            assert!(
                run_folder_picker(&lease, || panic!("retired provider must not start")).is_none()
            );
            assert_eq!(requests.finish("Folder", &lease), Some(false));
            assert!(requests.accept("Folder", false).is_some());
        }
        let mut requests = FolderPickerRequests::default();
        let lease = requests.accept("Folder", false).unwrap();
        drop(requests);
        assert!(run_folder_picker(&lease, || panic!("disposed provider must not start")).is_none());
    }

    #[test]
    fn folder_picker_late_selected_reply_is_retired_and_does_not_replace_changed_field() {
        let mut requests = FolderPickerRequests::default();
        let lease = requests.accept("Folder", false).unwrap();
        let provider_lease = lease.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            run_folder_picker(&provider_lease, || {
                started.send(()).unwrap();
                gate.recv().unwrap();
                Ok(json!({"path":"C:/old-selected"}))
            })
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        // This includes edit-then-revert: retirement is monotonic, not a final
        // string-equality check that can accidentally revive the old reply.
        requests.retire_key("Folder");
        release.send(()).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap().unwrap()["path"],
            "C:/old-selected"
        );
        assert_eq!(requests.finish("Folder", &lease), Some(false));
    }

    #[test]
    fn folder_picker_different_field_remains_current_when_one_field_is_edited() {
        let mut requests = FolderPickerRequests::default();
        let one = requests.accept("FirstFolder", false).unwrap();
        let two = requests.accept("OtherFolder", false).unwrap();
        requests.retire_key("FirstFolder");
        assert_eq!(requests.finish("FirstFolder", &one), Some(false));
        assert_eq!(requests.finish("OtherFolder", &two), Some(true));
    }

    #[test]
    fn two_embedded_forms_wait_for_both_and_keep_a_failed_form_error() {
        let queues: Vec<_> = (0..2)
            .map(|_| {
                Arc::new(Mutex::new(SaveQueue {
                    running: true,
                    ..Default::default()
                }))
            })
            .collect();
        let mut first = queue_barrier(&queues[0]);
        let mut second = queue_barrier(&queues[1]);
        let (waiters, result) = finish_queue(&mut queues[0].lock().unwrap());
        for waiter in waiters {
            waiter.send(result.clone()).unwrap();
        }
        assert_eq!(first.try_recv().unwrap(), Ok(()));
        assert!(matches!(
            second.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let (waiters, result) = {
            let mut queue = queues[1].lock().unwrap();
            queue.errors.insert(
                ("global".into(), "FontFamilyString".into()),
                "disk rejected global form".into(),
            );
            finish_queue(&mut queue)
        };
        for waiter in waiters {
            waiter.send(result.clone()).unwrap();
        }
        assert_eq!(
            second.try_recv().unwrap(),
            Err("disk rejected global form".into())
        );
    }

    #[test]
    fn stale_leave_completion_cannot_unlock_a_new_leave_or_reloaded_form() {
        assert!(same_leave(4, 7, 4, 7, true));
        assert!(!same_leave(4, 7, 4, 8, true));
        assert!(!same_leave(4, 7, 5, 7, true));
        assert!(!same_leave(4, 7, 4, 7, false));
    }

    #[test]
    fn accepted_write_barrier_waits_and_reports_failed_persistence() {
        let queue = Arc::new(Mutex::new(SaveQueue {
            running: true,
            ..Default::default()
        }));
        let mut barrier = queue_barrier(&queue);
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let (waiters, result) = {
            let mut state = queue.lock().unwrap();
            state.errors.insert(
                ("global".into(), "FontFamilyString".into()),
                "disk write rejected".into(),
            );
            finish_queue(&mut state)
        };
        for sender in waiters {
            sender.send(result.clone()).unwrap();
        }
        assert_eq!(
            barrier.try_recv().unwrap(),
            Err("disk write rejected".into())
        );
        queue.lock().unwrap().errors.clear();
        assert_eq!(queue_barrier(&queue).try_recv().unwrap(), Ok(()));
    }

    #[test]
    fn schema_error_blocks_leave_but_warning_is_kept_as_feedback() {
        assert_eq!(
            form_save_error(&json!({"messages":[{"level":"warning","text":"Shortcut warning"}]})),
            None
        );
        assert_eq!(
            form_save_error(&json!({"messages":[{"level":"error","text":"Cannot persist"}]})),
            Some("Cannot persist".into())
        );
    }

    #[test]
    fn saved_page_snapshot_does_not_overwrite_newer_field_edits() {
        let baseline = vec![
            json!({"key":"A","value":1}),
            json!({"key":"B","value":"old"}),
        ];
        let current = vec![
            json!({"key":"A","value":2}),
            json!({"key":"B","value":"new"}),
        ];
        let mut reply = json!({"fields":[{"key":"A","value":1},{"key":"B","value":"old"}]});
        merge_save_values(&mut reply, &current, &baseline);
        assert_eq!(reply["fields"][0]["value"], 2);
        assert_eq!(reply["fields"][1]["value"], "new");
        let mut normalized = json!({"fields":[{"key":"A","value":9},{"key":"B","value":"OLD"}]});
        merge_save_values(&mut normalized, &baseline, &baseline);
        assert_eq!(normalized["fields"][0]["value"], 9);
        assert_eq!(normalized["fields"][1]["value"], "OLD");
    }

    #[test]
    fn empty_get_messages_keep_transient_update_outcomes() {
        let mut message = Some(FormMessage::Schema(
            true,
            "Shortcut registration failed".into(),
        ));
        refresh_schema_message(&mut message, &json!({ "messages": [] }));
        assert!(
            matches!(&message, Some(FormMessage::Schema(true, text)) if text == "Shortcut registration failed")
        );
        refresh_schema_message(&mut message, &json!({}));
        assert!(
            matches!(&message, Some(FormMessage::Schema(true, text)) if text == "Shortcut registration failed")
        );
        refresh_schema_message(
            &mut message,
            &json!({ "messages": [{ "level": "info", "text": "Translated result" }] }),
        );
        assert!(
            matches!(&message, Some(FormMessage::Schema(false, text)) if text == "Translated result")
        );
        let mut local = Some(FormMessage::TagsSaved);
        refresh_schema_message(
            &mut local,
            &json!({ "messages": [{ "level": "warning", "text": "Other result" }] }),
        );
        assert!(matches!(local, Some(FormMessage::TagsSaved)));
    }

    #[test]
    fn language_refresh_keeps_optimistic_values_and_form_shape() {
        let mut fields = vec![
            json!({ "key": "UiLanguage", "label": "界面语言", "group": "外观", "description": "说明",
                "value": "en", "selectionItems": [{"key": "en", "name": "English"}], "inputType": "enum" }),
            json!({ "key": "FontSize", "label": "字号", "value": 21, "min": 12 }),
        ];
        let translated = vec![
            json!({ "key": "UiLanguage", "label": "Language", "group": "Appearance", "description": "Description",
                "value": "system", "selectionItems": [{"key": "en", "name": "English"}], "inputType": "text" }),
            json!({ "key": "FontSize", "label": "Font size", "value": 16, "min": 0 }),
            json!({ "key": "Unrelated", "label": "Other", "value": true }),
        ];
        merge_localized_metadata(&mut fields, &translated);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["value"], "en");
        assert_eq!(fields[0]["inputType"], "enum");
        assert_eq!(fields[0]["label"], "Language");
        assert_eq!(fields[0]["group"], "Appearance");
        assert_eq!(fields[0]["description"], "Description");
        assert_eq!(fields[1]["value"], 21);
        assert_eq!(fields[1]["min"], 12);
    }

    #[test]
    fn hotkeys_use_shared_wpf_key_names() {
        let ctrl = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(hotkey_chord("`", &ctrl).as_deref(), Some("Ctrl+Oem3"));
        let all = gpui::Modifiers {
            control: true,
            alt: true,
            shift: true,
            platform: true,
            ..Default::default()
        };
        assert_eq!(
            hotkey_chord("v", &all).as_deref(),
            Some("Ctrl+Alt+Shift+Win+V")
        );
        assert_eq!(hotkey_chord("1", &ctrl).as_deref(), Some("Ctrl+D1"));
        assert_eq!(
            hotkey_chord("f12", &Default::default()).as_deref(),
            Some("F12")
        );
        assert_eq!(hotkey_chord("unknown", &ctrl), None);
        assert_eq!(hotkey_display("Ctrl+Shift+Oem3"), "Ctrl + Shift + `");
        assert_eq!(hotkey_display("Alt+D1"), "Alt + 1");
    }

    #[test]
    fn routes_map_to_canonical_settings_pages() {
        assert_eq!(
            settings_page_from_route("/main/settings").as_deref(),
            Some("global")
        );
        assert_eq!(
            settings_page_from_route("/main/settings/clip?x=1").as_deref(),
            Some("clip")
        );
        assert_eq!(
            settings_page_from_route("/main/settings/hotkeys").as_deref(),
            Some("trigger")
        );
        for (route, page) in [
            ("/main/settings/menu_global", "menu_global"),
            ("/main/settings/menu_file", "menu_file"),
            ("/main/settings/textAction", "textAction"),
            ("/main/settings/deviceSync", DEVICES_PAGE),
            ("/settings/DEVICESYNC?from=tray", DEVICES_PAGE),
            ("/main/settings/user", "user"),
            ("/main/settings/menu", "menu_global"),
            ("/main/settings/menu_window", "menu_global"),
            ("/main/settings/menu_favor", "menu_global"),
        ] {
            assert_eq!(settings_page_from_route(route).as_deref(), Some(page), "{route}");
        }
        // The former management help route opens the unified main window.
        assert_eq!(settings_page_from_route("/main/docs").as_deref(), Some(HELP_PAGE));
    }

    #[test]
    fn numbers_clamp_and_keep_integer_storage() {
        let field = json!({ "value": 25, "min": 3, "max": 50 });
        assert_eq!(number_json(&field, 99.4), json!(50));
        assert_eq!(number_json(&field, 1.0), json!(3));
        let float = json!({ "value": 12.0, "min": 12.0, "max": 30.0 });
        assert_eq!(number_json(&float, 13.5), json!(13.5));
        assert_eq!(number_step(&json!({ "value": 160 })), 10.0);
        assert_eq!(
            number_step(&json!({ "value": 25, "min": 3, "max": 50 })),
            1.0
        );
    }

    #[test]
    fn multiline_text_is_not_offered_as_a_single_line_input() {
        let field = json!({ "inputType": "text", "formEditor": "default", "value": "a\nb" });
        assert_eq!(kind(&field), Kind::External);
        let paths = json!({ "inputType": "text", "formEditor": "fileMenuPaths", "value": "a\nb" });
        assert_eq!(kind(&paths), Kind::Lines);
    }
}
