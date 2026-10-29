//! Native Super Hub and cascading menu presentation. The existing menu registry
//! owns paths, conditions, execution targets, Check/Value state and refresh rules.
//! Every asynchronous reply belongs to one visible core session and UI request.

use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use cliphost_core::{
    host::{NativeSuperMenuKey, NativeSuperMenuKeyScope, NativeSuperMenuPasteRequest},
    i18n::tr,
    HostState,
};
use gpui::{
    canvas, div, img, prelude::*, px, relative, AnimationExt, AnyElement, App,
    Bounds, Context, Entity, Subscription,
    FocusHandle, Hsla, Image, ImageFormat, MouseButton, Pixels, ScrollHandle, SharedString, Task,
    Window,
    WindowHandle, WindowOptions,
};
use crate::crisp_svg::svg;
use crate::smooth_wheel::SmoothWheelExt;
use serde_json::{json, Value};

use crate::{motion, platform, preview::DesignTokens};
use crate::text_action_launcher::{TextActionLauncher, TextActionLauncherEvent};

#[path = "super_menu_file_preview.rs"]
mod file_preview;

#[path = "super_menu_browser.rs"]
pub mod browser_workspace;
use browser_workspace::BrowserPanel;

#[cfg(all(windows, debug_assertions))]
#[path = "super_menu_file_preview_fixture.rs"]
pub mod file_preview_fixture;

#[cfg(all(windows, debug_assertions))]
#[path = "super_menu_collision_fixture.rs"]
pub mod collision_fixture;

#[cfg(all(windows, debug_assertions))]
#[path = "super_menu_style_fixture.rs"]
pub mod style_fixture;

const EDGE: f32 = 12.0;
const GAP: f32 = 6.0;
const MAX_COLUMNS: usize = 16;
/// One history batch, as in the main list; scrolling near the end loads the next.
const HUB_TAKE: usize = 100;
/// Rows left below the viewport before the next batch is requested (main list rule).
const HUB_PREFETCH_ROWS: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionKey {
    generation: i32,
    id: String,
}

impl SessionKey {
    fn from_state(state: &Value) -> Option<Self> {
        if state["visible"].as_bool() != Some(true) {
            return None;
        }
        let id = state["sessionId"].as_str()?.to_string();
        if id.is_empty() {
            return None;
        }
        Some(Self {
            generation: state["generation"]
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or(0),
            id,
        })
    }

    fn is_current(&self, host: &HostState) -> bool {
        Self::from_state(&host.super_menu_state_detail()).as_ref() == Some(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Window,
    File,
    Favor,
    Bookmarks,
    Text,
    Clipboard,
}

impl Category {
    const ALL: [Self; 6] = [
        Self::Window,
        Self::File,
        Self::Favor,
        Self::Bookmarks,
        Self::Text,
        Self::Clipboard,
    ];

    fn adjacent(self, direction: i32) -> Self {
        let index = Self::ALL.iter().position(|category| *category == self).unwrap() as i32;
        Self::ALL[(index + direction).rem_euclid(Self::ALL.len() as i32) as usize]
    }

    fn from_action(value: &Value) -> Self {
        let numeric = value.as_u64().or_else(|| value.as_str()?.parse().ok());
        if let Some(code) = numeric {
            return match code {
                0 => Self::Window,
                1 => Self::Text,
                2 => Self::Bookmarks,
                3 | 6 => Self::File,
                _ => Self::Favor,
            };
        }
        match value
            .as_str()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "window" => Self::Window,
            "file" | "reopen" => Self::File,
            "bookmarks" => Self::Bookmarks,
            "text" => Self::Text,
            "clipboard" => Self::Clipboard,
            _ => Self::Favor,
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::File => "file",
            Self::Favor => "favor",
            Self::Bookmarks => "bookmarks",
            Self::Text => "text",
            Self::Clipboard => "clipboard",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Window => tr("窗口", "Windows"),
            Self::File => tr("文件", "Files"),
            Self::Favor => tr("收藏", "Favorites"),
            Self::Bookmarks => tr("书签", "Bookmarks"),
            Self::Text => tr("文本", "Text"),
            Self::Clipboard => tr("剪贴板", "Clipboard"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::File => "folder-open",
            Self::Favor => "star",
            Self::Bookmarks => "link",
            Self::Text => "text",
            Self::Clipboard => "clipboard",
        }
    }
}

#[derive(Clone, Debug)]
struct Column {
    parent: String,
    collection: &'static str,
    anchor_y: f32,
    items: Vec<Value>,
    status: String,
    message: String,
    cursor: Option<String>,
    loading: bool,
    nonce: u64,
    /// Identity of this column's appearance: fixed when it is created and
    /// kept across refreshes, so only a newly appearing column plays its entrance.
    entry: u64,
}

static NEXT_COLUMN_ENTRY: AtomicU64 = AtomicU64::new(1);

impl Column {
    fn from_page(parent: String, collection: &'static str, anchor_y: f32, page: &Value) -> Self {
        Self {
            parent,
            collection,
            anchor_y,
            items: strip_setting_rows(array(page, "items")),
            status: page["status"].as_str().unwrap_or("Ready").to_string(),
            message: page["message"].as_str().unwrap_or("").to_string(),
            cursor: if page["hasMore"].as_bool() == Some(true) {
                page["nextCursor"].as_str().map(str::to_string)
            } else {
                None
            },
            loading: false,
            nonce: 0,
            entry: NEXT_COLUMN_ENTRY.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// A refreshed or extended column replaces its predecessor without
    /// appearing again.
    fn refresh_of(mut self, previous: &Column) -> Self {
        self.entry = previous.entry;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn clamp_rect(mut rect: Rect, size: (f32, f32)) -> Rect {
    rect.width = rect.width.min((size.0 - EDGE * 2.0).max(1.0));
    rect.height = rect.height.min((size.1 - EDGE * 2.0).max(1.0));
    let max_x = (size.0 - EDGE - rect.width).max(0.0);
    let max_y = (size.1 - EDGE - rect.height).max(0.0);
    rect.x = rect.x.clamp(EDGE.min(max_x), max_x);
    rect.y = rect.y.clamp(EDGE.min(max_y), max_y);
    rect
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CascadeSide {
    Right,
    Left,
}

/// Continue in the last opening direction until the monitor edge forces a turn.
/// Re-preferring right on every level makes a leftward submenu fold back over
/// its ancestors even when there is still space on the left.
fn column_rect(
    previous: Option<Rect>,
    preferred: CascadeSide,
    anchor: (f32, f32),
    row_y: f32,
    width: f32,
    height: f32,
    size: (f32, f32),
) -> (Rect, CascadeSide) {
    let width = width.min((size.0 - EDGE * 2.0).max(1.0));
    let (x, side) = previous.map_or((anchor.0, preferred), |prev| {
        let candidate = |side| match side {
            CascadeSide::Right => prev.x + prev.width + GAP,
            CascadeSide::Left => prev.x - GAP - width,
        };
        let fits = |x: f32| x >= EDGE && x + width <= size.0 - EDGE;
        let opposite = match preferred {
            CascadeSide::Right => CascadeSide::Left,
            CascadeSide::Left => CascadeSide::Right,
        };
        let side = if fits(candidate(preferred)) {
            preferred
        } else if fits(candidate(opposite)) {
            opposite
        } else {
            // In a narrow viewport overlap is unavoidable. Use the side with
            // more free space, then clamp; the front column occludes its parent.
            let left_space = prev.x - GAP - EDGE;
            let right_space = size.0 - EDGE - prev.x - prev.width - GAP;
            if left_space > right_space {
                CascadeSide::Left
            } else if right_space > left_space {
                CascadeSide::Right
            } else {
                preferred
            }
        };
        (candidate(side), side)
    });
    (clamp_rect(
        Rect {
            x,
            y: if previous.is_some() { row_y } else { anchor.1 },
            width,
            height,
        },
        size,
    ), side)
}

// 2026-10-28: the overlay covers the monitor but the menu opens at the
// pointer, so the category strip rides directly above the menu instead of the
// screen top, and the settings gear is its last cell (ui-design `.wt-strip`,
// `.sms-gear--strip`). All six categories share one strip.
const STRIP_HEIGHT: f32 = 40.0;
const STRIP_PADDING: f32 = 4.0;
const STRIP_TAB_HEIGHT: f32 = 30.0;
const STRIP_TAB_GAP: f32 = 2.0;
const STRIP_TAB_PADDING: f32 = 10.0;
const STRIP_ICON_ONLY_TAB_PADDING: f32 = 8.0;
const STRIP_ICON: f32 = 15.0;
const STRIP_LABEL_GAP: f32 = 6.0;
const STRIP_LABEL_SIZE: f32 = 12.5;
/// The 1px divider before the gear plus 4px on either side.
const STRIP_DIVIDER: f32 = 9.0;
const STRIP_GEAR: f32 = 30.0;
/// Below this overlay width the strip shows icons only.
const STRIP_ICON_ONLY_BELOW: f32 = 560.0;
/// Strip bottom to every category's first row (the file hint sits beside the strip).
const STRIP_CONTENT_GAP: f32 = 14.0;
/// Room kept below the strip, so a pointer near the bottom still gets a menu.
const STRIP_MIN_CONTENT: f32 = 360.0;
const SETTINGS_PANEL_WIDTH: f32 = 340.0;
const SETTINGS_PANEL_GAP: f32 = 8.0;
/// A flyout beside the strip may rise to keep about this much height.
const SETTINGS_PANEL_PREFERRED_HEIGHT: f32 = 500.0;

/// GPUI measures ellipsized text before flex widths resolve, so the strip
/// gives each label an explicit width: CJK at 1em, Latin at ~0.62em.
fn strip_label_width(label: &str) -> f32 {
    label
        .chars()
        .map(|ch| if ch as u32 >= 0x2E80 { STRIP_LABEL_SIZE } else { STRIP_LABEL_SIZE * 0.62 })
        .sum::<f32>()
        .ceil()
        + 2.0
}

fn strip_tab_width(category: Category, icon_only: bool) -> f32 {
    if icon_only {
        STRIP_ICON_ONLY_TAB_PADDING * 2.0 + STRIP_ICON
    } else {
        STRIP_TAB_PADDING * 2.0 + STRIP_ICON + STRIP_LABEL_GAP + strip_label_width(category.label())
    }
}

fn strip_width(icon_only: bool) -> f32 {
    let tabs: f32 = Category::ALL.iter().map(|category| strip_tab_width(*category, icon_only)).sum();
    // Padding and the 1px border on both sides.
    (STRIP_PADDING + 1.0) * 2.0
        + tabs
        + STRIP_TAB_GAP * (Category::ALL.len() - 1) as f32
        + STRIP_DIVIDER
        + STRIP_GEAR
}

/// The strip sits just above the pointer where the menu first opened.
/// 2026-10-28: it depends on nothing that changes inside one opening (the
/// host keeps that anchor across category switches, docked file panels
/// shrink to fit), so clicking it never moves it; menus place around it.
fn category_strip_rect(anchor: (f32, f32), size: (f32, f32)) -> Rect {
    let width = strip_width(size.0 < STRIP_ICON_ONLY_BELOW).min((size.0 - EDGE * 2.0).max(1.0));
    let max_y = size.1 - EDGE - STRIP_MIN_CONTENT - STRIP_CONTENT_GAP - STRIP_HEIGHT;
    Rect {
        x: anchor.0.clamp(EDGE, (size.0 - width - EDGE).max(EDGE)),
        y: (anchor.1 - STRIP_CONTENT_GAP - STRIP_HEIGHT).clamp(EDGE, max_y.max(EDGE)),
        width,
        height: STRIP_HEIGHT,
    }
}

/// The gear is the strip's last cell.
fn strip_gear_rect(strip: Rect) -> Rect {
    Rect {
        x: strip.x + strip.width - 1.0 - STRIP_PADDING - STRIP_GEAR,
        y: strip.y + (strip.height - STRIP_GEAR) * 0.5,
        width: STRIP_GEAR,
        height: STRIP_GEAR,
    }
}

/// Settings fly out beside the gear, never over the menu below the strip;
/// without room on either side they drop below the strip at the screen edge.
/// Beside the strip they may rise above it when the pointer is low.
fn settings_panel_origin(strip: Rect, size: (f32, f32), width: f32) -> (f32, f32) {
    let right = strip.x + strip.width + SETTINGS_PANEL_GAP;
    let beside_y = strip.y.min(size.1 - EDGE - SETTINGS_PANEL_PREFERRED_HEIGHT).max(EDGE);
    if right + width <= size.0 - EDGE {
        (right, beside_y)
    } else if strip.x - SETTINGS_PANEL_GAP - width >= EDGE {
        (strip.x - SETTINGS_PANEL_GAP - width, beside_y)
    } else {
        ((size.0 - width - EDGE).max(EDGE), strip.y + strip.height + SETTINGS_PANEL_GAP)
    }
}

#[derive(Clone, Copy)]
enum SettingControl {
    Switch,
    Stepper { min: f64, max: f64, step: f64 },
    /// (stored value, zh, en)
    Choice(&'static [(&'static str, &'static str, &'static str)]),
    Link(SettingLink),
}

#[derive(Clone, Copy, PartialEq)]
enum SettingLink {
    CustomDirs,
    TextActions,
}

/// One in-menu setting; `config`/`key` are the existing settings form identities.
#[derive(Clone, Copy)]
struct SettingSpec {
    config: &'static str,
    key: &'static str,
    label: (&'static str, &'static str),
    hint: Option<(&'static str, &'static str)>,
    control: SettingControl,
    /// Disabled while this boolean key of the same config is off.
    requires: Option<&'static str>,
}

const fn setting(config: &'static str, key: &'static str, label: (&'static str, &'static str), control: SettingControl) -> SettingSpec {
    SettingSpec { config, key, label, hint: None, control, requires: None }
}

const fn hinted(mut spec: SettingSpec, hint: (&'static str, &'static str)) -> SettingSpec {
    spec.hint = Some(hint);
    spec
}

const GENERAL_SETTINGS: &[SettingSpec] = &[
    setting("menu_global", "MenuFontSize", ("字体大小", "Font size"), SettingControl::Stepper { min: 12.0, max: 30.0, step: 1.0 }),
    setting("menu_global", "MenuMaxWidth", ("菜单宽度", "Menu width"), SettingControl::Stepper { min: 300.0, max: 500.0, step: 10.0 }),
    hinted(setting("menu_global", "MaxSingleLoadCount", ("子菜单每批条目", "Submenu batch size"), SettingControl::Stepper { min: 5.0, max: 50.0, step: 5.0 }),
        ("向下滚动时追加同样条数", "Scrolling down loads the same number again")),
    hinted(setting("menu_global", "QuickLoadSubmenu", ("快速加载子菜单", "Load submenus at once"), SettingControl::Switch),
        ("一次加载全部条目，条目多时可能卡顿", "Loads every item; large menus may be slow")),
    setting("menu_global", "ShowMenuLocation", ("菜单位置", "Menu position"), SettingControl::Choice(&[
        ("NA", "默认", "Default"), ("CenterScreen", "屏幕中央", "Center"), ("MouseAround", "鼠标周围", "Near pointer"), ("MouseRightBottom", "鼠标右下", "Below-right"),
    ])),
];

const WINDOW_SETTINGS: &[SettingSpec] = &[
    hinted(setting("menu_global", "Grouping", ("按进程分组", "Group by process"), SettingControl::Switch),
        ("同一应用的多个窗口合为一组", "Windows of one app share a row")),
    hinted(setting("menu_global", "ShowWindowThumbnail", ("悬停显示窗口缩略图", "Window thumbnail on hover"), SettingControl::Switch),
        ("在操作菜单顶部显示小图；Alt+Tab 已有大预览", "A small image atop the actions menu")),
];

const FILE_SETTINGS: &[SettingSpec] = &[
    hinted(setting("menu_file", "PreferInternalBrowser", ("在超级菜单中打开文件夹", "Open folders in Super menu"), SettingControl::Switch),
        ("单击普通目录在浏览栏打开；文件对话框仍优先定位", "Plain folder clicks open here; file dialogs keep their location workflow")),
    setting("menu_file", "ShowType", ("显示内容", "Show"), SettingControl::Choice(&[
        ("dir_file", "文件和文件夹", "Both"), ("dir", "只文件夹", "Folders"), ("file", "只文件", "Files"),
    ])),
    setting("menu_file", "FileSortType", ("排序", "Sort"), SettingControl::Choice(&[
        ("NA", "默认", "Default"), ("name", "名称", "Name"), ("createTime", "创建", "Created"), ("accessTime", "访问", "Accessed"), ("editTime", "修改", "Modified"),
    ])),
    setting("menu_file", "SplitDirAndFile", ("文件夹和文件分开显示", "Separate folders and files"), SettingControl::Switch),
    setting("menu_file", "ShowExtension", ("显示扩展名", "Show extensions"), SettingControl::Switch),
    setting("menu_file", "ShowHiddenFiles", ("显示隐藏文件", "Show hidden files"), SettingControl::Switch),
    hinted(setting("menu_file", "ExpandOpenedFolder", ("直接展开打开的文件夹", "Expand open folders"), SettingControl::Switch),
        ("关闭时收进「打开的文件夹」子菜单", "Otherwise grouped under Open folders")),
    hinted(setting("menu_file", "TopEdgeLine", ("贴顶晾衣绳", "Top-edge file line"), SettingControl::Switch),
        ("指针停在屏幕顶边时落下文件夹；全屏程序时不触发", "Rest at the top screen edge to drop folders; off in full screen")),
    setting("menu_file", "Files", ("自定义目录", "Custom folders"), SettingControl::Link(SettingLink::CustomDirs)),
];

const FAVOR_SETTINGS: &[SettingSpec] = &[
    setting("menu_global", "ShowAdderMenuItem", ("显示「添加选中」", "Show Add selection"), SettingControl::Switch),
    setting("menu_global", "ShowWindowMenuItem", ("显示窗口菜单", "Show window menu"), SettingControl::Switch),
    SettingSpec {
        hint: Some(("关闭时收进「窗口管理」子菜单", "Otherwise grouped under Window management")),
        requires: Some("ShowWindowMenuItem"),
        ..setting("menu_global", "ExpandWindowMenu", ("直接展开窗口列表", "Expand window list"), SettingControl::Switch)
    },
    setting("menu_global", "ShowBookMarksMenu", ("显示浏览器书签", "Show browser bookmarks"), SettingControl::Switch),
];

const BOOKMARK_SETTINGS: &[SettingSpec] = &[hinted(
    setting("menu_global", "ShowBookMarksMenu", ("在收藏中也显示书签", "Also show in Favorites"), SettingControl::Switch),
    ("收藏分类末尾附加浏览器书签", "Appends browser bookmarks to Favorites"),
)];

const TEXT_SETTINGS: &[SettingSpec] = &[hinted(
    setting("textAction", "", ("管理文本操作", "Manage text actions"), SettingControl::Link(SettingLink::TextActions)),
    ("规则、分组与常用", "Rules, groups and favorites"),
)];

fn category_settings(category: Category) -> &'static [SettingSpec] {
    match category {
        Category::Window => WINDOW_SETTINGS,
        Category::File => FILE_SETTINGS,
        Category::Favor => FAVOR_SETTINGS,
        Category::Bookmarks => BOOKMARK_SETTINGS,
        Category::Text => TEXT_SETTINGS,
        Category::Clipboard => &[],
    }
}

/// GPUI drops the WPF-era trailing "设置" rows and the separators they leave behind.
fn strip_setting_rows(items: Vec<Value>) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        if item["role"].as_str() == Some("setting") {
            continue;
        }
        let separator = item["kind"].as_str() == Some("Separator");
        if separator && kept.last().is_none_or(|last| last["kind"].as_str() == Some("Separator")) {
            continue;
        }
        kept.push(item);
    }
    while kept.last().is_some_and(|last| last["kind"].as_str() == Some("Separator")) {
        kept.pop();
    }
    kept
}

#[derive(Default)]
struct CategoryWheel {
    distance: f32,
    direction: f32,
    precise: Option<bool>,
    last_event: Option<Instant>,
    last_step: Option<Instant>,
}

impl CategoryWheel {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn retire(&mut self) {
        // Session changes discard the gesture, while the emission timestamp
        // keeps smooth-wheel inertia from immediately switching again.
        let last_step = self.last_step;
        *self = Self { last_step, ..Self::default() };
    }

    fn clear_pending(&mut self) {
        self.distance = 0.0;
        self.direction = 0.0;
    }

    fn step(&mut self, delta: gpui::ScrollDelta, now: Instant) -> Option<i32> {
        let precise = delta.precise();
        let (x, y, threshold, notch) = match delta {
            gpui::ScrollDelta::Lines(delta) => (delta.x, delta.y, 1.0, delta.y.abs() >= 1.0),
            gpui::ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y), 40.0, false),
        };
        if !x.is_finite() || !y.is_finite() || y.abs() <= x.abs() {
            self.clear_pending();
            return None;
        }
        if self.last_event.is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_millis(200))
            || self.direction != y.signum() || self.precise != Some(precise)
        {
            self.distance = 0.0;
        }
        self.direction = y.signum();
        self.precise = Some(precise);
        self.last_event = Some(now);
        if notch {
            self.distance = 0.0;
            self.last_step = Some(now);
            return Some(if y < 0.0 { 1 } else { -1 });
        }
        if self.last_step.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_millis(120)) {
            self.distance = 0.0;
            return None;
        }
        self.distance += y;
        if self.distance.abs() < threshold {
            return None;
        }
        self.distance = 0.0;
        self.last_step = Some(now);
        Some(if y < 0.0 { 1 } else { -1 })
    }
}

fn array(value: &Value, key: &str) -> Vec<Value> {
    value[key].as_array().cloned().unwrap_or_default()
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").to_string()
}

pub(crate) fn enabled(node: &Value) -> bool {
    node["kind"].as_str() != Some("Separator") && node["isEnabled"].as_bool() != Some(false)
}

pub(crate) fn expandable(node: &Value) -> bool {
    node["kind"].as_str() != Some("Value")
        && (node["hasChildren"].as_bool() == Some(true) || node["kind"].as_str() == Some("Folder"))
}

fn node_collection(context: bool) -> &'static str {
    if context {
        "context-menu"
    } else {
        "children"
    }
}

fn default_native_key_scope(category: Category, has_clips: bool) -> NativeSuperMenuKeyScope {
    if category == Category::Clipboard && has_clips {
        NativeSuperMenuKeyScope::ClipboardHub
    } else {
        NativeSuperMenuKeyScope::Dismiss
    }
}

fn load_menu_column(
    host: &HostState,
    key: &SessionKey,
    parent: &str,
    collection: &str,
    cursor: Option<&str>,
) -> Result<Value, String> {
    if !key.is_current(host) {
        return Err("Session not found".into());
    }
    host.menu_nodes_ex(&key.id, parent, collection, cursor)
}

pub(crate) fn primary_invokes(node: &Value) -> bool {
    let mode = node["invokeMode"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Explicit expansion containers can carry a path for their children
    // (for example "Copy more"); that path is not their primary command.
    if mode == "expand" {
        return false;
    }
    if mode == "invoke"
        || mode == "invokeandexpand"
        || matches!(node["invokeMode"].as_u64(), Some(1 | 2))
    {
        return true;
    }
    if !expandable(node) {
        return true;
    }
    // None/0 containers (recent files, opened-folder roots) carry paths for
    // display or their descendants. Only old nodes with no mode use a fallback.
    if !node["invokeMode"].is_null() {
        return false;
    }
    let path = node["payload"]["path"]
        .as_str()
        .or_else(|| node["description"].as_str())
        .unwrap_or("")
        .trim();
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (path.as_bytes().get(1) == Some(&b':')
            && matches!(path.as_bytes().get(2), Some(b'\\' | b'/')))
}

fn icon_key(node: &Value) -> &'static str {
    let role = node["role"].as_str().unwrap_or("");
    if matches!(role, "window" | "window-group") {
        return "window";
    }
    if node["kind"].as_str() == Some("Folder") {
        return "folder-open";
    }
    match role {
        "window" | "window-group" => "window",
        "setting" => "settings",
        "text" | "text-action" => "text",
        "bookmark" | "bookmarks" | "favor-link" => "link",
        "file" | "opened-folder" => "file",
        _ => "clipboard",
    }
}

/// Node icon spec worth resolving; `text:` and empty specs mean the kind glyph.
fn node_icon_spec(node: &Value) -> Option<&str> {
    let spec = node["icon"].as_str()?.trim();
    (!spec.is_empty() && !spec.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("text:")))
        .then_some(spec)
}

const NODE_ICON_CACHE_LIMIT: usize = 512;
const COLUMN_PADDING: f32 = 5.0;
/// Window submenu header (icon, two-line title, app · exe) and its optional thumbnail.
const WINDOW_HEADER_HEIGHT: f32 = 58.0;
const WINDOW_THUMB_HEIGHT: f32 = 110.0;
const WINDOW_HINT_HEIGHT: f32 = 26.0;

/// `visual.window` of a window row (2026-10-23 window menu design).
fn window_visual(node: &Value) -> Option<&Value> {
    node["visual"]["window"].as_object().map(|_| &node["visual"]["window"])
}

/// Window flag rows keep their icon and draw the check state on the right.
fn window_check(node: &Value) -> bool {
    node["kind"].as_str() == Some("Check")
        && node["role"].as_str().is_some_and(|role| role.starts_with("window-"))
        && node_icon_spec(node).is_some()
}

fn window_handle(node: &Value) -> i64 {
    node["payload"]["windowHandle"].as_i64().unwrap_or(0)
}

/// Destructive window commands: drawn in the error color and confirmed first.
fn window_kill_role(node: &Value) -> bool {
    matches!(node["role"].as_str(), Some("window-kill" | "window-kill-all"))
}

/// Window state shown as row chips instead of the C# 【前台】【置顶】 title prefixes.
fn window_chips(visual: &Value) -> Vec<(String, bool)> {
    let mut chips = Vec::new();
    if visual["foreground"].as_bool() == Some(true) {
        chips.push((tr("前台", "Active").to_string(), true));
    }
    if visual["minimized"].as_bool() == Some(true) {
        chips.push((tr("最小化", "Minimized").to_string(), false));
    }
    if let Some(opacity) = visual["opacity"].as_i64().filter(|opacity| *opacity < 100) {
        chips.push((format!("{opacity}%"), false));
    }
    if visual["penetrate"].as_bool() == Some(true) {
        chips.push((tr("穿透", "Click-through").to_string(), false));
    }
    chips
}

enum WindowThumb {
    Loading,
    Ready(Arc<Image>),
    Missing,
}

struct KillConfirm {
    node: Value,
    title: String,
    body: String,
    confirm: String,
}

fn kill_confirm_for(node: &Value, parent: Option<&Value>) -> KillConfirm {
    let title = text(node, "title");
    if node["role"].as_str() == Some("window-kill") {
        let visual = parent.and_then(window_visual);
        let exe = visual.and_then(|v| v["exe"].as_str()).unwrap_or("");
        let pid = visual.and_then(|v| v["pid"].as_u64()).unwrap_or(0);
        let name = match (exe.is_empty(), pid) {
            (false, 0) => exe.to_string(),
            (false, pid) => format!("{exe}（PID {pid}）"),
            _ => tr("该进程", "this process").to_string(),
        };
        return KillConfirm {
            node: node.clone(),
            title: format!("{}{name}？", tr("结束 ", "End ")),
            body: tr("该进程的所有窗口会一起关闭，未保存的内容会丢失。", "Every window of this process closes; unsaved work is lost.").to_string(),
            confirm: tr("结束进程", "End process").to_string(),
        };
    }
    if title.starts_with("重启") {
        return KillConfirm {
            node: node.clone(),
            title: format!("{title}？"),
            body: tr("任务栏与所有资源管理器窗口会短暂消失后重新打开。", "The taskbar and every Explorer window disappear briefly, then reopen.").to_string(),
            confirm: tr("重启", "Restart").to_string(),
        };
    }
    KillConfirm {
        node: node.clone(),
        title: format!("{title}？"),
        body: tr("同名进程的所有窗口会一起关闭，未保存的内容会丢失。", "Every window of these processes closes; unsaved work is lost.").to_string(),
        confirm: tr("结束进程", "End process").to_string(),
    }
}

fn node_label(node: &Value) -> String {
    if node["kind"].as_str() == Some("Check") && node["check"]["isChecked"].as_bool() == Some(true)
    {
        match node["role"].as_str() {
            // Icon rows show the state in their check box instead.
            Some("window-topmost") if !window_check(node) => return tr("取消置顶", "Unpin").to_string(),
            Some("window-min" | "window-max") => return tr("还原", "Restore").to_string(),
            _ => {}
        }
    }
    text(node, "title")
}

fn value_range(node: &Value) -> (f64, f64, f64) {
    let finite = |key, default| {
        node["value"][key]
            .as_f64()
            .filter(|v| v.is_finite())
            .unwrap_or(default)
    };
    let min = finite("min", 0.0);
    let max = finite("max", 100.0).max(min);
    let step = finite("step", 1.0);
    (min, max, if step > 0.0 { step } else { 1.0 })
}

fn normalize_value(node: &Value, value: f64) -> f64 {
    let (min, max, step) = value_range(node);
    if !value.is_finite() {
        return min;
    }
    let steps = ((value.clamp(min, max) - min) / step).round();
    let snapped = min + steps * step;
    if snapped.is_finite() {
        snapped.clamp(min, max)
    } else {
        value.clamp(min, max)
    }
}

fn slider_value(node: &Value, fraction: f64) -> f64 {
    let (min, max, _) = value_range(node);
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    normalize_value(node, min + (max - min) * fraction)
}

fn value_percent(node: &Value, current: f64) -> String {
    let (min, max, _) = value_range(node);
    let current = normalize_value(node, current);
    if max >= 200.0 {
        return format!("{}%", (current / max * 100.0).round());
    }
    if max <= min {
        return format!("{}", current.round());
    }
    format!("{}%", ((current - min) / (max - min) * 100.0).round())
}

fn key_value(node: &Value, current: f64, key: &str) -> Option<f64> {
    let (min, max, step) = value_range(node);
    let next = match key {
        "left" | "ArrowLeft" => current - step,
        "right" | "ArrowRight" => current + step,
        "home" | "Home" => min,
        "end" | "End" => max,
        _ => return None,
    };
    Some(normalize_value(node, next))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ValueSubmission {
    revision: u64,
    value: f64,
}

/// Display intent is independent of the server node and ordinary menu busy
/// state. A pending reply can acknowledge a value without replacing a newer
/// drag/key draft. Only the latest queued commit follows an in-flight request.
#[derive(Clone, Debug)]
struct ValueDraft {
    node: Value,
    local: f64,
    confirmed: f64,
    revision: u64,
    editing: bool,
    in_flight: Option<ValueSubmission>,
    queued: Option<ValueSubmission>,
}

impl ValueDraft {
    fn new(node: Value, revision: u64) -> Self {
        let current = normalize_value(&node, node["value"]["current"].as_f64().unwrap_or(0.0));
        Self {
            node,
            local: current,
            confirmed: current,
            revision,
            editing: false,
            in_flight: None,
            queued: None,
        }
    }

    fn edit(&mut self, value: f64, revision: u64) -> bool {
        let value = normalize_value(&self.node, value);
        self.editing = true;
        if self.local == value {
            return false;
        }
        self.local = value;
        self.revision = revision;
        true
    }

    fn queue_commit(&mut self) {
        self.editing = false;
        let submission = ValueSubmission {
            revision: self.revision,
            value: self.local,
        };
        if self.in_flight.is_some() {
            // Returning to the in-flight value replaces any intervening intent
            // too. Clearing that queued intent prevents an obsolete value send.
            self.queued = self
                .in_flight
                .filter(|pending| pending.value != submission.value)
                .map(|_| submission);
        } else if self.confirmed != self.local {
            self.queued = Some(submission);
        } else {
            self.queued = None;
        }
    }

    fn begin_submit(&mut self) -> Option<ValueSubmission> {
        if self.in_flight.is_some() {
            return None;
        }
        let submission = self.queued.take()?;
        self.in_flight = Some(submission);
        Some(submission)
    }

    fn accept_reply(&mut self, submission: ValueSubmission, patched: Option<&Value>) -> bool {
        if self.in_flight != Some(submission) {
            return false;
        }
        self.in_flight = None;
        if let Some(patched) = patched {
            self.node = patched.clone();
            self.confirmed = normalize_value(
                patched,
                patched["value"]["current"]
                    .as_f64()
                    .unwrap_or(submission.value),
            );
        } else {
            self.confirmed = submission.value;
        }
        if !self.editing && self.revision == submission.revision && self.queued.is_none() {
            self.local = self.confirmed;
        }
        true
    }

    fn sync_node(&mut self, node: &Value) {
        self.node = node.clone();
        if !self.editing
            && self.in_flight.is_none()
            && self.queued.is_none()
            && self.local == self.confirmed
        {
            self.confirmed = normalize_value(
                node,
                node["value"]["current"].as_f64().unwrap_or(self.confirmed),
            );
            self.local = self.confirmed;
        }
    }
}

struct ValueDrag {
    node: Value,
    value: f64,
    bounds: Bounds<Pixels>,
    key: SessionKey,
    presentation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OpenTarget {
    Path(String),
    Uri(String),
}

fn row_text(item: &Value) -> &str {
    ["displayText", "summary", "title"]
        .into_iter()
        .filter_map(|key| item[key].as_str())
        .find(|text| !text.trim().is_empty())
        .unwrap_or("")
}

/// Mirrors quickPasteRowActions' offered buttons; the core still validates the
/// selected record and filesystem member before opening or pinning anything.
fn open_target(item: &Value) -> Option<OpenTarget> {
    if matches!(
        item["kind"].as_str(),
        Some("ImageClipItem" | "FileClipItem")
    ) {
        return item["filePaths"][0]
            .as_str()
            .filter(|path| !path.trim().is_empty())
            .map(|path| OpenTarget::Path(path.to_string()));
    }
    let line = row_text(item).trim();
    if !line.contains(['\r', '\n'])
        && (line.starts_with("\\\\")
            || line.starts_with("~/")
            || line.starts_with("~\\")
            || (line.as_bytes().get(1) == Some(&b':')
                && matches!(line.as_bytes().get(2), Some(b'\\' | b'/'))))
    {
        return Some(OpenTarget::Path(
            line.trim_end_matches(['.', ',', ';', '!', '?', ')', ']', '}', '>'])
                .replace('/', "\\"),
        ));
    }
    let lower = line.to_ascii_lowercase();
    let start = [
        lower.find("https://"),
        lower.find("http://"),
        lower.find("www."),
    ]
    .into_iter()
    .flatten()
    .min()?;
    let raw = line[start..]
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\''))
        .next()?
        .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '>']);
    let uri = if raw
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("www."))
    {
        format!("https://{raw}")
    } else {
        raw.to_string()
    };
    if uri
        .split_once("://")
        .is_none_or(|(_, host)| host.is_empty() || host.starts_with('/'))
    {
        return None;
    }
    Some(OpenTarget::Uri(uri))
}

fn can_pin(item: &Value) -> bool {
    if item["kind"].as_str() == Some("ImageClipItem") {
        return true;
    }
    if item["kind"].as_str() == Some("FileClipItem") && item["fileCount"].as_u64() != Some(1) {
        return false;
    }
    let Some(OpenTarget::Path(path)) = open_target(item) else {
        return false;
    };
    image_path(&path)
}

fn clip_kind_view(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "ImageClipItem" => ("image", tr("图片", "Image")),
        "FileClipItem" => ("file", tr("文件", "Files")),
        "LinkClipItem" => ("link", tr("链接", "Link")),
        "HtmlClipItem" | "QQRichTextItem" => ("code", tr("富文本", "Rich text")),
        "CmdClipItem" | "StepClipItem" | "ActionClipItem" => ("play", tr("动作", "Action")),
        _ => ("text", tr("文本", "Text")),
    }
}

/// Source process, natural time and size label from the list DTO only.
fn clip_meta(item: &Value) -> (String, String, String) {
    let source = item["sourceProcessName"].as_str().unwrap_or("").trim().to_string();
    let time = crate::appearance::RowMetadata::new(&source, item["createTime"].as_str().unwrap_or("")).natural_time;
    let english = cliphost_core::i18n::is_english();
    let size = match item["kind"].as_str() {
        Some("FileClipItem") => item["fileCount"].as_u64().filter(|count| *count > 0).map(|count| {
            if english { format!("{count} file{}", if count == 1 { "" } else { "s" }) } else { format!("{count} 个文件") }
        }),
        Some("ImageClipItem") => item["imageSizeBytes"].as_u64().map(|bytes| {
            if bytes >= 1024 * 1024 { format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0) } else { format!("{} KB", bytes.div_ceil(1024)) }
        }),
        _ => item["textLength"].as_u64().filter(|count| *count > 0).map(|count| {
            if english { format!("{count} chars") } else { format!("{count} 字") }
        }),
    };
    (source, time, size.unwrap_or_default())
}

/// Keyboard selection; `None` asks for the next batch instead of wrapping,
/// which is only meaningful once the whole history is loaded.
fn next_clip_index(selected: usize, delta: i32, len: usize, more: bool) -> Option<usize> {
    let next = selected as i64 + i64::from(delta);
    if more && (next < 0 || next >= len as i64) {
        return None;
    }
    Some(next.rem_euclid(len.max(1) as i64) as usize)
}

/// A deletion shifts every later server offset. Retire the old reader before
/// rewinding, so its discarded reply cannot leave pagination permanently busy.
fn clip_page_offset_after_delete(next_skip: usize, pending: &mut Option<Task<()>>) -> usize {
    *pending = None;
    next_skip.saturating_sub(1)
}

const HUB_THUMB_WIDTH: f32 = 112.0;
const HUB_THUMB_HEIGHT: f32 = 72.0;
/// Decoded at twice the row size for high-DPI screens.
const HUB_THUMB_PIXELS: (u32, u32) = (224, 144);
const HUB_THUMB_RETAIN: usize = 96;

#[derive(Clone)]
struct HubThumb {
    image: Arc<Image>,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug, PartialEq)]
enum ClipThumbSource {
    Clip,
    Path(String),
}

fn image_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico"
            )
        })
}

/// Image clips, and a file clip that is exactly one image file.
fn clip_thumb_source(item: &Value) -> Option<ClipThumbSource> {
    match item["kind"].as_str() {
        Some("ImageClipItem") => Some(ClipThumbSource::Clip),
        Some("FileClipItem") if item["fileCount"].as_u64() == Some(1) => item["filePaths"][0]
            .as_str()
            .filter(|path| image_path(path))
            .map(|path| ClipThumbSource::Path(path.to_string())),
        _ => None,
    }
}

/// Decode with bounded allocation on a worker; GPUI receives a small PNG.
fn hub_thumbnail(bytes: Vec<u8>) -> Option<HubThumb> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let (width, height) = (decoded.width(), decoded.height());
    let small = decoded.thumbnail(
        width.min(HUB_THUMB_PIXELS.0),
        height.min(HUB_THUMB_PIXELS.1),
    );
    let mut output = std::io::Cursor::new(Vec::new());
    small.write_to(&mut output, image::ImageFormat::Png).ok()?;
    Some(HubThumb {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, output.into_inner())),
        width,
        height,
    })
}

struct ClipRowView {
    title: String,
    meta: String,
    two_lines: bool,
}

fn file_name(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn parent_folder(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed
        .rfind(['\\', '/'])
        .map(|end| &trimmed[..end])
        .filter(|parent| !parent.is_empty())
        .unwrap_or("")
}

fn link_host(item: &Value) -> String {
    match open_target(item) {
        Some(OpenTarget::Uri(uri)) => uri
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&uri)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_string(),
        _ => String::new(),
    }
}

/// Row title and secondary line from the list DTO, per clip kind.
fn clip_row_view(item: &Value) -> ClipRowView {
    let (source, time, size) = clip_meta(item);
    let join = |parts: &[&str]| {
        parts
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let english = cliphost_core::i18n::is_english();
    match item["kind"].as_str().unwrap_or("") {
        "ImageClipItem" => ClipRowView {
            title: tr("图片", "Image").to_string(),
            meta: join(&[&source, &time, &size]),
            two_lines: false,
        },
        "FileClipItem" => {
            let paths: Vec<&str> = item["filePaths"]
                .as_array()
                .map(|paths| paths.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let count = item["fileCount"].as_u64().unwrap_or(paths.len() as u64) as usize;
            let names: Vec<&str> = paths.iter().take(2).map(|path| file_name(path)).collect();
            let mut title = names.join(if english { ", " } else { "、" });
            if count > names.len() {
                title = if english {
                    format!("{title} + {} more", count - names.len())
                } else {
                    format!("{title} 等 {count} 个")
                };
            }
            if title.is_empty() {
                title = row_text(item).to_string();
            }
            let folder = paths.first().map(|path| parent_folder(path)).unwrap_or("");
            ClipRowView { title, meta: join(&[&time, folder]), two_lines: false }
        }
        kind => {
            let collapsed = row_text(item)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(300)
                .collect();
            let host = if kind == "LinkClipItem" || kind == "TextClipItem" { link_host(item) } else { String::new() };
            ClipRowView {
                title: collapsed,
                meta: join(&[&host, &source, &time]),
                two_lines: kind != "LinkClipItem",
            }
        }
    }
}

fn clipboard_hub_rect(size: (f32, f32), x: f32, y: f32) -> Rect {
    clamp_rect(
        Rect {
            // Shares the strip's left edge, like every other category.
            x,
            y,
            width: 780.0,
            // Keep the hub below the category strip instead of clamping a
            // full-height panel upward over it on shorter displays.
            height: 530.0_f32.min((size.1 - y - EDGE).max(1.0)),
        },
        size,
    )
}

fn color(hex: &str) -> Hsla {
    crate::view::color(hex)
}

struct Preview {
    text: String,
    parts: Vec<(String, Option<Arc<Image>>)>,
    image: Option<Arc<Image>>,
}

fn bytes_image(bytes: Vec<u8>, mime: &str) -> Option<Arc<Image>> {
    Some(Arc::new(Image::from_bytes(
        ImageFormat::from_mime_type(mime)?,
        bytes,
    )))
}

fn load_preview(host: &HostState, item: &Value) -> Result<Preview, String> {
    let id = item["id"].as_str().unwrap_or("");
    let detail = host.detail(&json!({ "id": id }))?;
    let image = if item["kind"].as_str() == Some("ImageClipItem") {
        host.image_bytes(id)
            .ok()
            .and_then(|(bytes, mime)| bytes_image(bytes, &mime))
    } else {
        None
    };
    let mut parts = Vec::new();
    for segment in array(&detail, "imRichSegments") {
        let path = text(&segment, "path");
        let image = if segment["kind"].as_str() == Some("image") {
            host.local_image_bytes(&path)
                .and_then(|(bytes, mime)| bytes_image(bytes, &mime))
        } else {
            None
        };
        let value = match segment["kind"].as_str() {
            Some("file") => path,
            Some("image") if image.is_none() => {
                tr("[图片加载失败]", "[Image failed to load]").to_string()
            }
            _ => text(&segment, "text"),
        };
        parts.push((value, image));
    }
    if item["kind"].as_str() == Some("FileClipItem") {
        for path in array(&detail, "filePaths") {
            if let Some(path) = path.as_str() {
                parts.push((path.to_string(), None));
            }
        }
    }
    let body = if item["kind"].as_str() == Some("StepClipItem") {
        item["stepPresentation"]["previewText"]
            .as_str()
            .map(str::to_string)
    } else {
        None
    }
    .unwrap_or_else(|| text(&detail, "bodyText"));
    Ok(Preview {
        text: body,
        parts,
        image,
    })
}

pub struct SuperMenuWindow {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    session: Option<SessionKey>,
    state: Value,
    category: Category,
    category_wheel: CategoryWheel,
    columns: Vec<Column>,
    status: String,
    busy: bool,
    serial: u64,
    category_request: u64,
    load_task: Option<Task<()>>,
    column_tasks: HashMap<usize, Task<()>>,
    action_task: Option<Task<()>>,
    category_task: Option<Task<()>>,
    selected: usize,
    /// Node icon specs resolved to images, keyed by (spec, dark). `None` is
    /// pending or failed; the row keeps its generic glyph either way.
    node_icons: HashMap<(String, bool), Option<Arc<Image>>>,
    clips: Vec<Value>,
    clips_total: usize,
    clips_more: bool,
    clips_next_skip: usize,
    clips_more_task: Option<Task<()>>,
    clip_scroll: ScrollHandle,
    /// Row thumbnails by clip id; `None` is a completed failed load.
    clip_thumbs: HashMap<String, Option<HubThumb>>,
    clip_thumb_task: Option<Task<()>>,
    preview: Option<Preview>,
    preview_item: Option<String>,
    preview_task: Option<Task<()>>,
    /// Clipboard file item whose paths are registered as file-menu nodes and
    /// shown as column 1 inside the hub; deeper columns cascade beside it.
    clip_files: Option<String>,
    clip_files_task: Option<Task<()>>,
    dirs_open: bool,
    dirs: Value,
    dirs_task: Option<Task<()>>,
    source_hwnd: i64,
    slider_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// Painted bounds of each (column, row); submenus open at the real row,
    /// which separators, tall icons and scrolling move off the nominal grid.
    row_bounds: Rc<RefCell<HashMap<(usize, usize), Bounds<Pixels>>>>,
    column_rects: RefCell<HashMap<usize, Rect>>,
    file_preview_service: Option<Arc<crate::hover::HoverService>>,
    file_preview_owner: Option<Arc<crate::hover::HoverOwner>>,
    file_preview_ticket: Option<((u64, usize), crate::hover::Ticket)>,
    file_preview_enabled: bool,
    value_drag: Option<ValueDrag>,
    value_drafts: HashMap<String, ValueDraft>,
    value_tasks: HashMap<String, Task<()>>,
    value_serial: u64,
    value_presentation: u64,
    active_value: Option<String>,
    keyboard_request: Option<NativeSuperMenuPasteRequest>,
    native_key_request: Option<NativeSuperMenuPasteRequest>,
    value_commit_requests: HashMap<String, NativeSuperMenuPasteRequest>,
    value_keys_down: HashMap<String, String>,
    focus: FocusHandle,
    pin_provider: Option<&'static str>,
    text_launcher: Option<Entity<TextActionLauncher>>,
    text_subscription: Option<Subscription>,
    prepared_category: Option<SessionKey>,
    text_had_focus: bool,
    text_focus_task: Option<Task<()>>,
    browser_panels: Vec<BrowserPanel>,
    browser_next_id: u64,
    browser_active: Option<u64>,
    browser_pending_path: Option<String>,
    browser_pending_split: bool,
    /// (session id, target kind, target title) for the file hint line.
    folder_target_cache: RefCell<Option<(String, &'static str, String)>>,
    #[cfg(debug_assertions)]
    browser_fixture: bool,
    /// Hover thumbnails by HWND, only when `windowThumbnail` is enabled.
    window_thumbs: HashMap<i64, WindowThumb>,
    kill_confirm: Option<KillConfirm>,
    /// In-menu settings panel; values are the settings form's current fields.
    settings_open: bool,
    settings_general: bool,
    settings_values: HashMap<(&'static str, String), Value>,
    settings_reload_pending: bool,
    #[cfg(all(windows, debug_assertions))]
    settings_switch_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
}

impl SuperMenuWindow {
    fn new(host: Arc<HostState>, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) -> Self {
        Self {
            host,
            tokens,
            dark,
            session: None,
            state: Value::Null,
            category: Category::Favor,
            category_wheel: CategoryWheel::default(),
            columns: Vec::new(),
            status: String::new(),
            busy: false,
            serial: 0,
            category_request: 0,
            load_task: None,
            column_tasks: HashMap::new(),
            action_task: None,
            category_task: None,
            selected: 0,
            node_icons: HashMap::new(),
            window_thumbs: HashMap::new(),
            kill_confirm: None,
            settings_open: false,
            settings_general: false,
            settings_values: HashMap::new(),
            settings_reload_pending: false,
            #[cfg(all(windows, debug_assertions))]
            settings_switch_bounds: Rc::default(),
            clips: Vec::new(),
            preview: None,
            preview_item: None,
            preview_task: None,
            clips_total: 0,
            clips_more: false,
            clips_next_skip: 0,
            clips_more_task: None,
            clip_scroll: ScrollHandle::new(),
            clip_thumbs: HashMap::new(),
            clip_thumb_task: None,
            clip_files: None,
            clip_files_task: None,
            dirs_open: false,
            dirs: Value::Null,
            dirs_task: None,
            source_hwnd: 0,
            slider_bounds: Rc::new(RefCell::new(HashMap::new())),
            row_bounds: Rc::new(RefCell::new(HashMap::new())),
            column_rects: RefCell::default(),
            file_preview_service: None,
            file_preview_owner: None,
            file_preview_ticket: None,
            file_preview_enabled: true,
            value_drag: None,
            value_drafts: HashMap::new(),
            value_tasks: HashMap::new(),
            value_serial: 0,
            value_presentation: 0,
            active_value: None,
            keyboard_request: None,
            native_key_request: None,
            value_commit_requests: HashMap::new(),
            value_keys_down: HashMap::new(),
            focus: cx.focus_handle(),
            pin_provider: None,
            text_launcher: None,
            text_subscription: None,
            prepared_category: None,
            text_had_focus: false,
            text_focus_task: None,
            browser_panels: Vec::new(),
            browser_next_id: 0,
            browser_active: None,
            browser_pending_path: None,
            browser_pending_split: false,
            folder_target_cache: RefCell::default(),
            #[cfg(debug_assertions)]
            browser_fixture: false,
        }
    }

    /// Refresh custom colors without reopening the core session or replacing
    /// value drafts, active slider gestures, pending requests or focus.
    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        if dark != self.dark { self.cancel_file_preview(); }
        self.tokens = tokens;
        self.dark = dark;
        if let Some(launcher) = &self.text_launcher {
            launcher.update(cx, |launcher, cx| launcher.set_theme(self.tokens.clone(), dark, cx));
        }
        for panel in &self.browser_panels {
            panel.browser.update(cx, |browser, cx| browser.set_design_tokens(self.tokens.clone(), dark, cx));
        }
        // Value controls render directly from these tokens; they have no
        // separately cached input palette or text input entity to rebuild.
        cx.notify();
    }

    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        hover: Option<Arc<crate::hover::HoverService>>,
        previews_enabled: bool,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut App,
    ) -> Result<(), String> {
        let state = host.super_menu_state_detail();
        if SessionKey::from_state(&state).is_none() {
            return Ok(());
        }
        let handle = if let Some(handle) = existing {
            *handle
        } else {
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        focus: false,
                        show: false,
                        is_resizable: false,
                        is_minimizable: false,
                        window_background: gpui::WindowBackgroundAppearance::Transparent,
                        ..Default::default()
                    },
                    |window, cx| {
                        let close_host = host.clone();
                        window.on_window_should_close(cx, move |_, _| {
                            close_host.dismiss_native_window("super-menu");
                            false
                        });
                        cx.new(|cx| Self::new(host.clone(), tokens.clone(), dark, cx))
                    },
                )
                .map_err(|error| error.to_string())?;
            *existing = Some(handle);
            handle
        };
        handle
            .update(cx, |menu, window, cx| {
                menu.host = host.clone();
                menu.file_preview_service = hover;
                menu.file_preview_enabled = previews_enabled;
                menu.set_design_tokens(tokens, dark, cx);
                #[cfg(windows)]
                {
                    menu.source_hwnd = platform::hwnd(window)?.0 as i64;
                    host.set_super_menu_hwnd(menu.source_hwnd as isize);
                }
                menu.sync(state, cx);
                menu.bind_file_preview();
                // This only chooses GPUI's event dispatch target. It does not
                // activate the OS window or change WS_EX_NOACTIVATE.
                window.focus(&menu.focus);
                if let Some(rect) = host.super_menu_overlay_place() {
                    let key = menu.session.clone();
                    let guarded = host.clone();
                    let _guard = platform::guard_condition(move || {
                        key.as_ref().is_some_and(|key| key.is_current(&guarded))
                    });
                    platform::overlay_show(window, host.host_hwnd(), rect)?;
                }
                menu.prepare_category_input(window, cx)?;
                Ok(())
            })
            .map_err(|error| error.to_string())?
    }

    pub fn maintain(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |menu, window, cx| {
            let state = menu.host.super_menu_state_detail();
            if SessionKey::from_state(&state).is_none() {
                if menu.session.is_some() {
                    menu.hide_inactive(window, cx);
                }
                return;
            }
            if SessionKey::from_state(&state) != menu.session {
                // Creating a root reserves its core generation before the
                // slow environment read completes. Keep this UI busy until
                // the replacement id is published; do not re-enable old rows.
                if menu.category_task.is_some()
                    && SessionKey::from_state(&state)
                        .as_ref()
                        .zip(menu.session.as_ref())
                        .is_some_and(|(next, old)| next.id == old.id)
                {
                    return;
                }
                menu.sync(state, cx);
                if let Some(rect) = menu.host.super_menu_overlay_place() {
                    let key = menu.session.clone();
                    let host = menu.host.clone();
                    let _guard = platform::guard_condition(move || {
                        key.as_ref().is_some_and(|key| key.is_current(&host))
                    });
                    let _ = platform::overlay_show(window, menu.host.host_hwnd(), rect);
                }
            }
            if let Ok(Some(error)) = platform::take_error(window) {
                menu.status = error;
                cx.notify();
            }
            menu.bind_file_preview();
            if let Err(error) = menu.prepare_category_input(window, cx) {
                menu.status = error;
                cx.notify();
            }
            if menu.browsing() && menu.prepared_category.is_some() {
                if let Some(path) = menu.browser_pending_path.take() {
                    let split = std::mem::take(&mut menu.browser_pending_split);
                    menu.browse_from_menu(path, split, window, cx);
                }
            }
            menu.maintain_browser_geometry(window, cx);
            if menu.value_drag.as_ref().is_some_and(|drag| {
                menu.current(&drag.key) && drag.presentation == menu.value_presentation
            }) {
                // Preserve the existing outside-up suppression throughout a
                // held slider gesture, including a pause outside the HWND.
                menu.content_pointer();
            }
        });
    }

    /// Host dismissal owns session retirement. Hiding this surface also drops
    /// pending replies; a close for an older id cannot close a newer core menu.
    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |menu, window, cx| {
            menu.hide_inactive(window, cx);
        });
    }

    fn hide_inactive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() && self.text_focus_task.is_some() { return; }
        self.browser_pending_path = None;
        self.browser_pending_split = false;
        let restore = self.text_had_focus.then(|| self.keyboard_request.clone()).flatten();
        self.retire(cx);
        self.text_had_focus = false;
        cx.notify();
        if let Some(request) = restore {
            let host = self.host.clone();
            self.text_focus_task = Some(cx.spawn_in(window, async move |this, cx| {
                let _ = crate::native_dialog::run(move || host.restore_dismissed_native_super_menu_target(&request)).await;
                let _ = this.update_in(cx, |menu, window, _| {
                    menu.text_focus_task = None;
                    let host = menu.host.clone();
                    let _guard = platform::guard_condition(move || {
                        SessionKey::from_state(&host.super_menu_state_detail()).is_none()
                    });
                    let _ = platform::hide(window);
                });
            }));
        } else {
            let host = self.host.clone();
            let _guard = platform::guard_condition(move || {
                SessionKey::from_state(&host.super_menu_state_detail()).is_none()
            });
            let _ = platform::hide(window);
        }
    }

    pub fn navigate(handle: WindowHandle<Self>, delta: i32, cx: &mut App) {
        let _ = handle.update(cx, |menu, _, cx| menu.select_offset(delta, cx));
    }

    pub fn activate_selected(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |menu, _, cx| menu.activate(cx));
    }

    /// Drain HostState::take_native_super_menu_key on the owning GPUI loop and
    /// deliver each intact event here. Its request/epoch were accepted by the
    /// hook; neither the loop nor this handler recaptures a newer menu ticket.
    pub fn handle_native_key(handle: WindowHandle<Self>, event: NativeSuperMenuKey, cx: &mut App) {
        let _ = handle.update(cx, |menu, window, cx| {
            if !menu.host.native_super_menu_key_is_current(&event)
                || event.scope() != &menu.native_key_scope()
                || !menu.session.as_ref().is_some_and(|key| menu.current(key))
            {
                return;
            }
            window.focus(&menu.focus);
            menu.native_key_request = Some(event.request().clone());
            menu.handle_key(event.key(), event.down(), event.repeat(), cx);
            menu.native_key_request = None;
        });
    }

    fn native_key_scope(&self) -> NativeSuperMenuKeyScope {
        if let Some(id) = self
            .active_value
            .as_deref()
            .filter(|id| self.value_node(id).as_ref().is_some_and(enabled))
        {
            return NativeSuperMenuKeyScope::Value(id.to_string());
        }
        default_native_key_scope(self.category, !self.clips.is_empty())
    }

    fn bind_native_keyboard(&self) {
        if let Some(request) = &self.keyboard_request {
            if self.category == Category::Text || (self.category == Category::File && self.browsing()) || self.text_had_focus {
                let _ = self.host.use_focused_native_super_menu_input(request);
                return;
            }
            let _ = self
                .host
                .bind_native_super_menu_keyboard(request, self.native_key_scope());
        }
    }

    fn accepted_action_request(
        &self,
        key: &SessionKey,
    ) -> Result<NativeSuperMenuPasteRequest, String> {
        if let Some(request) = &self.native_key_request {
            // Preserve the private keyboard epoch through background actions.
            if self.host.native_super_menu_request_is_current(request) {
                return Ok(request.clone());
            }
            return Err("Menu session has ended.".into());
        }
        self.host.capture_native_super_menu_action(
            &key.id,
            key.generation,
            self.source_hwnd as isize,
        )
    }

    fn retire(&mut self, cx: &mut Context<Self>) {
        self.suspend_browsers(cx);
        self.file_preview_ticket = None;
        if let Some(owner) = self.file_preview_owner.take() {
            owner.retire();
        }
        self.column_rects.borrow_mut().clear();
        self.category_wheel.retire();
        self.session = None;
        self.serial = self.serial.wrapping_add(1);
        self.category_request = self.category_request.wrapping_add(1);
        self.columns.clear();
        self.clips.clear();
        self.clips_total = 0;
        self.clips_more = false;
        self.clips_next_skip = 0;
        self.clips_more_task = None;
        self.clip_scroll = ScrollHandle::new();
        self.clip_thumbs.clear();
        self.clip_thumb_task = None;
        self.preview = None;
        self.preview_item = None;
        self.busy = false;
        self.dirs_open = false;
        self.dirs = Value::Null;
        self.load_task = None;
        self.column_tasks.clear();
        self.action_task = None;
        self.category_task = None;
        self.preview_task = None;
        self.clip_files = None;
        self.clip_files_task = None;
        self.dirs_task = None;
        self.value_drag = None;
        self.value_presentation = self.value_presentation.wrapping_add(1);
        self.value_drafts.clear();
        self.value_tasks.clear();
        self.active_value = None;
        self.keyboard_request = None;
        self.native_key_request = None;
        self.value_commit_requests.clear();
        self.value_keys_down.clear();
        self.slider_bounds.borrow_mut().clear();
        self.row_bounds.borrow_mut().clear();
        self.pin_provider = None;
        self.prepared_category = None;
        self.text_focus_task = None;
        self.window_thumbs.clear();
        self.kill_confirm = None;
        self.settings_open = false;
    }

    fn prepare_category_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(key) = self.session.clone().filter(|key| self.current(key)) else { return Ok(()); };
        if self.prepared_category.as_ref() == Some(&key) { return Ok(()); }
        if self.category == Category::File && (self.browsing() || self.browser_pending_path.is_some()) {
            self.prepare_browsers(window, cx)?;
            let host = self.host.clone();
            let current = key.clone();
            let _guard = platform::guard_condition(move || current.is_current(&host));
            platform::set_no_activate(window, false)?;
            if !cliphost_core::is_headless() { platform::show(window, true, true)?; }
            self.focus_active_browser(window, cx);
            self.text_had_focus = true;
            self.bind_native_keyboard();
        } else if self.category == Category::Text {
            if self.text_launcher.is_none() {
                let launcher = cx.new(|cx| TextActionLauncher::new(
                    self.host.clone(), self.tokens.clone(), self.dark, window, cx));
                self.text_subscription = Some(cx.subscribe_in(&launcher, window,
                    |menu, _, event, _, cx| {
                        if menu.category != Category::Text || !menu.session.as_ref().is_some_and(|key| menu.current(key)) { return; }
                        match event {
                            TextActionLauncherEvent::Execute { id, rule_token } => menu.execute_text_rule(id.clone(), rule_token.clone(), cx),
                            TextActionLauncherEvent::Manage => {
                                let host = menu.host.clone();
                                menu.dismiss_now(cx);
                                host.show_web_route("/main/settings/textAction");
                            }
                            TextActionLauncherEvent::Dismiss => menu.dismiss(cx),
                        }
                    }));
                self.text_launcher = Some(launcher);
            }
            if let Some(launcher) = &self.text_launcher {
                launcher.update(cx, |launcher, cx| launcher.reset(window, cx));
            }
            let host = self.host.clone();
            let current = key.clone();
            let _guard = platform::guard_condition(move || current.is_current(&host));
            platform::set_no_activate(window, false)?;
            if !cliphost_core::is_headless() {
                platform::show(window, true, true)?;
            }
            if let Some(launcher) = &self.text_launcher {
                launcher.update(cx, |launcher, cx| launcher.focus_search(window, cx));
            }
            self.text_had_focus = true;
        } else {
            platform::set_no_activate(window, true)?;
            window.focus(&self.focus);
            if self.text_had_focus {
                let request = self.keyboard_request.clone().ok_or("Menu session has ended.")?;
                let host = self.host.clone();
                let restore_key = key.clone();
                let scope = self.native_key_scope();
                self.text_focus_task = Some(cx.spawn(async move |this, cx| {
                    let restore_request = request.clone();
                    let result = crate::native_dialog::run(move || host.restore_native_super_menu_target(&restore_request)).await;
                    let _ = this.update(cx, |menu, cx| {
                        if !menu.current(&restore_key) { return; }
                        menu.text_focus_task = None;
                        match result.and_then(|_| menu.host.resume_native_super_menu_keyboard(&request, scope)) {
                            Ok(()) => menu.text_had_focus = false,
                            Err(error) => {
                                menu.host.show_host_toast(&error, "error", None, None, None);
                                menu.dismiss_now(cx);
                            }
                        }
                        cx.notify();
                    });
                }));
            }
        }
        self.prepared_category = Some(key);
        cx.notify();
        Ok(())
    }

    fn execute_text_rule(&mut self, id: String, rule_token: String, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone().filter(|key| self.current(key)) else { return; };
        if self.busy { return; }
        let request = match self.accepted_action_request(&key) {
            Ok(request) => request,
            Err(error) => { self.status = error; cx.notify(); return; }
        };
        let reply_request = request.clone();
        let presentation = self.value_presentation;
        self.busy = true;
        self.status = tr("正在执行…", "Running…").into();
        let host = self.host.clone();
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::native_dialog::run(move || host.text_action_execute_native_checked(&id, &rule_token, &request)).await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation { return; }
                menu.busy = false;
                menu.action_task = None;
                match result {
                    Ok(_) if menu.host.native_super_menu_close_reply_is_current(&reply_request) => menu.dismiss(cx),
                    Ok(_) => {},
                    Err(error) => {
                        menu.status = error;
                        if let Some(launcher) = &menu.text_launcher { launcher.update(cx, |launcher, cx| launcher.reload(cx)); }
                    },
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn sync(&mut self, state: Value, cx: &mut Context<Self>) {
        let key = SessionKey::from_state(&state);
        if key == self.session {
            return;
        }
        let keep_dirs = self.dirs_open
            && self.category == Category::File
            && Category::from_action(&state["action"]) == Category::File;
        self.retire(cx);
        self.session = key;
        self.category = Category::from_action(&state["action"]);
        self.columns = vec![Column::from_page(
            String::new(),
            "children",
            0.0,
            &state["root"],
        )];
        self.status = text(&state, "message");
        self.state = state;
        self.keyboard_request = self.session.as_ref().and_then(|key| {
            self.host
                .capture_native_super_menu_action(
                    &key.id,
                    key.generation,
                    self.source_hwnd as isize,
                )
                .ok()
        });
        self.bind_native_keyboard();
        self.selected = 0;
        if self.category == Category::Clipboard {
            self.load_clips(cx);
        }
        if keep_dirs {
            self.dirs_open = true;
            self.load_dirs(cx);
        }
        cx.notify();
    }

    fn current(&self, key: &SessionKey) -> bool {
        self.session.as_ref() == Some(key) && key.is_current(&self.host)
    }

    fn render_current(&self, key: &Option<SessionKey>, presentation: u64) -> bool {
        presentation == self.value_presentation && key.as_ref().is_some_and(|key| self.current(key))
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.text_had_focus {
            if let Some(key) = self.session.clone().filter(|key| self.current(key)) {
                if self.host.menu_close_session(&key.id) {
                    // Retire the execution lease immediately. Hide/maintain
                    // keeps the original request long enough to hand back focus
                    // through the one-use dismissed-presentation fence.
                    self.action_task = None;
                    self.busy = true;
                    cx.notify();
                    return;
                }
            }
        }
        self.dismiss_now(cx);
    }

    fn dismiss_now(&mut self, cx: &mut Context<Self>) {
        self.text_had_focus = false;
        self.browser_pending_path = None;
        if let Some(key) = &self.session {
            self.host.menu_close_session(&key.id);
        }
        self.retire(cx);
        cx.notify();
    }

    fn content_pointer(&self) {
        if self
            .session
            .as_ref()
            .is_some_and(|key| key.is_current(&self.host))
        {
            self.host
                .super_menu_web_action(&json!({ "action": "contentPointer" }));
        }
    }

    fn bind_file_preview(&mut self) {
        if self.file_preview_owner.as_ref().is_some_and(|owner| owner.is_current()) {
            return;
        }
        self.file_preview_owner = None;
        if self.source_hwnd == 0 || self.session.is_none() {
            return;
        }
        if let Some(service) = &self.file_preview_service {
            match service.bind_owner(crate::hover::Owner {
                hwnd: self.source_hwnd as isize,
                session: self.value_presentation,
            }) {
                Ok(owner) => self.file_preview_owner = Some(Arc::new(owner)),
                Err(error) => tracing::debug!(%error, "File menu hover unavailable"),
            }
        }
    }

    fn cancel_file_preview(&mut self) {
        self.file_preview_ticket = None;
        if let Some(owner) = &self.file_preview_owner {
            owner.cancel();
        }
    }

    fn leave_file_preview(&mut self, entry: u64, row: usize) {
        if self.file_preview_ticket.as_ref().is_some_and(|(key, _)| *key == (entry, row)) {
            if let Some((_, ticket)) = self.file_preview_ticket.take() {
                if let Some(owner) = &self.file_preview_owner {
                    owner.leave(ticket);
                }
            }
        }
    }

    fn preview_file(&mut self, index: usize, row: usize, node: &Value, window: &Window) {
        if !self.file_preview_enabled || self.busy || self.settings_open || self.dirs_open
            || self.kill_confirm.is_some() {
            self.cancel_file_preview();
            return;
        }
        self.bind_file_preview();
        let Some(content) = file_preview::content_for_node(node) else { return };
        let Some(column) = self.columns.get(index) else { return };
        let key = (column.entry, row);
        let Some(bounds) = self.row_bounds.borrow().get(&(index, row)).copied() else { return };
        let beside = file_preview::avoid_bounds(self.column_rects.borrow().iter()
            .filter(|(index, _)| **index < self.columns.len()).map(|(_, rect)| *rect));
        let (Some(beside), Some(owner)) = (beside, self.file_preview_owner.clone()) else { return };
        if let Some((_, old)) = self.file_preview_ticket.take() {
            owner.leave(old);
        }
        match owner.schedule_beside(crate::hover::Request {
            anchor: [f32::from(bounds.left()) as f64, f32::from(bounds.top()) as f64,
                f32::from(bounds.size.width) as f64, f32::from(bounds.size.height) as f64],
            scale: window.scale_factor() as f64,
            dark: self.dark,
            content,
        }, Some(beside)) {
            Ok(Some(ticket)) => self.file_preview_ticket = Some((key, ticket)),
            Ok(None) => {},
            Err(error) => tracing::debug!(%error, "File menu preview unavailable"),
        }
    }

    fn change_category(&mut self, category: Category, cx: &mut Context<Self>) {
        self.cancel_file_preview();
        let Some(key) = self.session.clone() else {
            return;
        };
        if self.busy || !self.current(&key) {
            return;
        }
        self.busy = true;
        self.status = tr("加载中…", "Loading…").to_string();
        self.category_request = self.category_request.wrapping_add(1);
        let request = self.category_request;
        let host = self.host.clone();
        let source = self.source_hwnd;
        self.category_task = Some(cx.spawn(async move |this, cx| {
            let key_for_load = key.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.menu_create_session_for_current(
                        &key_for_load.id,
                        key_for_load.generation,
                        category.action(),
                        source,
                    )
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if menu.category_request != request || menu.session.as_ref() != Some(&key) {
                    return;
                }
                menu.category_task = None;
                menu.busy = false;
                match result {
                    Ok(_) => {
                        let keep_settings = menu.settings_open;
                        let general = menu.settings_general;
                        let same_category = menu.category == category;
                        let state = menu.host.super_menu_state_detail();
                        menu.sync(state, cx);
                        if keep_settings {
                            menu.open_settings();
                            if same_category {
                                menu.settings_general = general;
                            }
                        }
                        if std::mem::take(&mut menu.settings_reload_pending) {
                            menu.change_category(menu.category, cx);
                        }
                    }
                    Err(error) if menu.current(&key) => {
                        menu.status = error;
                        cx.notify();
                    }
                    Err(_) => {}
                }
            });
        }));
        cx.notify();
    }

    fn open_column(
        &mut self,
        index: usize,
        node: Value,
        context: bool,
        anchor_y: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.session.clone() else {
            return;
        };
        if !self.current(&key) || index + 1 >= MAX_COLUMNS || (!context && !enabled(&node)) {
            return;
        }
        let parent = text(&node, "id");
        let collection = node_collection(context);
        if self
            .columns
            .get(index + 1)
            .is_some_and(|col| col.parent == parent && col.collection == collection)
        {
            return;
        }
        self.cancel_file_preview();
        self.columns.truncate(index + 1);
        self.column_tasks
            .retain(|task_index, _| *task_index <= index);
        self.serial = self.serial.wrapping_add(1);
        let nonce = self.serial;
        let mut column = Column::from_page(parent, collection, anchor_y, &Value::Null);
        column.loading = true;
        column.nonce = nonce;
        self.columns.push(column);
        self.load_column(index + 1, false, key, nonce, cx);
        cx.notify();
    }

    fn load_column(
        &mut self,
        index: usize,
        append: bool,
        key: SessionKey,
        nonce: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = self.columns.get(index) else {
            return;
        };
        let parent = column.parent.clone();
        let collection = column.collection;
        let cursor = if append { column.cursor.clone() } else { None };
        let presentation = self.value_presentation;
        let host = self.host.clone();
        let task = cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let load_parent = parent.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    load_menu_column(&host, &load_key, &load_parent,
                        collection, cursor.as_deref())
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key)
                    || !menu.columns.get(index).is_some_and(|col| {
                        col.nonce == nonce && col.parent == parent && col.collection == collection
                    })
                    || menu.value_presentation != presentation
                {
                    return;
                }
                menu.column_tasks.remove(&index);
                menu.cancel_file_preview();
                match result {
                    Ok(page) => {
                        let old = if append {
                            menu.columns[index].items.clone()
                        } else {
                            Vec::new()
                        };
                        let y = menu.columns[index].anchor_y;
                        let mut col = Column::from_page(parent, collection, y, &page)
                            .refresh_of(&menu.columns[index]);
                        col.nonce = nonce;
                        if append {
                            let mut items = old;
                            for item in col.items {
                                if !items.iter().any(|old| old["id"] == item["id"]) {
                                    items.push(item);
                                }
                            }
                            col.items = items;
                        }
                        menu.columns[index] = col;
                    }
                    Err(error) => {
                        menu.columns[index].loading = false;
                        menu.columns[index].status = "Error".to_string();
                        menu.columns[index].message = error.clone();
                        menu.status = error;
                    }
                }
                cx.notify();
            });
        });
        self.column_tasks.insert(index, task);
    }

    fn more(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone() else {
            return;
        };
        if !self.current(&key) {
            return;
        }
        let Some(col) = self.columns.get_mut(index) else {
            return;
        };
        if col.loading || col.cursor.is_none() {
            return;
        }
        self.serial = self.serial.wrapping_add(1);
        col.nonce = self.serial;
        col.loading = true;
        let nonce = col.nonce;
        self.load_column(index, true, key, nonce, cx);
        cx.notify();
    }

    fn value_node(&self, id: &str) -> Option<Value> {
        self.columns
            .iter()
            .flat_map(|column| column.items.iter())
            .find(|node| node["id"].as_str() == Some(id) && node["kind"].as_str() == Some("Value"))
            .cloned()
    }

    fn edit_value(&mut self, node: &Value, value: f64) -> bool {
        let id = text(node, "id");
        self.value_serial = self.value_serial.wrapping_add(1);
        let revision = self.value_serial;
        let draft = self
            .value_drafts
            .entry(id.clone())
            .or_insert_with(|| ValueDraft::new(node.clone(), revision));
        self.active_value = Some(id);
        let changed = draft.edit(value, revision);
        self.bind_native_keyboard();
        changed
    }

    fn commit_value(&mut self, node: Value, cx: &mut Context<Self>) {
        let id = text(&node, "id");
        let Some(node) = self.value_node(&id).filter(enabled) else {
            return;
        };
        if !enabled(&node) || !self.session.as_ref().is_some_and(|key| self.current(key)) {
            return;
        }
        let Some(key) = self.session.as_ref() else {
            return;
        };
        let Ok(request) = self.accepted_action_request(key) else {
            return;
        };
        self.value_commit_requests.insert(id.clone(), request);
        let revision = self.value_serial;
        let draft = self
            .value_drafts
            .entry(id.clone())
            .or_insert_with(|| ValueDraft::new(node, revision));
        draft.queue_commit();
        self.submit_value(id, cx);
        cx.notify();
    }

    fn submit_value(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone().filter(|key| self.current(key)) else {
            return;
        };
        let Some(submission) = self
            .value_drafts
            .get_mut(&id)
            .and_then(ValueDraft::begin_submit)
        else {
            return;
        };
        let request = match self
            .value_commit_requests
            .get(&id)
            .cloned()
            .ok_or_else(|| "Menu session has ended.".to_string())
        {
            Ok(request) => request,
            Err(error) => {
                if let Some(draft) = self.value_drafts.get_mut(&id) {
                    draft.in_flight = None;
                }
                self.status = error;
                cx.notify();
                return;
            }
        };
        let presentation = self.value_presentation;
        let host = self.host.clone();
        let reply_request = request.clone();
        let task_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let params = json!({ "nodeId": task_id, "value": submission.value });
            let result = cx
                .background_executor()
                .spawn(async move {
                    // The native core API rechecks this session after reading the
                    // registry node and immediately before changing its HWND.
                    host.menu_set_value_native(&params, &request)
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation {
                    return;
                }
                if !menu
                    .host
                    .native_super_menu_request_is_current(&reply_request)
                {
                    // A scope/foreground retirement cancels this original
                    // commit. Release only its matching in-flight slot; a
                    // later pointer commit keeps its own accepted request.
                    if let Some(draft) = menu
                        .value_drafts
                        .get_mut(&task_id)
                        .filter(|draft| draft.in_flight == Some(submission))
                    {
                        draft.in_flight = None;
                        if !menu
                            .value_commit_requests
                            .get(&task_id)
                            .is_some_and(|request| {
                                menu.host.native_super_menu_request_is_current(request)
                            })
                        {
                            draft.queued = None;
                        }
                        menu.value_tasks.remove(&task_id);
                        menu.submit_value(task_id, cx);
                        cx.notify();
                    }
                    return;
                }
                let Some(draft) = menu
                    .value_drafts
                    .get_mut(&task_id)
                    .filter(|draft| draft.in_flight == Some(submission))
                else {
                    return;
                };
                let latest = draft.revision == submission.revision
                    && !draft.editing
                    && draft.queued.is_none();
                match result {
                    Ok(result) => {
                        let patched = result["patchedItem"]
                            .as_object()
                            .map(|_| &result["patchedItem"]);
                        draft.accept_reply(submission, patched);
                        if let Some(patched) = patched {
                            menu.patch_node(patched);
                        }
                        if latest {
                            menu.status = result["message"].as_str().unwrap_or("").to_string();
                        }
                    }
                    Err(error) => {
                        draft.in_flight = None;
                        // Keep the failed draft visible. An earlier error must
                        // not replace a newer edit/commit's status.
                        if latest {
                            menu.status = error;
                        }
                    }
                }
                menu.value_tasks.remove(&task_id);
                menu.submit_value(task_id, cx);
                cx.notify();
            });
        });
        self.value_tasks.insert(id, task);
    }

    fn move_value_drag(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.value_drag.as_ref().is_some_and(|drag| {
            self.current(&drag.key) && drag.presentation == self.value_presentation
        }) {
            return;
        }
        let Some(drag) = &mut self.value_drag else {
            return;
        };
        let fraction = f64::from(
            (event.position.x - drag.bounds.left()) / drag.bounds.size.width.max(px(1.0)),
        );
        drag.value = slider_value(&drag.node, fraction);
        let node = drag.node.clone();
        let value = drag.value;
        let changed = self.edit_value(&node, value);
        if event.pressed_button != Some(MouseButton::Left) {
            // A lost capture can report the released button on the next move.
            // Match the browser pointercancel path while the session is valid.
            self.finish_value_drag(None, cx);
        } else if changed {
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn finish_value_drag(
        &mut self,
        position: Option<gpui::Point<Pixels>>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.value_drag.as_ref().is_some_and(|drag| {
            self.current(&drag.key) && drag.presentation == self.value_presentation
        }) {
            return false;
        }
        let Some(mut drag) = self.value_drag.take() else {
            return false;
        };
        if let Some(position) = position {
            let fraction =
                f64::from((position.x - drag.bounds.left()) / drag.bounds.size.width.max(px(1.0)));
            drag.value = slider_value(&drag.node, fraction);
            self.edit_value(&drag.node, drag.value);
        }
        self.commit_value(drag.node, cx);
        true
    }

    fn handle_key(&mut self, key: &str, down: bool, repeat: bool, cx: &mut Context<Self>) -> bool {
        if !self.session.as_ref().is_some_and(|key| self.current(key)) {
            return false;
        }
        if down { self.cancel_file_preview(); }
        let value_key = match key {
            "left" | "ArrowLeft" => "left",
            "right" | "ArrowRight" => "right",
            "home" | "Home" => "home",
            "end" | "End" => "end",
            _ => key,
        };
        let key_owner = if down {
            None
        } else {
            self.value_keys_down.remove(value_key)
        };
        if let Some(node) = self
            .active_value
            .as_deref()
            .and_then(|id| self.value_node(id))
            .filter(enabled)
        {
            let id = text(&node, "id");
            let current = self
                .value_drafts
                .get(&id)
                .map(|draft| draft.local)
                .unwrap_or_else(|| node["value"]["current"].as_f64().unwrap_or(0.0));
            if let Some(value) = key_value(&node, current, key) {
                if down {
                    self.value_keys_down.insert(value_key.to_string(), id);
                    if self.edit_value(&node, value) {
                        cx.notify();
                    }
                } else {
                    if key_owner.as_deref() != Some(id.as_str()) {
                        return false;
                    }
                    self.commit_value(node, cx);
                }
                return true;
            }
        }
        if !down {
            return false;
        }
        match key {
            "escape" | "Escape" if self.kill_confirm.is_some() => {
                self.kill_confirm = None;
                cx.notify();
            }
            "escape" | "Escape" if self.settings_open => {
                self.settings_open = false;
                cx.notify();
            }
            "escape" | "Escape" => self.dismiss(cx),
            "up" | "ArrowUp"
                if self.category == Category::Clipboard && self.active_value.is_none() =>
            {
                self.select_offset(-1, cx)
            }
            "down" | "ArrowDown"
                if self.category == Category::Clipboard && self.active_value.is_none() =>
            {
                self.select_offset(1, cx)
            }
            "enter" | "Enter" => {
                if self.active_value.is_some()
                    || self.category != Category::Clipboard
                {
                    return false;
                }
                if !repeat {
                    self.activate(cx);
                }
            }
            _ => return false,
        }
        true
    }

    fn invoke(&mut self, node: Value, value: Option<f64>, keep_open: bool, cx: &mut Context<Self>) {
        self.cancel_file_preview();
        if let Some(value) = value {
            self.edit_value(&node, value);
            self.commit_value(node, cx);
            return;
        }
        if node["kind"].as_str() == Some("Value") {
            return;
        }
        self.active_value = None;
        let Some(key) = self.session.clone() else {
            return;
        };
        if self.busy || !enabled(&node) || !self.current(&key) {
            return;
        }
        let request = match self.accepted_action_request(&key) {
            Ok(request) => request,
            Err(error) => {
                self.status = error;
                cx.notify();
                return;
            }
        };
        let reply_request = request.clone();
        let presentation = self.value_presentation;
        self.busy = true;
        let host = self.host.clone();
        let id = text(&node, "id");
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            // Core menu commands may open native pickers indirectly. Keep the
            // whole dispatch off GPUI's MTA pool and leave its UI task free.
            let result = crate::native_dialog::run(move || {
                if !load_key.is_current(&host) {
                    return Err("Session not found".to_string());
                }
                host.menu_invoke_native(&id, &json!({}), &request)
            })
            .await;
            let _ = this.update(cx, |menu, cx| {
                if menu.current(&key)
                    && menu.value_presentation == presentation
                    && !keep_open
                    && result
                        .as_ref()
                        .is_ok_and(|result| result["closeMenu"].as_bool().unwrap_or(true))
                    && menu
                        .host
                        .native_super_menu_close_reply_is_current(&reply_request)
                {
                    menu.dismiss(cx);
                    return;
                }
                if !menu.current(&key) || menu.value_presentation != presentation {
                    return;
                }
                menu.busy = false;
                menu.action_task = None;
                if !menu
                    .host
                    .native_super_menu_request_is_current(&reply_request)
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(result) => {
                        if let Some(path) = result["browsePath"].as_str() {
                            menu.browser_pending_path = Some(path.to_string());
                            menu.browser_pending_split = false;
                            if !menu.browsing() {
                                menu.prepared_category = None;
                            }
                            if menu.category != Category::File {
                                menu.change_category(Category::File, cx);
                            }
                            cx.notify();
                            return;
                        }
                        let patched = &result["patchedItem"];
                        if !patched.is_null() {
                            menu.patch_node(patched);
                        }
                        menu.status = result["message"].as_str().unwrap_or("").to_string();
                        if !keep_open && result["closeMenu"].as_bool().unwrap_or(true) {
                            menu.dismiss(cx);
                        } else {
                            match result["refresh"].as_str().unwrap_or("None") {
                                "Root" | "Session" => menu.change_category(menu.category, cx),
                                "ParentChildren" => {
                                    if let Some(index) = menu.columns.iter().position(|col| {
                                        col.items.iter().any(|item| item["id"] == node["id"])
                                    }) {
                                        menu.reload_column(index, cx);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(error) => menu.status = error,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn reload_column(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone() else {
            return;
        };
        if index == 0 {
            self.change_category(self.category, cx);
            return;
        }
        if let Some(col) = self.columns.get_mut(index) {
            self.serial = self.serial.wrapping_add(1);
            col.nonce = self.serial;
            col.loading = true;
            let nonce = col.nonce;
            self.load_column(index, false, key, nonce, cx);
        }
    }

    fn patch_node(&mut self, patched: &Value) {
        if let Some(draft) = self.value_drafts.get_mut(&text(patched, "id")) {
            draft.sync_node(patched);
        }
        for col in &mut self.columns {
            for item in &mut col.items {
                if item["id"] == patched["id"] {
                    *item = patched.clone();
                }
            }
        }
    }

    fn select_offset(&mut self, delta: i32, cx: &mut Context<Self>) {
        if !self.session.as_ref().is_some_and(|key| self.current(key))
            || self.category != Category::Clipboard
            || self.clips.is_empty()
        {
            return;
        }
        let Some(next) = next_clip_index(self.selected, delta, self.clips.len(), self.clips_more) else {
            self.load_more_clips(cx);
            return;
        };
        self.selected = next;
        self.clip_scroll.scroll_to_item(self.selected);
        self.load_clips_near(self.selected, cx);
        self.request_clip_thumbs(cx);
        self.load_clip_preview(cx);
        cx.notify();
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        if self.category == Category::Clipboard {
            self.clip_action("paste", cx);
        }
    }

    fn load_clips(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone() else {
            return;
        };
        let host = self.host.clone();
        let presentation = self.value_presentation;
        self.clips_more_task = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let result = cx.background_executor().spawn(async move {
                if !load_key.is_current(&host) { return Err("Session not found".to_string()); }
                let page = host.search_items(&json!({ "webSessionId": load_key.id, "type": "all", "query": "", "skip": 0, "take": HUB_TAKE }))?;
                Ok((page, host.pin_overlay_status()))
            }).await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation || menu.category != Category::Clipboard { return; }
                menu.load_task = None;
                match result { Ok((page, pin)) => {
                    menu.pin_provider = if pin["snipasteAvailable"].as_bool() == Some(true) { Some("snipaste") } else if pin["pixPinAvailable"].as_bool() == Some(true) { Some("pixpin") } else { None };
                    menu.clips = array(&page, "items"); menu.selected = menu.selected.min(menu.clips.len().saturating_sub(1));
                    menu.accept_clip_page(&page); menu.load_clip_preview(cx); menu.request_clip_thumbs(cx);
                }, Err(error) => menu.status = error }
                cx.notify();
            });
        }));
    }

    fn accept_clip_page(&mut self, page: &Value) {
        self.clips_total = page["total"].as_u64().unwrap_or(self.clips.len() as u64) as usize;
        self.clips_more = page["hasMore"].as_bool() == Some(true);
        self.clips_next_skip = page["nextSkip"].as_u64().unwrap_or(self.clips.len() as u64) as usize;
    }

    /// Appends the next history batch; requests are serialized and a stale
    /// reply (session, category or offset changed meanwhile) is dropped.
    fn load_more_clips(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.clips_more || self.clips_more_task.is_some() || self.load_task.is_some() {
            return;
        }
        let Some(key) = self.session.clone() else {
            return;
        };
        if !self.current(&key) || self.category != Category::Clipboard {
            return;
        }
        let skip = self.clips_next_skip;
        let host = self.host.clone();
        let presentation = self.value_presentation;
        self.clips_more_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let result = cx.background_executor().spawn(async move {
                if !load_key.is_current(&host) { return Err("Session not found".to_string()); }
                host.search_items(&json!({ "webSessionId": load_key.id, "type": "all", "query": "", "skip": skip, "take": HUB_TAKE }))
            }).await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation
                    || menu.category != Category::Clipboard || menu.clips_next_skip != skip
                {
                    return;
                }
                menu.clips_more_task = None;
                match result {
                    Ok(page) => {
                        let known: std::collections::HashSet<String> =
                            menu.clips.iter().map(|item| text(item, "id")).collect();
                        menu.clips.extend(array(&page, "items").into_iter()
                            .filter(|item| !known.contains(&text(item, "id"))));
                        menu.accept_clip_page(&page);
                        menu.request_clip_thumbs(cx);
                    }
                    Err(error) => {
                        // A failed read does not mean the history has ended.
                        // Leave the next offset available for a later wheel/Down retry.
                        menu.status = error;
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load_clips_near(&mut self, index: usize, cx: &mut Context<Self>) {
        if index + HUB_PREFETCH_ROWS >= self.clips.len() {
            self.load_more_clips(cx);
        }
    }

    fn load_clip_preview(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone() else {
            return;
        };
        let Some(item) = self.clips.get(self.selected).cloned() else {
            self.preview = None;
            self.preview_item = None;
            self.load_clip_files(&Value::Null, cx);
            return;
        };
        let id = text(&item, "id");
        if self.preview_item.as_ref() == Some(&id) {
            return;
        }
        self.preview_item = Some(id.clone());
        self.preview = None;
        self.load_clip_files(&item, cx);
        let presentation = self.value_presentation;
        let host = self.host.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_preview(&host, &item) })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key)
                    || menu.value_presentation != presentation
                    || menu.preview_item.as_ref() != Some(&id)
                {
                    return;
                }
                menu.preview_task = None;
                match result {
                    Ok(preview) => menu.preview = Some(preview),
                    Err(error) => menu.status = error,
                }
                cx.notify();
            });
        }));
    }

    /// File items reuse the file category's nodes: folders expand through the
    /// disclosure button and right-click opens the same context menu.
    fn load_clip_files(&mut self, item: &Value, cx: &mut Context<Self>) {
        self.cancel_file_preview();
        self.columns.truncate(1);
        self.column_tasks.retain(|index, _| *index == 0);
        self.clip_files = None;
        self.clip_files_task = None;
        let Some(key) = self.session.clone() else {
            return;
        };
        if item["kind"].as_str() != Some("FileClipItem") {
            return;
        }
        let id = text(item, "id");
        let presentation = self.value_presentation;
        let host = self.host.clone();
        self.clip_files = Some(id.clone());
        self.clip_files_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let load_id = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !load_key.is_current(&host) {
                        return Err("Session not found".to_string());
                    }
                    host.menu_clip_files_node(&load_key.id, &load_id)
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key)
                    || menu.value_presentation != presentation
                    || menu.clip_files.as_ref() != Some(&id)
                {
                    return;
                }
                menu.clip_files_task = None;
                match result {
                    Ok(node) => menu.open_column(0, node, false, 0.0, cx),
                    Err(error) => menu.status = error,
                }
                cx.notify();
            });
        }));
    }

    fn clip_action(&mut self, action: &'static str, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone() else {
            return;
        };
        let Some(item) = self.clips.get(self.selected).cloned() else {
            return;
        };
        if self.busy || !self.current(&key) {
            return;
        }
        // Capture this menu's target before history I/O, independent of the
        // clipboard root's visibility and input lifetime.
        let action_request = match self.accepted_action_request(&key) {
            Ok(request) => request,
            Err(error) => {
                self.status = error;
                cx.notify();
                return;
            }
        };
        let reply_request = action_request.clone();
        let presentation = self.value_presentation;
        let paste = if action == "paste" || action == "simulate" {
            if self.native_key_request.is_some() {
                // A missing target remains an error in the existing native
                // paste API. Never replace this keyboard request with a fresh
                // menu/clipboard destination ticket.
                Some(action_request.clone())
            } else {
                match self.host.capture_super_menu_paste(
                    &key.id,
                    key.generation,
                    self.source_hwnd as isize,
                ) {
                    Ok(request) => Some(request),
                    Err(error) => {
                        self.status = error;
                        cx.notify();
                        return;
                    }
                }
            }
        } else {
            None
        };
        self.busy = true;
        if action == "delete" {
            // Retire the reader before the DB mutation starts. Waiting for the
            // delete reply could merge an offset page read after the deletion.
            self.clips_more_task = None;
        }
        let host = self.host.clone();
        let id = text(&item, "id");
        let id_after = id.clone();
        let pin_provider = self.pin_provider;
        let open = open_target(&item);
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !load_key.is_current(&host)
                        || !host.native_super_menu_request_is_current(&action_request)
                    {
                        return Err("Session not found".to_string());
                    }
                    match action {
                        "paste" => host.paste_super_menu_clip(
                            &id,
                            paste.as_ref().expect("captured menu paste"),
                        ),
                        "simulate" => host.simulate_super_menu_clip(
                            &id,
                            paste.as_ref().expect("captured menu input"),
                        ),
                        "copy" => host.copy_items(&json!({ "ids": [id] })),
                        "delete" => host.delete_item(&json!({ "id": id })),
                        "pin" => host.pin_overlay(
                            &json!({ "ids": [id], "provider": pin_provider.unwrap_or("snipaste") }),
                        ),
                        "open" => {
                            let detail = host.detail(&json!({ "id": id }))?;
                            if !load_key.is_current(&host) {
                                return Err("Session not found".to_string());
                            }
                            match open {
                                Some(OpenTarget::Uri(uri)) => {
                                    host.shell_open_uri(&json!({ "uri": uri }))
                                }
                                Some(OpenTarget::Path(path))
                                    if item["kind"].as_str() == Some("FileClipItem") =>
                                {
                                    host.open_item_path(&json!({ "id": id, "path": path }))
                                }
                                Some(OpenTarget::Path(path)) => {
                                    host.shell_open_path(&json!({ "path": path }))
                                }
                                None => {
                                    let path = detail["linkUrl"].as_str().unwrap_or("");
                                    host.shell_open_uri(&json!({ "uri": path }))
                                }
                            }
                        }
                        _ => Err(tr("操作不可用", "Action unavailable").to_string()),
                    }
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if menu.current(&key)
                    && menu.value_presentation == presentation
                    && result.is_ok()
                    && matches!(action, "paste" | "open" | "simulate")
                    && menu
                        .host
                        .native_super_menu_close_reply_is_current(&reply_request)
                {
                    menu.dismiss(cx);
                    return;
                }
                if !menu.current(&key) || menu.value_presentation != presentation {
                    return;
                }
                menu.action_task = None;
                menu.busy = false;
                if !menu
                    .host
                    .native_super_menu_request_is_current(&reply_request)
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(_) if action == "paste" || action == "open" || action == "simulate" => {
                        menu.dismiss(cx)
                    }
                    Ok(_) if action == "delete" => {
                        // Keep the loaded batches and scroll position; the
                        // server-side offset shifts by the removed row.
                        if let Some(index) = menu.clips.iter().position(|clip| text(clip, "id") == id_after) {
                            menu.clips_next_skip = clip_page_offset_after_delete(
                                menu.clips_next_skip,
                                &mut menu.clips_more_task,
                            );
                            menu.clips.remove(index);
                            menu.clips_total = menu.clips_total.saturating_sub(1);
                            menu.selected = menu.selected.min(menu.clips.len().saturating_sub(1));
                        }
                        menu.preview_item = None;
                        menu.load_clip_preview(cx);
                        menu.load_clips_near(menu.selected, cx);
                    }
                    Ok(_) => menu.status = tr("操作完成", "Done").to_string(),
                    Err(error) => menu.status = error,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load_dirs(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.session.clone() else {
            return;
        };
        let host = self.host.clone();
        let presentation = self.value_presentation;
        self.dirs_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.file_menu_snapshot() })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation || !menu.dirs_open
                {
                    return;
                }
                menu.dirs = result;
                menu.dirs_task = None;
                cx.notify();
            });
        }));
    }

    fn mutate_dirs(
        &mut self,
        action: &'static str,
        index: usize,
        path: String,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.session.clone() else {
            return;
        };
        if self.busy || !self.current(&key) {
            return;
        }
        let presentation = self.value_presentation;
        let mut lines: Vec<String> = array(&self.dirs, "lines")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        match action {
            "remove" => {
                if index >= lines.len() {
                    return;
                }
                lines.remove(index);
            }
            "up" if index > 0 && index < lines.len() => lines.swap(index, index - 1),
            "down" if index + 1 < lines.len() => lines.swap(index, index + 1),
            "separator" => lines.push("---".to_string()),
            "add" => lines.push(path),
            "pick" => {}
            _ => return,
        }
        let picker_request = if action == "pick" {
            match self.accepted_action_request(&key) {
                Ok(request) => Some(request),
                Err(error) => {
                    self.status = error;
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.busy = true;
        let host = self.host.clone();
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !load_key.is_current(&host) {
                        return Err("Session not found".to_string());
                    }
                    if action == "pick" {
                        let picked = crate::native_dialog::run_blocking(|| {
                            host.pick_folder_super_menu_native(
                                &json!({}),
                                picker_request.as_ref().expect("captured menu picker"),
                            )
                        })?;
                        if picked["cancelled"].as_bool() == Some(true) {
                            return Ok(None);
                        }
                        let Some(path) = picked["path"].as_str().filter(|path| !path.is_empty())
                        else {
                            return Ok(None);
                        };
                        if !load_key.is_current(&host) {
                            return Err("Session not found".to_string());
                        }
                        lines.push(path.to_string());
                    }
                    host.file_menu_replace(lines).map(Some)
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation {
                    return;
                }
                menu.busy = false;
                menu.action_task = None;
                match result {
                    Ok(Some(dirs)) => {
                        menu.dirs = dirs;
                        menu.change_category(Category::File, cx);
                    }
                    Ok(None) => {}
                    Err(error) => menu.status = error,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Queue background loads for icons of the visible column nodes.
    fn request_node_icons(&mut self, cx: &mut Context<Self>) {
        let dark = self.dark;
        let mut wanted = Vec::new();
        for node in self.columns.iter().flat_map(|column| column.items.iter()) {
            if node["kind"].as_str() == Some("Check") && !window_check(node) {
                continue;
            }
            if let Some(spec) = node_icon_spec(node) {
                let key = (spec.to_string(), dark);
                if !self.node_icons.contains_key(&key) && !wanted.contains(&key) {
                    wanted.push(key);
                }
            }
        }
        if wanted.is_empty() {
            return;
        }
        if self.node_icons.len() + wanted.len() > NODE_ICON_CACHE_LIMIT {
            self.node_icons.clear();
        }
        let foreground = self.tokens.theme(dark).text_muted.clone();
        for key in wanted {
            self.node_icons.insert(key.clone(), None);
            let host = self.host.clone();
            let foreground = foreground.clone();
            cx.spawn(async move |this, cx| {
                let spec = key.0.clone();
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        crate::native_content::load_icon_spec(&host, &spec, &foreground)
                    })
                    .await;
                let Some(image) = image else { return };
                let _ = this.update(cx, |menu, cx| {
                    if let Some(slot) = menu.node_icons.get_mut(&key) {
                        *slot = Some(image);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn node_icon(&self, node: &Value, size: f32) -> Option<AnyElement> {
        let image = node_icon_spec(node)
            .and_then(|spec| self.node_icons.get(&(spec.to_string(), self.dark)))
            .and_then(Option::as_ref);
        match image {
            Some(image) => Some(
                img(image.clone())
                    .size(px(size))
                    .flex_none()
                    .object_fit(gpui::ObjectFit::Contain)
                    .into_any_element(),
            ),
            None => self
                .icon(icon_key(node), size)
                .map(IntoElement::into_any_element),
        }
    }

    fn icon(&self, key: &str, size: f32) -> Option<crate::crisp_svg::CrispSvg> {
        let key = match key {
            "window" => "all",
            "link" => "list",
            key => key,
        };
        self.tokens.icons.get(key).map(|path| {
            svg()
                .path(path.clone())
                .size(px(size))
                .flex_none()
                .text_color(color(&self.tokens.theme(self.dark).text_muted))
        })
    }

    fn button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let hover = color(&t.hover);
        rsx! {
            <div id={id} flex items_center justify_center gap={px(6.0)} px={px(10.0)}
                py={px(5.0)} rounded={px(7.0)} border_1 border_color={color(&t.border)}
                bg={color(&t.secondary)} cursor_pointer />
        }
            .hover(move |style| style.bg(hover))
            .child(label.into())
    }

    fn panel(&self, id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div id={id} flex flex_col rounded={px(12.0)} border_1
                border_color={color(&t.border)} bg={color(&t.surface)} shadow_lg />
        }
    }

    /// Submenu top that lines its first row up with the painted parent row.
    fn row_anchor(&self, index: usize, row: usize, fallback: f32) -> f32 {
        self.row_bounds
            .borrow()
            .get(&(index, row))
            .map_or(fallback, |bounds| f32::from(bounds.top()) - COLUMN_PADDING)
    }

    fn node_row(
        &self,
        index: usize,
        row: usize,
        node: &Value,
        y: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if node["kind"].as_str() == Some("Separator") {
            return rsx! {
                <div h={px(7.0)} px={px(7.0)}>
                    <div h={px(1.0)} mt={px(3.0)}
                        bg={color(&self.tokens.theme(self.dark).border)} />
                </div>
            }
                .into_any_element();
        }
        let node = node.clone();
        let invoke_node = node.clone();
        let hover_node = node.clone();
        let context_node = node.clone();
        let row_key = self.session.clone();
        let row_presentation = self.value_presentation;
        let pointer_key = row_key.clone();
        let context_key = row_key.clone();
        let hover_key = row_key.clone();
        let row_entry = self.columns[index].entry;
        let invoke_key = row_key.clone();
        let active = enabled(&node);
        let t = self.tokens.theme(self.dark);
        let hover = color(&t.hover);
        let mut font = self.state["style"]["menuFontSize"]
            .as_f64()
            .unwrap_or(13.0)
            .clamp(10.0, 24.0) as f32;
        if matches!(self.category, Category::File | Category::Clipboard) {
            font = font.min(12.0);
        }
        let mut icon_size = node["visual"]["iconSize"]
            .as_f64()
            .or_else(|| self.state["style"]["iconSize"].as_f64())
            .unwrap_or(16.0)
            .clamp(8.0, 64.0) as f32;
        if matches!(self.category, Category::File | Category::Clipboard) {
            icon_size = icon_size.min(14.0);
        }
        // Window submenus use one 16 DIP command glyph; app icons keep the menu size.
        if self.category == Category::Window
            && node["visual"]["window"].is_null()
            && node["visual"]["windowGroup"].is_null()
        {
            icon_size = icon_size.min(16.0);
        }
        let expanded = self
            .columns
            .get(index + 1)
            .is_some_and(|col| col.parent == node["id"].as_str().unwrap_or(""));
        let content_height = (font * 1.25).max(20.0);
        let row_bounds = self.row_bounds.clone();
        let mut item = rsx! {
            <div id={("super-node", index * 100_000 + row)} relative flex flex_none items_center gap={px(8.0)}
                min_h={px(content_height + 8.0)} px={px(9.0)} py={px(4.0)} rounded={px(6.0)}
                text_size={px(font)} line_height={px(font * 1.25)} />
        }
            .child(
                canvas(
                    move |bounds, window, _| {
                        row_bounds.borrow_mut().insert((index, row),
                            bounds.intersect(&window.content_mask().bounds));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when(expanded, |item| item.bg(color(&t.selected)))
            .opacity(if active {
                node["visual"]["opacity"].as_f64().unwrap_or(1.0) as f32
            } else {
                0.45
            })
            .hover(move |style| style.bg(hover))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |menu, _, _, cx| {
                    if !menu.render_current(&pointer_key, row_presentation) {
                        return;
                    }
                    menu.content_pointer();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |menu, _, _, cx| {
                    if !menu.render_current(&context_key, row_presentation) {
                        return;
                    }
                    menu.content_pointer();
                    cx.stop_propagation();
                    if context_node["hasContextMenu"].as_bool() == Some(true) {
                        let y = menu.row_anchor(index, row, y);
                        menu.open_column(index, context_node.clone(), true, y, cx);
                    }
                }),
            )
            .on_hover(cx.listener(move |menu, hovered: &bool, window, cx| {
                if !menu.render_current(&hover_key, row_presentation)
                    || !menu.columns.get(index).is_some_and(|col| col.entry == row_entry
                        && col.items.get(row).is_some_and(|node| node["id"] == hover_node["id"])) {
                    return;
                }
                if !*hovered {
                    menu.leave_file_preview(row_entry, row);
                    return;
                }
                // The hub's own file list never opens or closes submenus on
                // hover; the disclosure button and right-click are explicit.
                if menu.category == Category::Clipboard && index == 1 {
                    menu.preview_file(index, row, &hover_node, window);
                    return;
                }
                if menu.category == Category::File && index == 0 && menu.browsing() {
                    // The root column is the docked browser's sidebar: hover
                    // never covers the panels. A clicked group can be swapped
                    // for another group; any other row closes it.
                    if menu.columns.len() > 1 && enabled(&hover_node) {
                        if expandable(&hover_node) && !primary_invokes(&hover_node) {
                            let y = menu.row_anchor(index, row, y);
                            menu.open_column(index, hover_node.clone(), false, y, cx);
                        } else {
                            menu.columns.truncate(1);
                            menu.column_tasks.retain(|task_index, _| *task_index == 0);
                            cx.notify();
                        }
                    }
                    return;
                }
                if *hovered && enabled(&hover_node) {
                    if expandable(&hover_node) {
                        let y = menu.row_anchor(index, row, y);
                        menu.open_column(index, hover_node.clone(), false, y, cx);
                    } else if menu.columns.len() > index + 1 {
                        menu.cancel_file_preview();
                        menu.columns.truncate(index + 1);
                        menu.column_tasks
                            .retain(|task_index, _| *task_index <= index);
                        cx.notify();
                    }
                    if file_preview::content_for_node(&hover_node).is_some() {
                        menu.preview_file(index, row, &hover_node, window);
                    } else {
                        menu.cancel_file_preview();
                    }
                }
            }))
            .on_click(cx.listener(move |menu, event: &gpui::ClickEvent, window, cx| {
                if !menu.render_current(&invoke_key, row_presentation) {
                    return;
                }
                if event.click_count() > 1 {
                    return;
                }
                if !enabled(&invoke_node) {
                    return;
                }
                if invoke_node["kind"].as_str() == Some("Value") {
                    return;
                }
                // With panels docked, a folder click opens there (Ctrl: side by
                // side); the footer sends the folder to the accepted target.
                if menu.category == Category::File && menu.browsing() {
                    if let Some(path) = browser_workspace::browse_path(&invoke_node) {
                        menu.browse_from_menu(path, event.modifiers().control, window, cx);
                        return;
                    }
                }
                if window_kill_role(&invoke_node) {
                    let parent = index
                        .checked_sub(1)
                        .and_then(|parent| menu.columns.get(parent))
                        .zip(menu.columns.get(index))
                        .and_then(|(parent, column)| {
                            parent.items.iter().find(|node| text(node, "id") == column.parent)
                        });
                    menu.kill_confirm = Some(kill_confirm_for(&invoke_node, parent));
                    cx.notify();
                } else if primary_invokes(&invoke_node) {
                    menu.invoke(invoke_node.clone(), None, event.modifiers().control, cx);
                } else if expandable(&invoke_node) {
                    let y = menu.row_anchor(index, row, y);
                    menu.open_column(index, invoke_node.clone(), false, y, cx);
                }
            }));
        let window = window_visual(&node).cloned();
        let minimized = window.as_ref().is_some_and(|v| v["minimized"].as_bool() == Some(true));
        if window_check(&node) {
            item = item.children(self.node_icon(&node, icon_size.min(16.0)));
        } else if minimized {
            item = item.child(div().flex_none().opacity(0.55).children(self.node_icon(&node, icon_size)));
        } else if node["kind"].as_str() == Some("Check") {
            item = item.child(div().w(px(16.0)).child(
                if node["check"]["isChecked"].as_bool() == Some(true) {
                    "✓"
                } else {
                    ""
                },
            ));
        } else {
            item = item.children(self.node_icon(&node, icon_size));
        }
        let group = node["visual"]["windowGroup"].as_object().map(|_| node["visual"]["windowGroup"].clone());
        let field = node["visual"]["field"].as_object().map(|_| node["visual"]["field"].clone());
        if let Some(window) = &window {
            let title = text(window, "title");
            let mut label = rsx! {
                <div flex_1 min_w_0 overflow_hidden whitespace_nowrap text_ellipsis>{title.clone()}</div>
            };
            if minimized {
                label = label.text_color(color(&t.text_secondary));
            }
            item = item.child(label).children(window_chips(window).into_iter().map(|(label, accent)| self.chip(label, accent)));
            if window["topmost"].as_bool() == Some(true) {
                item = item.children(self.tinted_icon("pin", 12.0, color(&t.pin)));
            }
            item = item.tooltip(crate::settings::settings_help(
                format!("{title}\n{} · {}", text(window, "app"), text(window, "exe")),
                t.clone(),
            ));
        } else if let Some(group) = &group {
            item = item
                .child(rsx! {
                    <div flex_1 min_w_0 flex flex_col overflow_hidden>
                        <div overflow_hidden whitespace_nowrap text_ellipsis font_weight={gpui::FontWeight::SEMIBOLD}>
                            {text(group, "app")}
                        </div>
                        <div overflow_hidden whitespace_nowrap text_ellipsis text_size={px((font - 2.0).max(9.0))}
                            line_height={px((font - 2.0).max(9.0) * 1.3)} text_color={color(&t.text_muted)}>
                            {text(group, "recent")}
                        </div>
                    </div>
                })
                .when(group["foreground"].as_bool() == Some(true), |item| {
                    item.child(self.chip(tr("前台", "Active").to_string(), true))
                })
                .child(self.count_pill(group["count"].as_u64().unwrap_or(0)))
                .tooltip(crate::settings::settings_help(text(&node, "description"), t.clone()));
        } else if let Some(field) = &field {
            // Information is the reason to open this menu: show complete values
            // rather than truncating them like ordinary command labels.
            item = item
                .child(rsx! {
                    <div w={px(34.0)} flex_none text_size={px((font - 1.0).max(9.0))} text_color={color(&t.text_muted)}>
                        {text(field, "label")}
                    </div>
                })
                .child(div().flex_1().min_w_0().child(text(field, "value")))
                .tooltip(crate::settings::settings_help(
                    format!("{}\n{}", text(&node, "description"), tr("点击复制", "Click to copy")),
                    t.clone(),
                ));
        } else if node["role"].as_str() == Some("window-copy") {
            // Information is the reason to open this menu: show complete values
            // rather than truncating them like ordinary command labels.
            item = item.child(div().flex_1().min_w_0().child(node_label(&node)))
                .tooltip(crate::settings::settings_help(text(&node, "description"), t.clone()));
        } else {
            item = item.child(rsx! {
                <div flex_1 min_w_0 overflow_hidden whitespace_nowrap text_ellipsis>
                    {node_label(&node)}
                </div>
            });
            if !self.file_preview_enabled || self.file_preview_owner.is_none()
                || file_preview::content_for_node(&node).is_none() {
                item = item.tooltip(crate::settings::settings_help({
                    let label = node_label(&node);
                    let description = text(&node, "description");
                    if description.is_empty() || description == label {
                        label
                    } else {
                        format!("{label}\n{description}")
                    }
                }, t.clone()));
            }
        }
        if let Some(path) = (self.category == Category::File)
            .then(|| browser_workspace::browse_path(&node))
            .flatten()
        {
            if self.browsing() {
                let current = self
                    .browser_panels
                    .iter()
                    .find(|p| Some(p.id) == self.browser_active)
                    .is_some_and(|p| p.browser.read(cx).current_path().eq_ignore_ascii_case(path.trim_end_matches('\\'))
                        || p.browser.read(cx).current_path().eq_ignore_ascii_case(&path));
                if current {
                    item = item.bg(color(&t.accent).opacity(0.14)).border_l_2().border_color(color(&t.accent));
                }
            } else if !self.host.file_menu_prefers_internal_browser() || self.folder_target().0 == "dialog" {
                let group = SharedString::from(format!("super-row-{index}-{row}"));
                let browse_key = self.session.clone();
                let middle_key = self.session.clone();
                let middle_path = path.clone();
                item = item
                    .group(group.clone())
                    .on_mouse_down(MouseButton::Middle, cx.listener(move |menu, _, window, cx| {
                        if menu.render_current(&middle_key, row_presentation) {
                            menu.content_pointer();
                            cx.stop_propagation();
                            menu.browse_from_menu(middle_path.clone(), true, window, cx);
                        }
                    }))
                    .child(
                        div()
                            .id(("super-browse", index * 100_000 + row))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .w(px(22.0))
                            .h(px(content_height))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .opacity(0.0)
                            .group_hover(group, |style| style.opacity(1.0))
                            .hover(move |style| style.bg(hover))
                            .children(self.icon("open-panel", 13.0))
                            .tooltip(crate::settings::settings_help(tr("在浏览栏打开", "Open in the docked browser").to_string(), t.clone()))
                            .on_mouse_down(MouseButton::Left, cx.listener(|menu, _, _, cx| {
                                menu.content_pointer();
                                cx.stop_propagation();
                            }))
                            .on_click(cx.listener(move |menu, _, window, cx| {
                                if !menu.render_current(&browse_key, row_presentation) {
                                    return;
                                }
                                cx.stop_propagation();
                                menu.browse_from_menu(path.clone(), false, window, cx);
                            })),
                    );
            }
        }
        if window_kill_role(&node) {
            item = item.text_color(color(&t.error));
        }
        if matches!(node["role"].as_str(), Some("window-kill" | "window-center")) {
            let hint = text(&node, "description");
            if !hint.is_empty() {
                item = item.child(rsx! {
                    <div flex_none max_w={px(120.0)} overflow_hidden whitespace_nowrap text_ellipsis
                        text_size={px((font - 1.5).max(9.0))} text_color={color(&t.text_muted)}>{hint}</div>
                });
            }
        }
        if window_check(&node) && !matches!(node["role"].as_str(), Some("window-min" | "window-max")) {
            let checked = node["check"]["isChecked"].as_bool() == Some(true);
            let mut boxed = rsx! {
                <div flex flex_none items_center justify_center size={px(16.0)} rounded={px(4.0)} border_1 />
            };
            boxed = if checked {
                boxed.border_color(color(&t.accent)).bg(color(&t.accent)).text_color(gpui::white())
                    .text_size(px(11.0)).child("✓")
            } else {
                boxed.border_color(color(&t.border))
            };
            item = item.child(boxed);
        }
        if node["kind"].as_str() == Some("Value") {
            let current = self
                .value_drafts
                .get(&text(&node, "id"))
                .map(|draft| draft.local)
                .unwrap_or_else(|| node["value"]["current"].as_f64().unwrap_or(0.0));
            let (min, max, _) = value_range(&node);
            let bounds_map = self.slider_bounds.clone();
            let id = text(&node, "id");
            let slider_node = node.clone();
            let slider_key = self.session.clone();
            let slider_presentation = self.value_presentation;
            let ratio = if max > min {
                ((current - min) / (max - min)).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            item = item.child(value_percent(&node, current)).child(
                rsx! {
                    <div id={("super-value-slider", index * 100_000 + row)} relative w={px(96.0)}
                        h={px(22.0)} flex_none cursor_pointer />
                }
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                bounds_map.borrow_mut().insert(id, bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .size_full(),
                    )
                    .child(
                        rsx! {
                            <div absolute left_0 right_0 top={px(9.0)} h={px(4.0)}
                                rounded={px(2.0)} bg={color(&t.border)} />
                        },
                    )
                    .child(
                        rsx! {
                            <div absolute left_0 top={px(9.0)} w={relative(ratio)} h={px(4.0)}
                                rounded={px(2.0)} bg={color(&t.accent)} />
                        },
                    )
                    .child(
                        rsx! {
                            <div absolute left={relative(ratio)} top={px(5.0)} size={px(12.0)}
                                rounded_full bg={color(&t.accent)} />
                        },
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |menu, event: &gpui::MouseDownEvent, _, cx| {
                            if !menu.render_current(&slider_key, slider_presentation)
                                || !enabled(&slider_node)
                            {
                                return;
                            }
                            cx.stop_propagation();
                            menu.content_pointer();
                            let bounds = menu
                                .slider_bounds
                                .borrow()
                                .get(&text(&slider_node, "id"))
                                .copied();
                            if let Some(bounds) = bounds {
                                let fraction = f64::from(
                                    (event.position.x - bounds.left())
                                        / bounds.size.width.max(px(1.0)),
                                );
                                let value = slider_value(&slider_node, fraction);
                                menu.edit_value(&slider_node, value);
                                menu.value_drag = Some(ValueDrag {
                                    node: slider_node.clone(),
                                    value,
                                    bounds,
                                    key: slider_key.clone().expect("current render has a session"),
                                    presentation: slider_presentation,
                                });
                                cx.notify();
                            }
                        }),
                    )
                    .on_click(|_, _, cx| cx.stop_propagation()),
            );
        } else if expandable(&node) {
            let expand_node = node;
            item = item.child(
                // Keep the disclosure inside the row's content box. The shared
                // action button adds vertical padding and makes folder rows taller.
                rsx! {
                    <div id={("super-expand", index * 100_000 + row)} flex flex_none
                        items_center justify_center w={px(24.0)} h={px(content_height)}
                        rounded={px(4.0)} cursor_pointer />
                }
                    .children(self.icon("chevron-right", 13.0))
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(move |menu, _, _, cx| {
                        if !menu.render_current(&row_key, row_presentation) {
                            return;
                        }
                        cx.stop_propagation();
                        let y = menu.row_anchor(index, row, y);
                        menu.open_column(index, expand_node.clone(), false, y, cx);
                    })),
            );
        } else if node["hasContextMenu"].as_bool() == Some(true) {
            let context_node = node;
            item = item.child(rsx! {
                <div id={("super-context", index * 100_000 + row)} flex flex_none
                    items_center justify_center w={px(24.0)} h={px(content_height)}
                    rounded={px(4.0)} cursor_pointer>
                    "⋮"
                </div>
            }.hover(move |style| style.bg(hover))
                .tooltip(crate::settings::settings_help(tr("更多操作", "More actions").to_string(), t.clone()))
                .on_click(cx.listener(move |menu, _, _, cx| {
                    if !menu.render_current(&row_key, row_presentation) {
                        return;
                    }
                    cx.stop_propagation();
                    let y = menu.row_anchor(index, row, y);
                    menu.open_column(index, context_node.clone(), true, y, cx);
                })));
        }
        item.into_any_element()
    }

    fn tree(
        &self,
        size: (f32, f32),
        anchor: (f32, f32),
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.tree_from(0, None, size, anchor, cx)
    }

    /// Columns from `start`, cascading beside `first` (the clipboard hub) when given.
    fn tree_from(
        &self,
        start: usize,
        first: Option<Rect>,
        size: (f32, f32),
        anchor: (f32, f32),
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut width = self.state["style"]["menuMaxWidth"]
            .as_f64()
            .unwrap_or(280.0)
            .clamp(160.0, 500.0) as f32;
        if matches!(self.category, Category::File | Category::Clipboard) {
            width = width.min(240.0);
        }
        let mut font = self.state["style"]["menuFontSize"]
            .as_f64()
            .unwrap_or(13.0)
            .clamp(10.0, 24.0) as f32;
        if matches!(self.category, Category::File | Category::Clipboard) {
            font = font.min(12.0);
        }
        let row_height = (font * 1.25).max(20.0) + 8.0;
        let mut previous = first;
        let mut side = CascadeSide::Right;
        let mut result = Vec::new();
        for (index, col) in self.columns.iter().enumerate().skip(start) {
            let column_key = self.session.clone();
            let column_presentation = self.value_presentation;
            let column_entry = col.entry;
            // Window submenus name their target window; the root lists the mouse mapping.
            let window_parent = (self.category == Category::Window && index > 0)
                .then(|| self.columns[index - 1].items.iter().find(|node| text(node, "id") == col.parent))
                .flatten()
                .filter(|node| window_visual(node).is_some());
            let thumbnail = window_parent.is_some()
                && col.collection == "children"
                && self.state["style"]["windowThumbnail"].as_bool() == Some(true);
            let window_hint = if self.category != Category::Window || col.items.is_empty() {
                None
            } else if index == 0 {
                Some(tr("单击切换 · 悬停操作 · 右键信息", "Click to switch · Hover for actions · Right-click for info"))
            } else if window_parent.is_some() && col.collection == "children" {
                Some(tr("右键窗口查看信息", "Right-click a window for info"))
            } else {
                None
            };
            let chrome = window_parent.map_or(0.0, |_| WINDOW_HEADER_HEIGHT)
                + if thumbnail { WINDOW_THUMB_HEIGHT + 6.0 } else { 0.0 }
                + window_hint.map_or(0.0, |_| WINDOW_HINT_HEIGHT);
            let height = chrome + if col.items.iter().any(|node| node["role"].as_str() == Some("window-copy")) {
                // Field values wrap; let this card grow to its actual content
                // before scrolling, up to the remaining screen space.
                (size.1 - EDGE - col.anchor_y.max(anchor.1)).max(70.0)
            } else {
                (col.items.len() as f32 * row_height
                    + 16.0
                    + if col.cursor.is_some() { 34.0 } else { 0.0 })
                .max(70.0)
            };
            // The strip sits right above the menu, so tall columns of any
            // category scroll below it instead of being clamped up over it.
            // The root never rises at all: every category starts at one top.
            let strip = self.strip_rect(size);
            let ceiling = if previous.is_none() { anchor.1 } else { strip.y + strip.height + 8.0 };
            let height = height.min((size.1 - EDGE - ceiling).max(70.0));
            let (rect, next_side) = column_rect(previous, side, anchor, col.anchor_y, width, height, size);
            self.column_rects.borrow_mut().insert(index, rect);
            previous = Some(rect);
            side = next_side;
            let mut card = self
                .panel(("super-column", index))
                // Opaque painting alone does not occlude GPUI hitboxes. Block
                // parent hover/click/scroll even when this column flips over it.
                .occlude()
                .absolute()
                .left(px(rect.x))
                .top(px(rect.y))
                .w(px(rect.width))
                .max_h(px(rect.height))
                .p(px(COLUMN_PADDING))
                .overflow_y_scroll()
                .smooth_scroll(("super-column", column_entry), cx)
                .on_scroll_wheel(cx.listener(move |menu, event: &gpui::ScrollWheelEvent, _, cx| {
                    if event.delta.pixel_delta(px(20.0)).y < px(0.0)
                        && menu.render_current(&column_key, column_presentation)
                        && menu.columns.get(index).is_some_and(|col| col.entry == column_entry)
                    {
                        // Match the shared frontend: each downward wheel asks
                        // for a batch, even before this column needs to scroll.
                        // Keep the default overflow scrolling for the same event.
                        menu.more(index, cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|menu, _, _, cx| {
                        menu.content_pointer();
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|menu, _, _, cx| {
                        menu.content_pointer();
                        cx.stop_propagation();
                    }),
                );
            if let Some(parent) = window_parent {
                card = card.child(self.window_header(parent, window_visual(parent).unwrap_or(&Value::Null), thumbnail));
            }
            if col.loading && col.items.is_empty() {
                card = card.child(div().p(px(12.0)).child(tr("加载中…", "Loading…")));
            } else if col.items.is_empty() {
                card = card.child(div().p(px(12.0)).child(if col.message.is_empty() {
                    tr("没有菜单项", "No menu items").to_string()
                } else {
                    col.message.clone()
                }));
            } else {
                for (row, node) in col.items.iter().enumerate() {
                    card = card.child(self.node_row(
                        index,
                        row,
                        node,
                        // Nominal fallback until the row has been painted.
                        rect.y + row as f32 * row_height,
                        cx,
                    ));
                }
            }
            if col.status == "Error" {
                card = card.child(
                    rsx! {
                        <div p={px(8.0)} text_color={color(&self.tokens.theme(self.dark).error)}>
                            {col.message.clone()}
                        </div>
                    },
                );
            }
            if let Some(hint) = window_hint {
                card = card.child(self.hint_line(hint.to_string()));
            }
            if col.cursor.is_some() {
                card = card.child(
                    rsx! {
                        <div id={("super-more", index)} px={px(10.0)} py={px(5.0)}
                            text_color={color(&self.tokens.theme(self.dark).text_muted)} />
                    }
                        .child(if col.loading {
                            tr("加载中…", "Loading…")
                        } else {
                            tr("向下滚动加载更多", "Scroll down to load more")
                        }),
                );
            }
            // `super-menu-column-in`: 120ms ease-out, opacity and translateX(-4px).
            if motion::reduced_motion() {
                result.push(card.into_any_element());
            } else {
                let left = rect.x;
                result.push(
                    card.with_animation(
                        ("super-column-in", col.entry),
                        motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                        move |card, t| card.opacity(t).left(px(left - 4.0 * (1.0 - t))),
                    )
                    .into_any_element(),
                );
            }
        }
        result
    }

    fn tinted_icon(&self, key: &str, size: f32, tint: Hsla) -> Option<crate::crisp_svg::CrispSvg> {
        self.tokens
            .icons
            .get(key)
            .map(|path| svg().path(path.clone()).size(px(size)).flex_none().text_color(tint))
    }

    fn chip(&self, label: String, accent: bool) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        let (fg, bg) = if accent {
            let accent = color(&t.accent);
            (accent, accent.opacity(0.14))
        } else {
            (color(&t.text_secondary), color(&t.tertiary))
        };
        rsx! {
            <div flex flex_none items_center h={px(16.0)} px={px(5.0)} rounded={px(4.0)}
                text_size={px(10.0)} line_height={px(16.0)} text_color={fg} bg={bg}>{label}</div>
        }
    }

    fn count_pill(&self, count: u64) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div flex flex_none items_center justify_center min_w={px(18.0)} h={px(16.0)} px={px(5.0)}
                rounded={px(8.0)} text_size={px(10.0)} line_height={px(16.0)}
                text_color={color(&t.text_secondary)} bg={color(&t.tertiary)}>{count.to_string()}</div>
        }
    }

    /// Target of a window submenu: optional thumbnail, icon, title and app · exe.
    fn window_header(&self, parent: &Value, visual: &Value, thumbnail: bool) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        let mut header = rsx! {
            <div flex flex_col flex_none gap={px(6.0)} px={px(4.0)} pt={px(4.0)} pb={px(6.0)} mb={px(4.0)}
                border_b_1 border_color={color(&t.border)} />
        };
        if thumbnail {
            let mut frame = rsx! {
                <div flex flex_col flex_none items_center justify_center gap={px(6.0)} h={px(WINDOW_THUMB_HEIGHT)}
                    overflow_hidden rounded={px(5.0)} bg={color(&t.tertiary)} border_1 border_color={color(&t.border)}
                    text_size={px(10.5)} text_color={color(&t.text_muted)} />
            };
            frame = match self.window_thumbs.get(&window_handle(parent)) {
                Some(WindowThumb::Ready(image)) => frame.child(
                    img(image.clone()).max_w_full().max_h_full().object_fit(gpui::ObjectFit::Contain),
                ),
                Some(WindowThumb::Missing) => frame.children(self.node_icon(parent, 24.0)).child(
                    if visual["minimized"].as_bool() == Some(true) {
                        tr("已最小化，暂无画面", "Minimized, no preview")
                    } else {
                        tr("暂无画面", "No preview")
                    },
                ),
                _ => frame,
            };
            header = header.child(frame);
        }
        header.child(
            rsx! { <div flex items_center gap={px(9.0)} min_w_0 px={px(2.0)} /> }
                .children(self.node_icon(parent, 24.0))
                .child(rsx! {
                    <div flex flex_col flex_1 min_w_0>
                        <div text_size={px(12.0)} line_height={px(16.0)} font_weight={gpui::FontWeight::SEMIBOLD}
                            line_clamp={2}>{text(visual, "title")}</div>
                        <div overflow_hidden whitespace_nowrap text_ellipsis text_size={px(10.5)} line_height={px(14.0)}
                            text_color={color(&t.text_muted)}>{format!("{} · {}", text(visual, "app"), text(visual, "exe"))}</div>
                    </div>
                }),
        )
    }

    fn hint_line(&self, label: String) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div flex_none mt={px(2.0)} px={px(8.0)} pt={px(5.0)} pb={px(2.0)} border_t_1 border_color={color(&t.border)}
                text_size={px(10.5)} text_color={color(&t.text_muted)} whitespace_nowrap overflow_hidden text_ellipsis>{label}</div>
        }
    }

    /// Thumbnails for open window-actions submenus, captured off the UI thread.
    fn request_window_thumbs(&mut self, cx: &mut Context<Self>) {
        if self.category != Category::Window || self.state["style"]["windowThumbnail"].as_bool() != Some(true) {
            return;
        }
        let mut wanted = Vec::new();
        for pair in self.columns.windows(2) {
            if pair[1].collection != "children" {
                continue;
            }
            let Some(parent) = pair[0].items.iter().find(|node| text(node, "id") == pair[1].parent) else {
                continue;
            };
            let hwnd = window_handle(parent);
            if window_visual(parent).is_some() && hwnd != 0 && !self.window_thumbs.contains_key(&hwnd) {
                wanted.push(hwnd);
            }
        }
        for hwnd in wanted {
            self.window_thumbs.insert(hwnd, WindowThumb::Loading);
            let session = self.session.clone();
            cx.spawn(async move |this, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        cliphost_core::window_thumb::try_get_cached_or_capture(hwnd as isize, 320)
                            .and_then(|jpeg| bytes_image(jpeg, "image/jpeg"))
                    })
                    .await;
                let _ = this.update(cx, |menu, cx| {
                    if menu.session != session {
                        return;
                    }
                    if let Some(slot) = menu.window_thumbs.get_mut(&hwnd) {
                        *slot = image.map_or(WindowThumb::Missing, WindowThumb::Ready);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn kill_confirm_card(&self, size: (f32, f32), anchor: (f32, f32), cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.kill_confirm.as_ref()?;
        let t = self.tokens.theme(self.dark);
        let width = 320.0_f32.min((size.0 - EDGE * 2.0).max(1.0));
        let x = anchor.0.clamp(EDGE, (size.0 - width - EDGE).max(EDGE));
        let y = anchor.1.clamp(EDGE, (size.1 - 170.0 - EDGE).max(EDGE));
        let node = confirm.node.clone();
        let error = color(&t.error);
        Some(
            self.panel("super-kill-confirm")
                .occlude()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(width))
                .p(px(16.0))
                .gap(px(8.0))
                .bg(color(&t.background))
                .on_mouse_down(MouseButton::Left, cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                }))
                .child(rsx! { <div text_size={px(13.0)} font_weight={gpui::FontWeight::MEDIUM}>{confirm.title.clone()}</div> })
                .child(rsx! { <div text_size={px(11.5)} line_height={px(18.0)} text_color={color(&t.text_secondary)}>{confirm.body.clone()}</div> })
                .child(
                    rsx! { <div flex justify_end gap={px(8.0)} mt={px(8.0)} /> }
                        .child(self.button("super-kill-cancel", tr("取消", "Cancel")).on_click(cx.listener(|menu, _, _, cx| {
                            cx.stop_propagation();
                            menu.kill_confirm = None;
                            cx.notify();
                        })))
                        .child(
                            self.button("super-kill-confirm-button", confirm.confirm.clone())
                                .text_color(error)
                                .border_color(error.opacity(0.45))
                                .on_click(cx.listener(move |menu, _, _, cx| {
                                    cx.stop_propagation();
                                    menu.kill_confirm = None;
                                    menu.invoke(node.clone(), None, false, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_general = category_settings(self.category).is_empty();
        self.load_setting_values();
    }

    fn load_setting_values(&mut self) {
        self.settings_values.clear();
        for config in ["menu_global", "menu_file"] {
            let Ok(form) = self.host.settings_form_get(&json!({ "configId": config })) else {
                continue;
            };
            for field in form["fields"].as_array().into_iter().flatten() {
                if let Some(key) = field["key"].as_str() {
                    self.settings_values.insert((config, key.to_string()), field["value"].clone());
                }
            }
        }
    }

    fn setting_value(&self, spec: &SettingSpec) -> Value {
        self.settings_values.get(&(spec.config, spec.key.to_string())).cloned().unwrap_or(Value::Null)
    }

    /// Saves through the settings form (same validation and files as the main
    /// window), then rebuilds the menu so the change shows next to the panel.
    fn set_menu_setting(&mut self, spec: SettingSpec, value: Value, cx: &mut Context<Self>) {
        let mut values = serde_json::Map::new();
        values.insert(spec.key.to_string(), value);
        match self.host.settings_form_update(&json!({ "configId": spec.config, "values": values })) {
            Ok(_) => {
                self.load_setting_values();
                if self.busy {
                    self.settings_reload_pending = true;
                } else {
                    self.change_category(self.category, cx);
                    // The rebuild is quick; don't flash the category "加载中…" status.
                    self.status.clear();
                }
            }
            Err(error) => self.status = error,
        }
        cx.notify();
    }

    fn open_setting_link(&mut self, link: SettingLink, cx: &mut Context<Self>) {
        match link {
            SettingLink::CustomDirs => {
                self.settings_open = false;
                self.dirs_open = true;
                self.load_dirs(cx);
            }
            SettingLink::TextActions => {
                self.host.open_settings_page("textAction");
                self.dismiss(cx);
            }
        }
        cx.notify();
    }

    /// Pointer position when the menu opened, in overlay DIPs.
    fn overlay_anchor(&self) -> (f32, f32) {
        let scale_x = self.state["hostDpiScaleX"].as_f64().unwrap_or(1.0).max(0.1);
        let scale_y = self.state["hostDpiScaleY"].as_f64().unwrap_or(1.0).max(0.1);
        (
            ((self.state["anchorScreenX"].as_f64().unwrap_or(0.0)
                - self.state["overlayLeftPx"].as_f64().unwrap_or(0.0))
                / scale_x) as f32,
            ((self.state["anchorScreenY"].as_f64().unwrap_or(0.0)
                - self.state["overlayTopPx"].as_f64().unwrap_or(0.0))
                / scale_y) as f32,
        )
    }

    fn strip_rect(&self, size: (f32, f32)) -> Rect {
        category_strip_rect(self.overlay_anchor(), size)
    }

    /// Every category opens at the same point below the strip's left edge, so
    /// switching category only changes what extends to the right. Docked file
    /// panels share the root's top edge and fit the height left below it.
    fn menu_anchor(&self, size: (f32, f32)) -> (f32, f32) {
        let strip = self.strip_rect(size);
        (strip.x, strip.y + strip.height + STRIP_CONTENT_GAP)
    }

    /// Category tabs, a divider and the settings gear in one pill above the menu.
    fn category_strip(&self, strip: Rect, size: (f32, f32), cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let presentation = self.value_presentation;
        let wheel_key = self.session.clone();
        let icon_only = size.0 < STRIP_ICON_ONLY_BELOW;
        let hover = color(&t.hover);
        let mut pill = rsx! {
            <div id={"super-categories"} absolute flex items_center
                p={px(STRIP_PADDING)} rounded={px(11.0)} border_1 border_color={color(&t.border)}
                bg={color(&t.surface)} shadow_md />
        }
            .occlude()
            .left(px(strip.x))
            .top(px(strip.y))
            .w(px(strip.width))
            .h(px(strip.height))
            .gap(px(STRIP_TAB_GAP))
            .on_hover(cx.listener(|menu, hovered: &bool, _, _| {
                if !*hovered { menu.category_wheel.clear_pending(); }
            }))
            .on_scroll_wheel(cx.listener(move |menu, event: &gpui::ScrollWheelEvent, _, cx| {
                if !menu.render_current(&wheel_key, presentation) { return; }
                if event.modifiers.modified() {
                    menu.category_wheel.clear_pending();
                    return;
                }
                cx.stop_propagation();
                if menu.busy {
                    menu.category_wheel.clear_pending();
                    return;
                }
                if let Some(direction) = menu.category_wheel.step(event.delta, Instant::now()) {
                    menu.content_pointer();
                    menu.change_category(menu.category.adjacent(direction), cx);
                }
            }));
        for category in Category::ALL {
            let active = category == self.category;
            let mut tab = rsx! {
                <div flex flex_none items_center gap={px(STRIP_LABEL_GAP)} h={px(STRIP_TAB_HEIGHT)}
                    rounded={px(7.0)} cursor_pointer />
            }
                .id(SharedString::from(format!("super-cat-{}", category.action())))
                .px(px(if icon_only { STRIP_ICON_ONLY_TAB_PADDING } else { STRIP_TAB_PADDING }))
                .text_color(color(if active { &t.text } else { &t.text_secondary }))
                .when(active, |tab| tab.bg(color(&t.tab_active)))
                .when(!active, |tab| tab.hover(move |style| style.bg(hover)))
                .on_mouse_down(MouseButton::Left, cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                }))
                .on_click(cx.listener(move |menu, _, _, cx| {
                    menu.category_wheel.clear_pending();
                    menu.change_category(category, cx);
                }))
                // Right-click a category to jump straight to its settings.
                .on_mouse_down(MouseButton::Right, cx.listener(move |menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                    menu.category_wheel.clear_pending();
                    menu.open_settings();
                    if menu.category != category {
                        menu.change_category(category, cx);
                    }
                    cx.notify();
                }))
                .children(self.icon(category.icon(), STRIP_ICON)
                    .map(|icon| if active { icon.text_color(color(&t.accent)) } else { icon }));
            tab = if icon_only {
                tab.tooltip(crate::settings::settings_help(category.label().to_string(), t.clone()))
            } else {
                tab.child(
                    rsx! {
                        <div text_size={px(STRIP_LABEL_SIZE)} font_weight={gpui::FontWeight::SEMIBOLD}>
                            {category.label()}
                        </div>
                    }.w(px(strip_label_width(category.label()))).flex_none().whitespace_nowrap(),
                )
            };
            pill = pill.child(tab);
        }
        pill.child(rsx! { <div flex_none w={px(1.0)} h={px(18.0)} mx={px(4.0)} bg={color(&t.border)} /> })
            .child(self.settings_gear(cx))
            .into_any_element()
    }

    fn settings_gear(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let hover = color(&t.hover);
        rsx! {
            <div id={"super-settings-gear"} flex flex_none items_center justify_center
                size={px(STRIP_GEAR)} rounded={px(7.0)} cursor_pointer />
        }
            .when(self.settings_open, |gear| gear.bg(color(&t.tab_active)))
            .when(!self.settings_open, |gear| gear.hover(move |style| style.bg(hover)))
            .children(self.icon("gear", 16.0)
                .map(|icon| if self.settings_open { icon.text_color(color(&t.accent)) } else { icon }))
            .tooltip(crate::settings::settings_help(
                tr("菜单设置（也可右键分类）", "Menu settings (or right-click a category)").to_string(), t.clone()))
            .on_mouse_down(MouseButton::Left, cx.listener(|menu, _, _, cx| {
                menu.content_pointer();
                cx.stop_propagation();
            }))
            .on_click(cx.listener(|menu, _, _, cx| {
                if menu.settings_open {
                    menu.settings_open = false;
                } else {
                    menu.open_settings();
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn settings_panel(&self, strip: Rect, size: (f32, f32), cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let width = SETTINGS_PANEL_WIDTH.min((size.0 - EDGE * 2.0).max(1.0));
        let (x, y) = settings_panel_origin(strip, size, width);
        let own = category_settings(self.category);
        let specs = if own.is_empty() || self.settings_general { GENERAL_SETTINGS } else { own };
        let mut head = rsx! {
            <div flex flex_none items_center gap={px(8.0)} h={px(42.0)} pl={px(10.0)} pr={px(8.0)}
                border_b_1 border_color={color(&t.border)} />
        };
        head = if own.is_empty() {
            head.child(rsx! { <div text_size={px(12.0)} font_weight={gpui::FontWeight::SEMIBOLD}>{tr("通用", "General")}</div> })
        } else {
            let mut tabs = rsx! { <div flex p={px(2.0)} rounded={px(7.0)} bg={color(&t.secondary)} /> };
            for (index, general) in [false, true].into_iter().enumerate() {
                let active = self.settings_general == general;
                let label = if general { tr("通用", "General").to_string() } else { self.category.label().to_string() };
                tabs = tabs.child(
                    rsx! {
                        <div id={("super-settings-tab", index)} flex items_center h={px(24.0)} px={px(12.0)} rounded={px(5.0)}
                            text_size={px(11.5)} cursor_pointer />
                    }
                        .when(active, |tab| tab.bg(color(&t.background)).font_weight(gpui::FontWeight::MEDIUM).shadow_sm())
                        .text_color(color(if active { &t.text } else { &t.text_secondary }))
                        .child(label)
                        .on_click(cx.listener(move |menu, _, _, cx| {
                            menu.settings_general = general;
                            cx.notify();
                        })),
                );
            }
            head.child(tabs)
        };
        let hover = color(&t.hover);
        head = head.child(div().flex_1()).child(
            rsx! {
                <div id={"super-settings-close"} flex items_center justify_center size={px(26.0)} rounded={px(5.0)}
                    cursor_pointer text_color={color(&t.text_muted)}>"×"</div>
            }
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(|menu, _, _, cx| {
                    menu.settings_open = false;
                    cx.notify();
                })),
        );
        let mut body = rsx! { <div id={"super-settings-body"} flex flex_col flex_1 min_h_0 p={px(4.0)} overflow_y_scroll /> };
        for (index, spec) in specs.iter().copied().enumerate() {
            if matches!(spec.control, SettingControl::Link(_)) && index > 0 {
                body = body.child(rsx! { <div h={px(1.0)} mx={px(8.0)} my={px(4.0)} bg={color(&t.border)} /> });
            }
            body = body.child(self.setting_row(index, spec, cx));
        }
        let page = match self.category {
            Category::File => "menu_file",
            Category::Text => "textAction",
            _ => "menu_global",
        };
        let accent = color(&t.accent);
        let foot = rsx! {
            <div flex flex_none items_center gap={px(8.0)} h={px(34.0)} pl={px(12.0)} pr={px(8.0)}
                border_t_1 border_color={color(&t.border)} bg={color(&t.surface)}
                text_size={px(10.5)} text_color={color(&t.text_muted)}>
                {tr("修改立即生效", "Changes apply immediately")}
            </div>
        }
            .child(div().flex_1())
            .child(
                rsx! {
                    <div id={"super-settings-all"} flex items_center h={px(24.0)} px={px(8.0)} rounded={px(5.0)}
                        text_size={px(11.0)} cursor_pointer>{tr("全部设置…", "All settings…")}</div>
                }
                    .text_color(accent)
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(move |menu, _, _, cx| {
                        menu.host.open_settings_page(page);
                        menu.dismiss(cx);
                    })),
            );
        let panel = rsx! {
            <div id={"super-settings-panel"} absolute flex flex_col overflow_hidden rounded={px(10.0)} border_1
                border_color={color(&t.border)} bg={color(&t.background)} shadow_lg />
        }
            .occlude()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .max_h(px((size.1 - y - EDGE).max(120.0)))
            .on_mouse_down(MouseButton::Left, cx.listener(|menu, _, _, cx| {
                menu.content_pointer();
                cx.stop_propagation();
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(|menu, _, _, cx| {
                menu.content_pointer();
                cx.stop_propagation();
            }))
            .child(head)
            .child(body)
            .child(foot);
        if motion::reduced_motion() {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    "super-settings-in",
                    motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                    move |panel, t| panel.opacity(t).top(px(y - 4.0 * (1.0 - t))),
                )
                .into_any_element()
        }
    }

    fn setting_row(&self, index: usize, spec: SettingSpec, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let hover = color(&t.hover);
        let value = self.setting_value(&spec);
        let disabled = spec.requires.is_some_and(|key| {
            self.settings_values.get(&(spec.config, key.to_string())).and_then(Value::as_bool) == Some(false)
        });
        let mut label = rsx! {
            <div flex flex_col flex_1 min_w_0 gap={px(1.0)} text_size={px(12.0)} line_height={px(17.0)}>
                {tr(spec.label.0, spec.label.1)}
            </div>
        };
        if let Some(hint) = spec.hint {
            label = label.child(rsx! {
                <div text_size={px(10.5)} line_height={px(14.0)} text_color={color(&t.text_muted)}>{tr(hint.0, hint.1)}</div>
            });
        }
        if disabled {
            label = label.opacity(0.5);
        }
        let row = rsx! {
            <div id={("super-setting-row", index)} flex flex_none items_center gap={px(10.0)} min_h={px(40.0)} px={px(8.0)} py={px(6.0)}
                rounded={px(6.0)} />
        };
        match spec.control {
            SettingControl::Switch => {
                let on = value.as_bool() == Some(true);
                let mut track = rsx! {
                    <div id={("super-setting-switch", index)} relative flex_none w={px(34.0)} h={px(20.0)} rounded={px(10.0)} border_1 />
                }
                    .border_color(color(if on { &t.accent } else { &t.border }))
                    .bg(color(if on { &t.accent } else { &t.tertiary }))
                    .child(rsx! {
                        <div absolute top={px(2.0)} left={px(if on { 16.0 } else { 2.0 })} size={px(14.0)} rounded_full shadow_sm />
                    }.bg(if on { gpui::white() } else { color(&t.background) }));
                #[cfg(all(windows, debug_assertions))]
                {
                    let bounds = self.settings_switch_bounds.clone();
                    track = track.child(canvas(
                        move |rect, _, _| { bounds.borrow_mut().insert(index, rect); },
                        |_, _, _, _| {},
                    ).absolute().size_full());
                }
                if disabled {
                    track = track.opacity(0.45);
                } else {
                    track = track.cursor_pointer().on_click(cx.listener(move |menu, _, _, cx| {
                        menu.set_menu_setting(spec, Value::Bool(!on), cx);
                    }));
                }
                row.child(label).child(track).into_any_element()
            }
            SettingControl::Stepper { min, max, step } => {
                let current = value.as_f64().unwrap_or(min);
                let mut stepper = rsx! {
                    <div flex flex_none items_center h={px(26.0)} rounded={px(6.0)} border_1 border_color={color(&t.border)} overflow_hidden />
                };
                for (offset, glyph) in [(-step, "−"), (step, "+")] {
                    let next = (current + offset).clamp(min, max);
                    let enabled = next != current;
                    let mut button = rsx! {
                        <div id={("super-setting-step", index * 2 + usize::from(offset > 0.0))} flex items_center justify_center
                            w={px(26.0)} h_full text_size={px(14.0)} text_color={color(&t.text_secondary)}>{glyph}</div>
                    };
                    button = if enabled {
                        button.cursor_pointer().hover(move |style| style.bg(hover)).on_click(cx.listener(move |menu, _, _, cx| {
                            menu.set_menu_setting(spec, json!(next), cx);
                        }))
                    } else {
                        button.opacity(0.35)
                    };
                    if offset > 0.0 {
                        stepper = stepper.child(rsx! {
                            <div min_w={px(40.0)} px={px(4.0)} h_full flex items_center justify_center border_l_1 border_r_1
                                border_color={color(&t.border)} text_size={px(12.0)}>{format!("{current:.0}")}</div>
                        });
                    }
                    stepper = stepper.child(button);
                }
                row.child(label).child(stepper).into_any_element()
            }
            SettingControl::Choice(options) => {
                let current = value.as_str().unwrap_or("").to_string();
                let mut segmented = rsx! { <div flex p={px(2.0)} rounded={px(7.0)} bg={color(&t.secondary)} /> };
                for (option_index, (stored, zh, en)) in options.iter().copied().enumerate() {
                    let active = current.eq_ignore_ascii_case(stored) || (current.is_empty() && option_index == 0);
                    segmented = segmented.child(
                        rsx! {
                            <div id={("super-setting-choice", index * 16 + option_index)} flex flex_1 min_w_0 items_center justify_center
                                h={px(24.0)} px={px(4.0)} rounded={px(5.0)} text_size={px(11.0)} overflow_hidden whitespace_nowrap
                                cursor_pointer>{tr(zh, en)}</div>
                        }
                            .text_color(color(if active { &t.text } else { &t.text_secondary }))
                            .when(active, |chip| chip.bg(color(&t.background)).font_weight(gpui::FontWeight::MEDIUM).shadow_sm())
                            .on_click(cx.listener(move |menu, _, _, cx| {
                                if !active {
                                    menu.set_menu_setting(spec, json!(stored), cx);
                                }
                            })),
                    );
                }
                row.flex_col().items_start().gap(px(6.0)).child(label.w_full()).child(segmented.w_full()).into_any_element()
            }
            SettingControl::Link(link) => {
                let summary = match link {
                    SettingLink::CustomDirs => {
                        let count = value.as_str().unwrap_or("").lines()
                            .filter(|line| !line.trim().is_empty() && line.trim() != "---")
                            .count();
                        format!("{count} {}", tr("个", "folders"))
                    }
                    SettingLink::TextActions => String::new(),
                };
                row.cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(label)
                    .child(rsx! {
                        <div flex_none text_size={px(11.0)} text_color={color(&t.text_muted)}>{summary}</div>
                    })
                    .children(self.icon("chevron-right", 12.0))
                    .on_click(cx.listener(move |menu, _, _, cx| menu.open_setting_link(link, cx)))
                    .into_any_element()
            }
        }
    }

    fn keycap(&self, label: &'static str, on_accent: bool) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        let (fg, bg, border) = if on_accent {
            (gpui::white(), gpui::white().opacity(0.16), gpui::white().opacity(0.35))
        } else {
            (color(&t.text_secondary), color(&t.background), color(&t.border))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .min_w(px(18.0))
            .h(px(17.0))
            .px(px(4.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(px(9.5))
            .child(label)
    }

    fn clip_tile(&self, kind: &str) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        let (icon, _) = clip_kind_view(kind);
        let (tint, fill) = match kind {
            "LinkClipItem" => (color(&t.accent), color(&t.accent).opacity(0.15)),
            "FileClipItem" => (color(&t.warning), color(&t.warning).opacity(0.16)),
            "HtmlClipItem" | "QQRichTextItem" => (color("#a77bf0"), color("#a77bf0").opacity(0.15)),
            _ => (color(&t.text_secondary), color(&t.tertiary)),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .rounded(px(7.0))
            .bg(fill)
            .children(self.tinted_icon(icon, 14.0, tint))
    }

    /// One history row. Images lead with a thumbnail, files with their names,
    /// links with their domain and text with up to two lines of content; raw
    /// image file names are never the row title.
    fn clip_row(&self, index: usize, item: &Value, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let muted = color(&t.text_muted);
        let hover = color(&t.hover);
        let row_view = clip_row_view(item);
        let thumb = clip_thumb_source(item)
            .map(|_| self.clip_thumbs.get(&text(item, "id")).cloned().flatten());
        let mut title = row_view.title;
        if let (Some(Some(thumb)), "ImageClipItem") = (&thumb, item["kind"].as_str().unwrap_or("")) {
            title = format!("{title} · {} × {}", thumb.width, thumb.height);
        }
        let leading = match &thumb {
            Some(loaded) => div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(HUB_THUMB_WIDTH))
                .h(px(HUB_THUMB_HEIGHT))
                .rounded(px(6.0))
                .overflow_hidden()
                .bg(color(&t.tertiary))
                .map(|frame| match loaded {
                    Some(loaded) => frame.child(
                        img(loaded.image.clone())
                            .max_w(px(HUB_THUMB_WIDTH))
                            .max_h(px(HUB_THUMB_HEIGHT))
                            .object_fit(gpui::ObjectFit::Contain),
                    ),
                    None => frame.children(self.tinted_icon("image", 18.0, muted)),
                }),
            None => self.clip_tile(item["kind"].as_str().unwrap_or("")),
        };
        let mut title_line = div()
            .text_size(px(12.5))
            .line_height(px(18.0))
            .overflow_hidden()
            .child(title);
        title_line = if row_view.two_lines && thumb.is_none() {
            title_line.line_clamp(2)
        } else {
            title_line.whitespace_nowrap().text_ellipsis()
        };
        let row = div()
            .id(("super-clip", index))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .min_h(px(if thumb.is_some() { HUB_THUMB_HEIGHT + 12.0 } else { 46.0 }))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .on_hover(cx.listener(move |menu, hovered, _, cx| {
                if *hovered && menu.selected != index {
                    menu.selected = index;
                    menu.load_clip_preview(cx);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |menu, _, _, cx| {
                menu.selected = index;
                menu.clip_action("paste", cx);
            }))
            .child(leading)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.0))
                    .child(title_line)
                    .child(
                        div()
                            .text_size(px(10.5))
                            .line_height(px(14.0))
                            .text_color(muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(row_view.meta),
                    ),
            )
            .children(
                (item["isTop"].as_bool() == Some(true))
                    .then(|| self.tinted_icon("pin-top", 12.0, color(&t.pin)))
                    .flatten(),
            );
        if index == self.selected {
            row.bg(color(&t.selected))
        } else {
            row.hover(move |style| style.bg(hover))
        }
    }

    /// Thumbnails only for rows near the viewport, a few per worker pass and
    /// chained until that window is covered; far rows are evicted again.
    fn request_clip_thumbs(&mut self, cx: &mut Context<Self>) {
        if self.clip_thumb_task.is_some() || self.clips.is_empty() {
            return;
        }
        let Some(key) = self.session.clone() else {
            return;
        };
        if !self.current(&key) || self.category != Category::Clipboard {
            return;
        }
        let top = self.clip_scroll.top_item().min(self.selected);
        let bottom = self.clip_scroll.bottom_item().max(self.selected);
        let window = top.saturating_sub(4)..(bottom + 12).min(self.clips.len());
        if self.clip_thumbs.len() > HUB_THUMB_RETAIN {
            let keep: std::collections::HashSet<String> = self.clips
                [top.saturating_sub(30)..(bottom + 30).min(self.clips.len())]
                .iter()
                .map(|item| text(item, "id"))
                .collect();
            self.clip_thumbs.retain(|id, _| keep.contains(id));
        }
        let wanted: Vec<(String, ClipThumbSource)> = self.clips[window]
            .iter()
            .filter_map(|item| {
                let id = text(item, "id");
                (!self.clip_thumbs.contains_key(&id))
                    .then(|| clip_thumb_source(item).map(|source| (id, source)))
                    .flatten()
            })
            .take(6)
            .collect();
        if wanted.is_empty() {
            return;
        }
        let host = self.host.clone();
        let presentation = self.value_presentation;
        self.clip_thumb_task = Some(cx.spawn(async move |this, cx| {
            let load_key = key.clone();
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    wanted
                        .into_iter()
                        .map(|(id, source)| {
                            let thumb = load_key
                                .is_current(&host)
                                .then(|| match &source {
                                    ClipThumbSource::Clip => host.image_bytes(&id).ok(),
                                    ClipThumbSource::Path(path) => host.local_image_bytes(path),
                                })
                                .flatten()
                                .and_then(|(bytes, _)| hub_thumbnail(bytes));
                            (id, thumb)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) || menu.value_presentation != presentation {
                    return;
                }
                menu.clip_thumb_task = None;
                menu.clip_thumbs.extend(loaded);
                menu.request_clip_thumbs(cx);
                cx.notify();
            });
        }));
    }

    fn clipboard_hub(&self, rect: Rect, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let muted = color(&t.text_muted);
        let hover = color(&t.hover);
        let list_width = (rect.width * 0.4).clamp(220.0, 300.0);
        let hub = div()
            .id("super-clipboard-hub")
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .flex()
            .flex_row()
            .overflow_hidden()
            .rounded(px(10.0))
            .border_1()
            .border_color(color(&t.border))
            .bg(color(&t.background))
            .shadow_lg()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                    // A click on the hub itself closes the file submenus beside it.
                    if menu.columns.len() > 2 {
                        menu.columns.truncate(2);
                        menu.column_tasks.retain(|index, _| *index <= 1);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                }),
            );

        let mut rows = div()
            .id("super-clipboard-list")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .px(px(6.0))
            .pb(px(6.0))
            .overflow_y_scroll()
            .track_scroll(&self.clip_scroll)
            .on_scroll_wheel(cx.listener(|menu, event: &gpui::ScrollWheelEvent, _, cx| {
                if event.delta.pixel_delta(px(20.0)).y < px(0.0) {
                    let bottom = menu.clip_scroll.bottom_item();
                    menu.load_clips_near(bottom, cx);
                }
                menu.request_clip_thumbs(cx);
            }));
        if self.load_task.is_some() && self.clips.is_empty() {
            rows = rows.child(
                div()
                    .p(px(16.0))
                    .text_color(muted)
                    .child(tr("正在读取剪贴板历史…", "Loading clipboard history…")),
            );
        } else if self.clips.is_empty() {
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.0))
                    .py(px(40.0))
                    .text_size(px(11.0))
                    .text_color(muted)
                    .children(self.icon("clipboard", 28.0))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(color(&t.text_secondary))
                            .child(tr("暂无剪贴板条目", "Clipboard is empty")),
                    )
                    .child(tr(
                        "复制文字、图片或文件后会出现在这里",
                        "Copied text, images and files appear here",
                    )),
            );
        }
        for (index, item) in self.clips.iter().enumerate() {
            rows = rows.child(self.clip_row(index, item, cx));
        }
        if self.clips_more {
            rows = rows.child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(10.5))
                    .text_color(muted)
                    .child(if self.clips_more_task.is_some() {
                        tr("加载中…", "Loading…")
                    } else {
                        tr("向下滚动加载更多", "Scroll down to load more")
                    }),
            );
        }
        let total = self.clips_total.max(self.clips.len());
        let count = if self.clips.is_empty() {
            String::new()
        } else if cliphost_core::i18n::is_english() {
            total.to_string()
        } else {
            format!("{total} 条")
        };
        let list = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(list_width))
            .h_full()
            .min_h_0()
            .border_r_1()
            .border_color(color(&t.border))
            .bg(color(&t.surface))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .h(px(34.0))
                    .px(px(14.0))
                    .text_size(px(11.0))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color(&t.text_secondary))
                            .child(tr("最近复制", "Recent")),
                    )
                    .child(div().text_color(muted).child(count)),
            )
            .child(rows)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.0))
                    .h(px(30.0))
                    .px(px(12.0))
                    .border_t_1()
                    .border_color(color(&t.border))
                    .text_size(px(10.0))
                    .text_color(muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(self.keycap("↑", false))
                    .child(self.keycap("↓", false))
                    .child(tr("选择", "Select"))
                    .child(self.keycap("Enter", false).ml(px(6.0)))
                    .child(tr("粘贴", "Paste"))
                    .child(self.keycap("Esc", false).ml(px(6.0)))
                    .child(tr("关闭", "Close")),
            );

        let mut detail = div().flex().flex_col().flex_1().min_w_0().h_full().min_h_0();
        if let Some(item) = self.clips.get(self.selected) {
            let kind = item["kind"].as_str().unwrap_or("");
            let id = text(item, "id");
            let (kind_icon, kind_label) = clip_kind_view(kind);
            let (source, time, size) = clip_meta(item);
            let mut meta = div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(11.0))
                .text_color(muted)
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(4.0))
                        .h(px(20.0))
                        .px(px(7.0))
                        .rounded(px(10.0))
                        .bg(color(&t.tertiary))
                        .text_color(color(&t.text))
                        .children(self.tinted_icon(kind_icon, 12.0, color(&t.text_secondary)))
                        .child(kind_label),
                );
            for part in [source, time, size].into_iter().filter(|part| !part.is_empty()) {
                meta = meta.child(part);
            }
            let mut head = div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .h(px(40.0))
                .px(px(16.0))
                .border_b_1()
                .border_color(color(&t.border))
                .child(meta);
            if item["isTop"].as_bool() == Some(true) {
                head = head.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(3.0))
                        .text_size(px(10.5))
                        .text_color(color(&t.pin))
                        .children(self.tinted_icon("pin-top", 12.0, color(&t.pin)))
                        .child(tr("置顶", "Pinned")),
                );
            }
            let mut preview = div()
                .id("super-clipboard-preview")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .px(px(16.0))
                .py(px(14.0))
                .overflow_y_scroll()
                .text_size(px(13.0))
                .line_height(px(21.0));
            if kind == "FileClipItem" {
                preview = preview.gap(px(1.0)).px(px(8.0)).py(px(8.0));
                let files = self
                    .columns
                    .get(1)
                    .filter(|_| self.clip_files.as_deref() == Some(id.as_str()));
                match files {
                    Some(col) if !col.items.is_empty() => {
                        for (row, node) in col.items.iter().enumerate() {
                            // Nominal fallback until the row has been painted.
                            let y = rect.y + 48.0 + row as f32 * 28.0;
                            preview = preview.child(self.node_row(1, row, node, y, cx));
                        }
                    }
                    Some(col) if !col.loading => {
                        preview = preview.child(div().px(px(9.0)).text_color(muted).child(
                            if col.message.is_empty() {
                                tr("没有可用的文件", "No files available").to_string()
                            } else {
                                col.message.clone()
                            },
                        ));
                    }
                    _ => {
                        for path in array(item, "filePaths") {
                            preview = preview.child(
                                div()
                                    .px(px(9.0))
                                    .py(px(4.0))
                                    .text_size(px(12.0))
                                    .text_color(color(&t.text_secondary))
                                    .child(path.as_str().unwrap_or("").to_string()),
                            );
                        }
                    }
                }
                preview = preview.child(
                    div()
                        .mt(px(8.0))
                        .px(px(9.0))
                        .text_size(px(10.5))
                        .text_color(muted)
                        .child(tr(
                            "单击打开 · 右键更多文件操作 · 箭头展开文件夹",
                            "Click to open · Right-click for file actions · Arrow expands folders",
                        )),
                );
            } else if let Some(loaded) = &self.preview {
                let image_height = (rect.height - 130.0).max(80.0);
                if let Some(image) = &loaded.image {
                    preview = preview.items_center().child(
                        img(image.clone())
                            .w_full()
                            .max_h(px(image_height))
                            .rounded(px(6.0))
                            .object_fit(gpui::ObjectFit::Contain),
                    );
                } else if !loaded.parts.is_empty() {
                    for (value, image) in &loaded.parts {
                        if let Some(image) = image {
                            preview = preview.child(
                                img(image.clone())
                                    .w_full()
                                    .max_h(px(image_height))
                                    .object_fit(gpui::ObjectFit::Contain),
                            );
                        }
                        if !value.is_empty() {
                            preview = preview.child(value.clone());
                        }
                    }
                } else if kind == "LinkClipItem" {
                    preview = preview.child(
                        div()
                            .p(px(12.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(color(&t.border))
                            .bg(color(&t.surface))
                            .text_size(px(12.0))
                            .text_color(color(&t.accent))
                            .child(loaded.text.clone()),
                    );
                } else {
                    preview = preview.child(loaded.text.clone());
                }
            } else {
                preview = preview.text_color(muted).child(tr("加载中…", "Loading…"));
            }

            let disabled = self.busy || self.clips.is_empty();
            let accent = color(&t.accent);
            let mut actions = div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.0))
                .h(px(48.0))
                .px(px(12.0))
                .border_t_1()
                .border_color(color(&t.border))
                .bg(color(&t.surface))
                .opacity(if disabled { 0.5 } else { 1.0 })
                .child(
                    div()
                        .id("super-clip-paste")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.0))
                        .h(px(28.0))
                        .pl(px(10.0))
                        .pr(px(6.0))
                        .rounded(px(6.0))
                        .bg(accent)
                        .text_color(gpui::white())
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(move |style| style.bg(accent.opacity(0.88)))
                        .children(self.tinted_icon("paste", 14.0, gpui::white()))
                        .child(tr("粘贴", "Paste"))
                        .child(self.keycap("Enter", true))
                        .on_click(cx.listener(|menu, _, _, cx| menu.clip_action("paste", cx))),
                );
            let mut entries = vec![("copy", "copy", tr("复制", "Copy"))];
            if !matches!(kind, "FileClipItem" | "ImageClipItem")
                && !row_text(item).trim().is_empty()
                && row_text(item).trim() != "…"
            {
                entries.push(("simulate", "keyboard", tr("模拟输入", "Simulate typing")));
            }
            if self.pin_provider.is_some() && can_pin(item) {
                entries.push(("pin", "pin", tr("贴图", "Pin image")));
            }
            if open_target(item).is_some() {
                entries.push(("open", "open-panel", tr("打开", "Open")));
            }
            for (action, icon, label) in entries {
                actions = actions.child(
                    div()
                        .id(SharedString::from(format!("super-clip-{action}")))
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.0))
                        .h(px(28.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .text_size(px(12.0))
                        .text_color(color(&t.text_secondary))
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .children(self.tinted_icon(icon, 14.0, color(&t.text_secondary)))
                        .child(label)
                        .on_click(cx.listener(move |menu, _, _, cx| menu.clip_action(action, cx))),
                );
            }
            let danger = color(&t.error);
            actions = actions.child(div().flex_1()).child(
                div()
                    .id("super-clip-delete")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(move |style| style.bg(danger.opacity(0.12)))
                    .tooltip(crate::settings::settings_help(tr("删除", "Delete").to_string(), t.clone()))
                    .children(self.tinted_icon("delete", 14.0, muted))
                    .on_click(cx.listener(|menu, _, _, cx| menu.clip_action("delete", cx))),
            );
            detail = detail.child(head).child(preview).child(actions);
        } else {
            detail = detail
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(if self.load_task.is_some() {
                    ""
                } else {
                    tr("没有可预览的条目", "Nothing to preview")
                });
        }
        hub.child(list).child(detail).into_any_element()
    }

    fn dirs_panel(&self, size: (f32, f32), cx: &mut Context<Self>) -> AnyElement {
        let rect = clamp_rect(
            Rect {
                x: size.0 * 0.5 - 220.0,
                y: 120.0,
                width: 440.0,
                height: size.1 - 160.0,
            },
            size,
        );
        let mut panel = self
            .panel("super-custom-dirs")
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .max_h(px(rect.height))
            .p(px(12.0))
            .gap(px(8.0))
            .overflow_y_scroll()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|menu, _, _, cx| {
                    menu.content_pointer();
                    cx.stop_propagation();
                }),
            )
            .child(
                rsx! {
                    <div flex justify_between>
                        {tr("自定义目录", "Custom directories")}
                    </div>
                }
                    .child(self.button("super-dirs-close", "×").on_click(cx.listener(
                        |menu, _, _, cx| {
                            menu.dirs_open = false;
                            menu.dirs_task = None;
                            cx.notify();
                        },
                    ))),
            )
            .child(
                rsx! {
                    <div text_size={px(11.0)}
                        text_color={color(&self.tokens.theme(self.dark).text_muted)}>
                        {tr(
                        "与收藏无关 · 出现在文件菜单根部",
                        "Independent of Favorites · Shown at the file menu root",
                    )}
                    </div>
                },
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(5.0))
                    .child(
                        self.button("super-dirs-pick", tr("选择文件夹…", "Choose folder…"))
                            .on_click(cx.listener(|menu, _, _, cx| {
                                menu.mutate_dirs("pick", 0, String::new(), cx)
                            })),
                    )
                    .child(
                        self.button("super-dirs-separator", tr("插入分割线", "Insert separator"))
                            .on_click(cx.listener(|menu, _, _, cx| {
                                menu.mutate_dirs("separator", 0, String::new(), cx)
                            })),
                    )
                    .child(
                        self.button("super-dirs-reload", tr("刷新", "Refresh"))
                            .on_click(cx.listener(|menu, _, _, cx| menu.load_dirs(cx))),
                    ),
            );
        for (index, path) in array(&self.dirs, "openedFolders")
            .iter()
            .filter_map(Value::as_str)
            .enumerate()
        {
            let path = path.to_string();
            let add = path.clone();
            let exists = array(&self.dirs, "lines")
                .iter()
                .filter_map(Value::as_str)
                .any(|line| {
                    line.trim_end_matches(['\\', '/'])
                        .eq_ignore_ascii_case(path.trim_end_matches(['\\', '/']))
                });
            panel = panel.child(
                rsx! {
                    <div flex items_center gap={px(5.0)}>
                        <div flex_1 min_w_0>
                            {path}
                        </div>
                    </div>
                }
                    .child(
                        self.button(("super-dirs-opened", index), if exists { "✓" } else { "+" })
                            .on_click(cx.listener(move |menu, _, _, cx| {
                                if !exists {
                                    menu.mutate_dirs("add", 0, add.clone(), cx);
                                }
                            })),
                    ),
            );
        }
        let lines = array(&self.dirs, "lines");
        panel = panel.child(
            div()
                .text_size(px(12.0))
                .child(tr("当前列表", "Current list")),
        );
        for (index, line) in lines.iter().filter_map(Value::as_str).enumerate() {
            panel =
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(div().flex_1().min_w_0().child(
                            if line.chars().all(|ch| ch == '-') {
                                tr("—— 分割线 ——", "—— Separator ——").to_string()
                            } else {
                                line.to_string()
                            },
                        ))
                        .child(
                            self.button(("super-dir-up", index), "↑")
                                .on_click(cx.listener(move |menu, _, _, cx| {
                                    menu.mutate_dirs("up", index, String::new(), cx)
                                })),
                        )
                        .child(
                            self.button(("super-dir-down", index), "↓")
                                .on_click(cx.listener(move |menu, _, _, cx| {
                                    menu.mutate_dirs("down", index, String::new(), cx)
                                })),
                        )
                        .child(self.button(("super-dir-remove", index), "×").on_click(
                            cx.listener(move |menu, _, _, cx| {
                                menu.mutate_dirs("remove", index, String::new(), cx)
                            }),
                        )),
                );
        }
        panel.into_any_element()
    }
}

impl Render for SuperMenuWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.column_rects.borrow_mut().clear();
        self.bind_native_keyboard();
        self.request_node_icons(cx);
        self.request_window_thumbs(cx);
        window.set_window_title(tr("ClipHost GPUI · 超级菜单", "ClipHost GPUI · Super menu"));
        let width = f32::from(window.viewport_size().width);
        let height = f32::from(window.viewport_size().height);
        let size = (width, height);
        let strip = self.strip_rect(size);
        let presentation = self.value_presentation;
        let render_key = self.session.clone();
        let left_key = render_key.clone();
        let right_key = render_key.clone();
        let move_key = render_key.clone();
        let up_key = render_key.clone();
        let out_key = render_key.clone();
        let key_down_key = render_key.clone();
        let key_up_key = render_key;
        let move_listener = cx.listener(move |menu, event: &gpui::MouseMoveEvent, _, cx| {
            if menu.render_current(&move_key, presentation) {
                menu.move_value_drag(event, cx);
            }
        });
        let cancel_key = self.session.clone();
        let cancel_pointer = cx.listener(move |menu, _: &gpui::MouseDownEvent, _, _| {
            if menu.render_current(&cancel_key, presentation) {
                menu.cancel_file_preview();
            }
        });
        let scroll_key = self.session.clone();
        let cancel_scroll = cx.listener(move |menu, _: &gpui::ScrollWheelEvent, _, _| {
            if menu.render_current(&scroll_key, presentation) { menu.cancel_file_preview(); }
        });
        let pointer_events = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &gpui::MouseDownEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Capture { cancel_pointer(event, window, cx); }
                });
                window.on_mouse_event(move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Capture { cancel_scroll(event, window, cx); }
                });
                // GPUI's div on_mouse_move is hitbox-filtered even though its
                // Windows backend holds native mouse capture. Listen during paint
                // so a drag keeps updating outside this root's visible rectangle.
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        move_listener(event, window, cx);
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let content_top = strip.y + strip.height + STRIP_CONTENT_GAP;
        let menu_anchor = self.menu_anchor(size);
        let mut root = div()
            .id("super-menu-overlay")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(gpui::transparent_black())
            .font_family(self.tokens.typography.font_family.clone())
            .text_color(color(&self.tokens.theme(self.dark).text))
            .text_size(px(13.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |menu, _, _, cx| {
                    if !menu.render_current(&left_key, presentation) {
                        return;
                    }
                    // Clicking away from the confirmation or the settings panel
                    // closes that layer, not the menu.
                    if menu.kill_confirm.take().is_some() {
                        cx.notify();
                    } else if menu.settings_open {
                        menu.settings_open = false;
                        cx.notify();
                    } else {
                        menu.dismiss(cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |menu, _, _, cx| {
                    if menu.render_current(&right_key, presentation) {
                        menu.dismiss(cx);
                    }
                }),
            )
            .child(pointer_events)
            .capture_key_down(cx.listener(|menu, event: &gpui::KeyDownEvent, window, cx| {
                if menu.settings_open && event.keystroke.key == "escape"
                    && !event.keystroke.modifiers.modified()
                    && !menu.browser_composing(cx)
                    && !menu.text_launcher.as_ref().is_some_and(|launcher| launcher.read(cx).has_composition(cx)) {
                    menu.settings_open = false; cx.notify(); cx.stop_propagation(); return;
                }
                if menu.category == Category::Text && menu.session.as_ref().is_some_and(|key| menu.current(key)) {
                    if let Some(launcher) = &menu.text_launcher {
                        if launcher.update(cx, |launcher, cx| launcher.handle_key(event, window, cx)) {
                            cx.stop_propagation();
                        }
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |menu, event: &gpui::MouseUpEvent, _, cx| {
                    if menu.render_current(&up_key, presentation)
                        && menu.finish_value_drag(Some(event.position), cx)
                    {
                        cx.stop_propagation();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |menu, event: &gpui::MouseUpEvent, _, cx| {
                    if menu.render_current(&out_key, presentation)
                        && menu.finish_value_drag(Some(event.position), cx)
                    {
                        cx.stop_propagation();
                    }
                }),
            )
            .on_key_down(cx.listener(move |menu, event: &gpui::KeyDownEvent, _, cx| {
                if menu.render_current(&key_down_key, presentation)
                    && menu.category != Category::Text
                    && !(menu.category == Category::File && menu.browsing())
                    && !event.keystroke.modifiers.modified()
                    && menu.handle_key(&event.keystroke.key, true, event.is_held, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(move |menu, event: &gpui::KeyUpEvent, _, cx| {
                if menu.render_current(&key_up_key, presentation)
                    && menu.category != Category::Text
                    && !(menu.category == Category::File && menu.browsing())
                    && !event.keystroke.modifiers.modified()
                    && menu.handle_key(&event.keystroke.key, false, false, cx)
                {
                    cx.stop_propagation();
                }
            }));
        if self.session.is_none() {
            return root;
        }
        root = root.child(self.category_strip(strip, size, cx));
        match self.category {
            Category::Text => {
                if let Some(launcher) = &self.text_launcher {
                    let panel_width = 560.0_f32.min((width - EDGE * 2.0).max(1.0));
                    let panel_height = 500.0_f32.min((height - content_top - EDGE).max(1.0));
                    let panel_x = strip.x.clamp(EDGE, (width - panel_width - EDGE).max(EDGE));
                    let panel_y = content_top;
                    root = root.child(rsx! {
                        <div id={"super-text-actions-panel"} absolute left={px(panel_x)} top={px(panel_y)}
                            w={px(panel_width)} h={px(panel_height)} min_w_0 min_h_0 overflow_hidden
                            rounded={px(14.0)} shadow_lg />
                    }.on_mouse_down(MouseButton::Left, cx.listener(|menu, _, _, cx| {
                        menu.content_pointer(); cx.stop_propagation();
                    })).on_mouse_down(MouseButton::Right, cx.listener(|menu, _, _, cx| {
                        menu.content_pointer(); cx.stop_propagation();
                    })).child(launcher.clone()));
                }
            }
            Category::Clipboard => {
                let hub = clipboard_hub_rect(size, strip.x, content_top);
                self.column_rects.borrow_mut().insert(1, hub);
                root = root
                    .child(self.clipboard_hub(hub, cx))
                    .children(self.tree_from(2, Some(hub), size, menu_anchor, cx));
            }
            _ => {
                let tree = self.tree(size, menu_anchor, cx);
                let root_rect = self.column_rects.borrow().get(&0).copied();
                if root_rect.is_some() && self.category == Category::File {
                    // Panels first: cascades from the root still paint above them.
                    root = root.children(self.file_hint(strip, size));
                    if self.browsing() {
                        root = root.child(self.browser_workspace(size, cx));
                    }
                }
                root = root.children(tree);
            }
        }
        if !self.status.is_empty() {
            root = root.child(
                self.panel("super-menu-status")
                    .absolute()
                    .left(px(strip.x))
                    .top(px((height - 100.0).max(EDGE)))
                    .max_w(px((width - EDGE * 2.0).max(1.0)))
                    .p(px(8.0))
                    .text_size(px(12.0))
                    .child(self.status.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|menu, _, _, cx| {
                            menu.content_pointer();
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        if self.dirs_open {
            root = root.child(self.dirs_panel(size, cx));
        }
        if self.settings_open {
            root = root.child(self.settings_panel(strip, size, cx));
        }
        if let Some(card) = self.kill_confirm_card(size, menu_anchor, cx) {
            root = root.child(card);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_or_reopened_sessions_have_distinct_keys() {
        let state = json!({ "visible": true, "sessionId": "same", "generation": 3 });
        let key = SessionKey::from_state(&state).unwrap();
        assert_eq!(key, SessionKey::from_state(&state).unwrap());
        assert_ne!(
            Some(key),
            SessionKey::from_state(
                &json!({ "visible": true, "sessionId": "same", "generation": 4 })
            )
        );
        assert!(SessionKey::from_state(
            &json!({ "visible": false, "sessionId": "same", "generation": 3 })
        )
        .is_none());
    }

    #[test]
    fn folder_primary_action_and_expansion_are_independent() {
        assert!(!primary_invokes(
            &json!({ "kind": "Folder", "hasChildren": true, "invokeMode": "None" })
        ));
        assert!(primary_invokes(
            &json!({ "kind": "Folder", "hasChildren": true, "payload": { "path": "C:\\中文\\目录" } })
        ));
        assert!(primary_invokes(
            &json!({ "kind": "Folder", "hasChildren": true, "invokeMode": 2 })
        ));
        assert!(!primary_invokes(
            &json!({ "kind": "Folder", "role": "file-copy-more", "hasChildren": true,
                "invokeMode": " Expand ", "payload": { "path": "C:\\中文\\目录" } })
        ));
        for mode in [json!("None"), json!(" none "), json!(0)] {
            for path_field in ["payload", "description"] {
                let mut node = json!({"kind":"Folder", "hasChildren":true, "invokeMode":mode});
                if path_field == "payload" {
                    node["payload"] = json!({"path":"C:\\fixture\\Recent"});
                } else {
                    node["description"] = json!("C:\\fixture\\first-opened-folder");
                }
                assert!(!primary_invokes(&node), "explicit expand-only root: {node}");
            }
        }
        assert!(!expandable(
            &json!({ "kind": "Value", "hasChildren": true })
        ));
        assert!(!enabled(&json!({ "kind": "Separator" })));
        assert!(!enabled(&json!({ "kind": "Command", "isEnabled": false })));
    }

    #[test]
    fn only_a_newly_created_column_gets_a_new_entry_and_a_refresh_keeps_it() {
        let a = Column::from_page("p".into(), "children", 0.0, &Value::Null);
        let b = Column::from_page("p".into(), "children", 0.0, &Value::Null);
        assert_ne!(a.entry, b.entry, "each new column appears separately");
        let reloaded = Column::from_page("p".into(), "children", 4.0, &json!({"items": []}))
            .refresh_of(&a);
        assert_eq!(reloaded.entry, a.entry, "a refresh must not replay");
        assert_eq!(a.clone().entry, a.entry);
    }

    #[test]
    fn node_icon_spec_keeps_real_icons_and_skips_kind_fallbacks() {
        assert_eq!(node_icon_spec(&json!({ "icon": r" icon:D:.txt " })), Some(r"icon:D:.txt"));
        assert_eq!(node_icon_spec(&json!({ "icon": "icon:." })), Some("icon:."));
        assert_eq!(node_icon_spec(&json!({ "icon": "fa:Light_Link" })), Some("fa:Light_Link"));
        assert_eq!(node_icon_spec(&json!({ "icon": "TEXT:abc" })), None);
        assert_eq!(node_icon_spec(&json!({ "icon": "  " })), None);
        assert_eq!(node_icon_spec(&json!({})), None);
    }

    #[test]
    fn category_strip_rides_above_the_pointer_inside_every_viewport() {
        // Mid-screen: the strip ends just above the pointer, so the menu's
        // first row still opens under it.
        let strip = category_strip_rect((400.0, 500.0), (1920.0, 1080.0));
        assert_eq!((strip.x, strip.y + strip.height + STRIP_CONTENT_GAP), (400.0, 500.0));
        assert_eq!(strip.height, STRIP_HEIGHT);
        for size in [(1920.0, 1080.0), (1280.0, 720.0), (800.0, 600.0), (440.0, 420.0)] {
            for anchor in [(0.0, 0.0), (size.0, size.1), (size.0 * 0.5, size.1 * 0.5), (-50.0, size.1 + 50.0)] {
                let strip = category_strip_rect(anchor, size);
                assert!(strip.x >= EDGE && strip.x + strip.width <= size.0 - EDGE + 0.01, "{anchor:?} {size:?}");
                assert!(strip.y >= EDGE && strip.y + strip.height <= size.1 - EDGE, "{anchor:?} {size:?}");
                // A pointer near the bottom lifts the strip to leave a usable menu.
                if size.1 >= EDGE * 2.0 + STRIP_HEIGHT + STRIP_CONTENT_GAP + STRIP_MIN_CONTENT {
                    assert!(size.1 - EDGE - (strip.y + strip.height + STRIP_CONTENT_GAP) >= STRIP_MIN_CONTENT - 0.01);
                }
            }
        }
        // Narrow overlays drop the labels; the labelled strip fits the CJK names.
        assert!(strip_width(true) < strip_width(false));
        assert!(strip_label_width("剪贴板") >= STRIP_LABEL_SIZE * 3.0);
    }

    #[test]
    fn category_wheel_notches_follow_visual_order_and_wrap() {
        let mut wheel = CategoryWheel::default();
        let now = Instant::now();
        let lines = |x, y| gpui::ScrollDelta::Lines(gpui::point(x, y));
        let mut category = Category::Window;
        for expected in [Category::File, Category::Favor, Category::Bookmarks, Category::Text, Category::Clipboard, Category::Window] {
            category = category.adjacent(wheel.step(lines(0.0, -3.0), now).unwrap());
            assert_eq!(category, expected);
        }
        assert_eq!(category.adjacent(wheel.step(lines(0.0, 3.0), now).unwrap()), Category::Clipboard);
        assert_eq!(wheel.step(lines(3.0, 0.1), now), None);
        assert_eq!(wheel.step(lines(0.0, f32::NAN), now), None);
    }

    #[test]
    fn category_wheel_accumulates_precision_and_discards_short_inertia() {
        let mut wheel = CategoryWheel::default();
        let start = Instant::now();
        let pixels = |y| gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(y)));
        let at = |ms| start + Duration::from_millis(ms);
        assert_eq!(wheel.step(pixels(-15.0), at(0)), None);
        assert_eq!(wheel.step(pixels(-24.0), at(10)), None);
        assert_eq!(wheel.step(pixels(-1.0), at(20)), Some(1));
        assert_eq!(wheel.step(pixels(-120.0), at(30)), None);
        assert_eq!(wheel.step(pixels(-40.0), at(141)), Some(1));
        assert_eq!(wheel.step(pixels(-25.0), at(300)), None);
        assert_eq!(wheel.step(pixels(20.0), at(310)), None);
        assert_eq!(wheel.step(pixels(20.0), at(320)), Some(-1));
        assert_eq!(wheel.step(pixels(-20.0), at(600)), None);
        assert_eq!(wheel.step(pixels(-21.0), at(801)), None);
        assert_eq!(wheel.step(pixels(-19.0), at(810)), Some(1));
        wheel.clear_pending();
        assert_eq!(wheel.step(pixels(-40.0), at(820)), None);
        wheel.reset();
        assert_eq!(wheel.step(pixels(-20.0), at(830)), None);
    }

    #[test]
    fn category_wheel_session_change_discards_gesture_but_keeps_rate_limit() {
        let mut wheel = CategoryWheel::default();
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let pixels = |y| gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(y)));
        assert_eq!(wheel.step(pixels(-40.0), at(0)), Some(1));
        assert_eq!(wheel.step(pixels(-20.0), at(120)), None);
        wheel.retire();
        assert_eq!(wheel.distance, 0.0);
        assert_eq!(wheel.direction, 0.0);
        assert_eq!(wheel.precise, None);
        assert_eq!(wheel.last_event, None);
        assert_eq!(wheel.last_step, Some(start));
        assert_eq!(wheel.step(pixels(-20.0), at(130)), None);
        assert_eq!(wheel.step(pixels(-20.0), at(140)), Some(1));
        wheel.retire();
        assert_eq!(wheel.step(pixels(-40.0), at(259)), None);
        assert_eq!(wheel.step(pixels(-40.0), at(260)), Some(1));
    }

    #[test]
    fn category_wheel_does_not_mix_line_and_pixel_remainders() {
        let mut wheel = CategoryWheel::default();
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let lines = |y| gpui::ScrollDelta::Lines(gpui::point(0.0, y));
        let pixels = |y| gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(y)));
        assert_eq!(wheel.step(lines(-0.75), at(0)), None);
        assert_eq!(wheel.step(pixels(-39.0), at(10)), None);
        assert_eq!(wheel.step(pixels(-1.0), at(20)), Some(1));
        assert_eq!(wheel.step(lines(-0.75), at(141)), None);
        assert_eq!(wheel.step(lines(-0.25), at(150)), Some(1));
    }

    #[test]
    fn cascading_columns_reverse_and_keep_inside_monitor() {
        let previous = Rect {
            x: 680.0,
            y: 10.0,
            width: 280.0,
            height: 100.0,
        };
        let (rect, side) = column_rect(
            Some(previous),
            CascadeSide::Right,
            (0.0, 0.0),
            730.0,
            280.0,
            1000.0,
            (1000.0, 800.0),
        );
        assert_eq!(rect.x, 394.0);
        assert_eq!(side, CascadeSide::Left);
        assert_eq!(rect.height, 776.0);
        assert_eq!(rect.y, 12.0);
        let small = clamp_rect(
            Rect {
                x: -500.0,
                y: 999.0,
                width: 500.0,
                height: 500.0,
            },
            (200.0, 180.0),
        );
        assert_eq!(
            small,
            Rect {
                x: 12.0,
                y: 12.0,
                width: 176.0,
                height: 156.0
            }
        );
    }

    #[test]
    fn leftward_cascade_keeps_growing_left_instead_of_covering_its_root() {
        let root = Rect { x: 680.0, y: 20.0, width: 280.0, height: 100.0 };
        let (child, side) = column_rect(Some(root), CascadeSide::Right,
            (0.0, 0.0), 20.0, 280.0, 100.0, (1000.0, 800.0));
        let (grandchild, side) = column_rect(Some(child), side,
            (0.0, 0.0), 20.0, 280.0, 100.0, (1000.0, 800.0));
        assert_eq!(side, CascadeSide::Left);
        assert_eq!(grandchild.x, 108.0);
        assert!(grandchild.x + grandchild.width < child.x);
        assert!(child.x + child.width < root.x);
    }

    #[test]
    fn cascade_turns_again_only_when_its_current_side_is_exhausted() {
        let parent = Rect { x: 22.0, y: 20.0, width: 280.0, height: 100.0 };
        let (child, side) = column_rect(Some(parent), CascadeSide::Left,
            (0.0, 0.0), 20.0, 280.0, 100.0, (600.0, 420.0));
        assert_eq!(side, CascadeSide::Right);
        assert_eq!(child.x, 308.0);
        assert!(child.x >= parent.x + parent.width + GAP);
    }

    #[test]
    fn unavoidable_overlap_uses_the_roomier_side_and_stays_on_screen() {
        for (parent_x, preferred, expected_x, expected_side) in [
            (12.0, CascadeSide::Right, 148.0, CascadeSide::Right),
            (148.0, CascadeSide::Right, 12.0, CascadeSide::Left),
            (80.0, CascadeSide::Left, 12.0, CascadeSide::Left),
        ] {
            let parent = Rect { x: parent_x, y: 20.0, width: 280.0, height: 100.0 };
            let (child, side) = column_rect(Some(parent), preferred,
                (0.0, 0.0), 20.0, 280.0, 100.0, (440.0, 420.0));
            assert_eq!(child.x, expected_x);
            assert_eq!(side, expected_side);
            assert!(child.x >= EDGE && child.x + child.width <= 440.0 - EDGE);
            assert!(child.x < parent.x + parent.width && child.x + child.width > parent.x);
        }
    }

    #[test]
    fn numeric_and_text_action_names_match_the_existing_hub() {
        assert_eq!(Category::from_action(&json!(0)), Category::Window);
        assert_eq!(Category::from_action(&json!("3")), Category::File);
        assert_eq!(Category::from_action(&json!(" FILE ")), Category::File);
        assert_eq!(Category::from_action(&json!("super")), Category::Favor);
        assert_eq!(
            Category::from_action(&json!("clipboard")),
            Category::Clipboard
        );
    }

    #[test]
    fn window_menu_click_invokes_and_management_roots_remain_expandable() {
        let window = json!({"id":"existing-window", "role":"window", "kind":"Folder",
            "hasChildren":true, "invokeMode":"InvokeAndExpand", "payload":{"windowHandle":42}});
        assert_eq!(node_collection(false), "children");
        assert_eq!(node_collection(true), "context-menu");
        assert!(primary_invokes(&window));
        assert!(expandable(&window));
        let group = json!({"role":"window-group", "kind":"Folder", "invokeMode":"InvokeAndExpand",
            "hasChildren":true, "payload":{"windowHandle":42}});
        assert!(primary_invokes(&group));
        assert!(expandable(&group));
        // Favorites also embed a "window" node that is a management root,
        // not an individual window. Its HWND must not hide its window list.
        let root = json!({"role":"window", "kind":"Folder", "hasChildren":true,
            "invokeMode":"None", "payload":{"windowHandle":42,"root":true}});
        assert!(expandable(&root));
        assert!(!primary_invokes(&root));
    }

    #[test]
    fn trailing_setting_rows_and_their_separators_are_dropped() {
        let items = vec![
            json!({"kind":"Command", "role":"setting"}),
            json!({"kind":"Separator"}),
            json!({"kind":"Folder", "role":"window", "id":"a"}),
            json!({"kind":"Separator"}),
            json!({"kind":"Separator"}),
            json!({"kind":"Folder", "role":"window", "id":"b"}),
            json!({"kind":"Separator"}),
            json!({"kind":"Command", "role":"setting"}),
        ];
        let ids: Vec<_> = strip_setting_rows(items).iter().map(|item| text(item, "kind")).collect();
        assert_eq!(ids, ["Folder", "Separator", "Folder"]);
    }

    #[test]
    fn settings_fly_out_beside_the_strip_without_covering_the_menu() {
        for size in [(1920.0, 1080.0), (800.0, 600.0), (440.0, 420.0)] {
            for anchor in [(EDGE, 300.0), (size.0 * 0.5, 300.0), (size.0, 300.0)] {
                let strip = category_strip_rect(anchor, size);
                let width = SETTINGS_PANEL_WIDTH.min(size.0 - EDGE * 2.0);
                let (x, y) = settings_panel_origin(strip, size, width);
                assert!(x >= EDGE && x + width <= size.0 - EDGE + 0.01, "{anchor:?} {size:?}");
                let beside = y <= strip.y;
                if beside {
                    // Beside the strip: never overlapping it horizontally.
                    assert!(x >= strip.x + strip.width || x + width <= strip.x, "{anchor:?} {size:?}");
                } else {
                    assert_eq!(y, strip.y + strip.height + SETTINGS_PANEL_GAP);
                }
            }
        }
        let wide = category_strip_rect((EDGE, 300.0), (1920.0, 1080.0));
        let gear = strip_gear_rect(wide);
        assert!(gear.x > wide.x && gear.x + gear.width < wide.x + wide.width && gear.y > wide.y);
        assert_eq!(settings_panel_origin(wide, (1920.0, 1080.0), SETTINGS_PANEL_WIDTH),
            (wide.x + wide.width + SETTINGS_PANEL_GAP, wide.y));
        // A low pointer: the flyout rises beside the strip to keep its height.
        let low = category_strip_rect((EDGE, 1060.0), (1920.0, 1080.0));
        let (_, y) = settings_panel_origin(low, (1920.0, 1080.0), SETTINGS_PANEL_WIDTH);
        assert!(y < low.y && 1080.0 - EDGE - y >= SETTINGS_PANEL_PREFERRED_HEIGHT);
        // Every in-menu setting maps to an existing settings form field.
        let global: Vec<_> = cliphost_core::extra_settings::menu_global_fields(&Default::default())
            .iter().map(|field| field["key"].as_str().unwrap().to_string()).collect();
        let file: Vec<_> = cliphost_core::extra_settings::file_menu_fields(&Default::default())
            .iter().map(|field| field["key"].as_str().unwrap().to_string()).collect();
        for category in Category::ALL {
            for spec in category_settings(category).iter().chain(GENERAL_SETTINGS) {
                match spec.config {
                    "menu_global" => assert!(global.contains(&spec.key.to_string()), "{}", spec.key),
                    "menu_file" => assert!(file.contains(&spec.key.to_string()), "{}", spec.key),
                    _ => assert!(matches!(spec.control, SettingControl::Link(_))),
                }
            }
        }
    }

    #[test]
    fn window_rows_show_state_as_chips_and_kills_need_confirmation() {
        let visual = json!({"title":"a.txt - 记事本", "exe":"Notepad.exe", "pid":42, "foreground":true,
            "minimized":true, "opacity":85, "penetrate":true, "topmost":true});
        let labels: Vec<_> = window_chips(&visual).into_iter().map(|(label, _)| label).collect();
        assert_eq!(labels.len(), 4, "foreground, minimized, opacity and click-through: {labels:?}");
        assert!(labels.contains(&"85%".to_string()));
        assert!(window_chips(&json!({"opacity":100})).is_empty());

        let row = json!({"role":"window", "visual":{"window":visual}});
        let kill = json!({"role":"window-kill", "title":"结束进程"});
        assert!(window_kill_role(&kill) && !window_kill_role(&json!({"role":"window-close"})));
        assert!(kill_confirm_for(&kill, Some(&row)).title.contains("Notepad.exe（PID 42）"));
        let restart = kill_confirm_for(&json!({"role":"window-kill-all", "title":"重启explorer"}), None);
        assert_eq!(restart.title, "重启explorer？");

        let check = json!({"kind":"Check", "role":"window-topmost", "icon":"fa:Light_Thumbtack"});
        assert!(window_check(&check));
        assert!(!window_check(&json!({"kind":"Check", "role":"file-hidden", "icon":"fa:Light_Eye"})));
    }

    #[test]
    fn window_menu_does_not_capture_carousel_navigation_or_activation() {
        assert_eq!(default_native_key_scope(Category::Window, true), NativeSuperMenuKeyScope::Dismiss);
        assert_eq!(default_native_key_scope(Category::Clipboard, true), NativeSuperMenuKeyScope::ClipboardHub);
        assert_eq!(default_native_key_scope(Category::Clipboard, false), NativeSuperMenuKeyScope::Dismiss);
    }

    #[test]
    fn opacity_slider_uses_absolute_255_range_and_steps() {
        let node = json!({ "value": { "min": 26, "max": 255, "step": 1 } });
        assert_eq!(slider_value(&node, -1.0), 26.0);
        assert_eq!(slider_value(&node, 1.5), 255.0);
        assert_eq!(slider_value(&node, 0.5), 141.0);
        let step = json!({ "value": { "min": 0, "max": 100, "step": 5 } });
        assert_eq!(slider_value(&step, 0.26), 25.0);
    }

    #[test]
    fn values_keep_fractional_steps_finite_bounds_and_web_percent_rules() {
        let fractional = json!({ "value": { "min": 0.0, "max": 1.0, "step": 0.1 } });
        assert!((slider_value(&fractional, 0.26) - 0.3).abs() < 1e-9);
        assert!((key_value(&fractional, 0.3, "right").unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(slider_value(&fractional, f64::NAN), 0.0);
        assert_eq!(normalize_value(&fractional, f64::INFINITY), 0.0);
        let opacity = json!({ "value": { "min": 26, "max": 255, "step": 1 } });
        assert_eq!(value_percent(&opacity, 128.0), "50%");
        let relative = json!({ "value": { "min": 20, "max": 100, "step": 1 } });
        assert_eq!(value_percent(&relative, 60.0), "50%");
        assert_eq!(key_value(&relative, 60.0, "Home"), Some(20.0));
        assert_eq!(key_value(&relative, 60.0, "End"), Some(100.0));
        assert_eq!(
            value_percent(&json!({ "value": { "min": 7, "max": 7 } }), 7.0),
            "7"
        );
        assert_eq!(
            slider_value(
                &json!({ "value": { "min": 0, "max": 10, "step": 0 } }),
                0.26
            ),
            3.0
        );
    }

    fn value_node(current: f64) -> Value {
        json!({ "id": "opacity", "kind": "Value", "value": { "min": 26, "max": 255, "step": 1, "current": current } })
    }

    #[test]
    fn old_value_reply_acknowledges_without_clobbering_a_new_drag() {
        let mut draft = ValueDraft::new(value_node(200.0), 1);
        draft.edit(100.0, 2);
        draft.queue_commit();
        let submitted = draft.begin_submit().unwrap();
        draft.edit(140.0, 3);
        assert!(draft.accept_reply(submitted, Some(&value_node(100.0))));
        assert_eq!(draft.confirmed, 100.0);
        assert_eq!(draft.local, 140.0);
        draft.sync_node(&value_node(90.0));
        assert_eq!(
            draft.local, 140.0,
            "a late page or patchedItem cannot replace an active draft"
        );
        draft.queue_commit();
        assert_eq!(draft.begin_submit().unwrap().value, 140.0);
    }

    #[test]
    fn value_commits_coalesce_to_the_latest_intent_and_remain_editable() {
        let mut draft = ValueDraft::new(value_node(200.0), 1);
        draft.edit(100.0, 2);
        draft.queue_commit();
        let first = draft.begin_submit().unwrap();
        draft.edit(110.0, 3);
        draft.queue_commit();
        draft.edit(120.0, 4);
        draft.queue_commit();
        assert!(
            draft.begin_submit().is_none(),
            "only one request executes for a value node"
        );
        assert_eq!(draft.queued.unwrap().value, 120.0);
        draft.accept_reply(first, Some(&value_node(100.0)));
        assert_eq!(draft.local, 120.0);
        let final_request = draft.begin_submit().unwrap();
        assert_eq!(final_request.value, 120.0);
        assert!(
            !draft.accept_reply(first, Some(&value_node(100.0))),
            "duplicate old replies cannot retire the new request"
        );
        draft.accept_reply(final_request, Some(&value_node(120.0)));
        assert_eq!(draft.local, 120.0);
        assert_eq!(draft.confirmed, 120.0);
        assert!(draft.in_flight.is_none() && draft.queued.is_none());
    }

    #[test]
    fn returning_to_the_pending_value_removes_a_superseded_queued_commit() {
        let mut draft = ValueDraft::new(value_node(200.0), 1);
        draft.edit(100.0, 2);
        draft.queue_commit();
        let first = draft.begin_submit().unwrap();
        draft.edit(110.0, 3);
        draft.queue_commit();
        draft.edit(100.0, 4);
        draft.queue_commit();
        assert!(draft.queued.is_none());
        draft.accept_reply(first, Some(&value_node(100.0)));
        assert!(draft.begin_submit().is_none());
        assert_eq!(draft.local, 100.0);
    }

    #[test]
    fn failed_value_submission_retains_the_draft_for_retry() {
        let mut draft = ValueDraft::new(value_node(200.0), 1);
        draft.edit(130.0, 2);
        draft.queue_commit();
        draft.begin_submit().unwrap();
        // This is the error callback path: no server value was acknowledged.
        draft.in_flight = None;
        draft.sync_node(&value_node(200.0));
        assert_eq!(draft.confirmed, 200.0);
        assert_eq!(draft.local, 130.0);
        draft.queue_commit();
        assert_eq!(draft.begin_submit().unwrap().value, 130.0);
    }

    #[test]
    fn clipboard_hub_metadata_uses_list_fields_and_stays_below_the_bar() {
        let file = json!({"kind": "FileClipItem", "fileCount": 3, "sourceProcessName": " explorer.exe ", "createTime": "invalid"});
        let (source, time, size) = clip_meta(&file);
        assert_eq!(source, "explorer.exe");
        assert!(time.is_empty());
        assert!(size.contains('3'));
        let image = json!({"kind": "ImageClipItem", "imageSizeBytes": 1500});
        assert_eq!(clip_meta(&image).2, "2 KB");
        assert!(clip_meta(&json!({"kind": "TextClipItem", "textLength": 0})).2.is_empty());
        assert_eq!(clip_kind_view("FileClipItem").0, "file");
        assert_eq!(clip_kind_view("unknown").0, "text");
        let hub = clipboard_hub_rect((1280.0, 600.0), 200.0, 116.0);
        assert_eq!((hub.x, hub.y), (200.0, 116.0));
        assert!(hub.y + hub.height <= 600.0 - EDGE);
        // Near the right edge the hub slides back on screen.
        let hub = clipboard_hub_rect((1280.0, 600.0), 1000.0, 116.0);
        assert!(hub.x + hub.width <= 1280.0);
    }

    #[test]
    fn clipboard_hub_keyboard_pages_instead_of_wrapping_a_partial_history() {
        assert_eq!(next_clip_index(99, 1, 100, true), None);
        assert_eq!(next_clip_index(0, -1, 100, true), None);
        assert_eq!(next_clip_index(5, 1, 100, true), Some(6));
        assert_eq!(next_clip_index(99, 1, 100, false), Some(0));
        assert_eq!(next_clip_index(0, -1, 100, false), Some(99));
        assert!(HUB_TAKE >= 100 && HUB_PREFETCH_ROWS == 20);
    }

    #[test]
    fn deleting_a_clip_during_pagination_retires_the_old_reader_and_allows_the_next_batch() {
        // The reader was accepted at offset 200. A deletion moves its next
        // unread row to 199; retaining that reader would reject its response
        // for the old offset while keeping the load-more slot occupied.
        let mut pending = Some(Task::ready(()));
        let next_skip = clip_page_offset_after_delete(200, &mut pending);
        assert_eq!(next_skip, 199);
        assert!(pending.is_none(), "a fresh page must be allowed after deletion");
        assert_eq!(next_clip_index(198, 1, 199, true), None,
            "Down at the remaining loaded boundary still requests the next batch");

        assert_eq!(clip_page_offset_after_delete(1, &mut pending), 0);
        assert_eq!(clip_page_offset_after_delete(0, &mut pending), 0);
        assert!(pending.is_none());
    }

    #[test]
    fn clipboard_rows_never_title_images_with_file_names() {
        let image = json!({"kind": "ImageClipItem", "displayText": "a1b2c3d4e5.png", "sourceProcessName": "Snipaste"});
        let view = clip_row_view(&image);
        assert!(!view.title.contains(".png"));
        assert!(view.meta.contains("Snipaste"));
        assert_eq!(clip_thumb_source(&image), Some(ClipThumbSource::Clip));

        let files = json!({"kind": "FileClipItem", "fileCount": 3,
            "filePaths": [r"D:\docs\plan.md", r"D:\docs\notes.txt", r"D:\docs\x.md"]});
        let view = clip_row_view(&files);
        assert!(view.title.starts_with("plan.md") && view.title.contains("notes.txt") && view.title.contains('3'));
        assert!(view.meta.ends_with(r"D:\docs"), "time stays visible before a long folder");
        assert_eq!(clip_thumb_source(&files), None);
        let one_image = json!({"kind": "FileClipItem", "fileCount": 1, "filePaths": [r"C:\shots\Cap.PNG"]});
        assert_eq!(clip_thumb_source(&one_image), Some(ClipThumbSource::Path(r"C:\shots\Cap.PNG".into())));

        let link = json!({"kind": "LinkClipItem", "displayText": "https://www.github.com/ceastld/eaclip"});
        assert!(clip_row_view(&link).meta.starts_with("github.com"));
        let text = json!({"kind": "TextClipItem", "displayText": "line one\n\n  line two"});
        let view = clip_row_view(&text);
        assert_eq!(view.title, "line one line two");
        assert!(view.two_lines);
    }

    #[test]
    fn clipboard_row_thumbnails_are_small_pngs_with_natural_size() {
        let source = image::RgbaImage::from_pixel(1280, 720, image::Rgba([10, 20, 30, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source).write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let thumb = hub_thumbnail(bytes.into_inner()).unwrap();
        assert_eq!((thumb.width, thumb.height), (1280, 720));
        let small = image::load_from_memory(&thumb.image.bytes).unwrap();
        assert!(small.width() <= HUB_THUMB_PIXELS.0 && small.height() <= HUB_THUMB_PIXELS.1);
        assert!(hub_thumbnail(b"not an image".to_vec()).is_none());
    }

    #[test]
    fn clipboard_actions_handle_image_paths_unicode_urls_and_punctuation() {
        let image = json!({ "kind": "ImageClipItem", "filePaths": ["C:\\中文 图片.png"] });
        assert_eq!(
            open_target(&image),
            Some(OpenTarget::Path("C:\\中文 图片.png".to_string()))
        );
        assert!(can_pin(&image));
        assert_eq!(
            open_target(
                &json!({ "kind": "TextClipItem", "displayText": "查看 https://example.test/中文?q=1)." })
            ),
            Some(OpenTarget::Uri("https://example.test/中文?q=1".to_string()))
        );
        assert_eq!(
            open_target(&json!({ "kind": "TextClipItem", "displayText": "www.example.test" })),
            Some(OpenTarget::Uri("https://www.example.test".to_string()))
        );
        assert!(
            open_target(&json!({ "kind": "TextClipItem", "displayText": "https://" })).is_none()
        );
        assert!(!can_pin(
            &json!({ "kind": "FileClipItem", "fileCount": 2, "filePaths": ["C:\\a.png", "C:\\b.png"] })
        ));
    }
}
