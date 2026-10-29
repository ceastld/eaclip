//! Native favorite panel. The catalog, directory contents, configured click
//! behavior and every write belong to HostState. Each retained window request
//! is scoped to both the core presentation and the local directory lifetime.

use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use cliphost_core::{
    host::{ChromeTask, HostState},
    i18n::tr,
    ChromeCommand,
};
use gpui::{
    div, img, list, point, prelude::*, px, rgb, rgba, AnyElement, AnyWindowHandle, App,
    Context, Entity, FocusHandle, Hsla, Image, ImageFormat, KeyDownEvent, ListAlignment, ListState,
    MouseButton, ScrollHandle, SharedString, Subscription, Task, Window, WindowHandle,
    WindowOptions,
};
use crate::crisp_svg::svg;
use crate::smooth_wheel::SmoothWheelExt;
use serde_json::{json, Value};

use crate::{
    favorite_drag::{ContentDrag, DragSource, DropTarget, Placement},
    input::{InputColors, SearchEvent, SearchInput},
    menu::{ContextMenuPopup, MenuAction, MenuEntry, SelectHandler},
    multiline::{MultilineEvent, MultilineInput},
    motion::{self, Tweens},
    platform,
    preview::{DesignTokens, ThemeTokens},
    settings::SettingsForm,
    view::{color, hover_binding},
    window_background::{PreparedBackground, WindowBackgroundOptions},
};

#[cfg(windows)]
use crate::native_interactions::{DropDestination, NativeInteractionEvent, NativeInteractions};
use crate::{service::NativeHost, view::next_native_session};
#[path = "panel_surfaces.rs"]
mod surfaces;
#[path = "panel_drag.rs"]
mod drag_support;
pub(crate) use surfaces::css_mix;
use surfaces::Style;
const PANEL_SIZE: (f64, f64) = (420.0, 520.0);
const ORPHANS: &str = "__cea:detached-orphans";

fn sidebar_width(viewport_width: f32) -> f32 {
    (viewport_width * 0.25).clamp(96.0, 120.0)
}

fn action_available(busy: bool, loading: bool, current: bool) -> bool {
    !busy && !loading && current
}

struct PanelTooltip {
    text: SharedString,
    tokens: DesignTokens,
    dark: bool,
}

impl Render for PanelTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div max_w={px(400.0)} px_2 py_1 rounded={px(4.0)} border_1
                border_color={color(&t.border)} bg={color(&t.surface)}
                text_size={px(self.tokens.typography.small_font_size)} text_color={color(&t.text)}>
                {self.text.clone()}
            </div>
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn panel_hover_plan(item: &Value) -> crate::hover::RowHoverContent {
    use crate::hover::{row_hover_content, RowHoverInput, RowHoverSource};
    let paths = item["filePaths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let segments = item["imRichSegments"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    row_hover_content(
        RowHoverSource::Favorite,
        RowHoverInput {
            id: item["id"].as_str().unwrap_or_default(),
            kind: item["kind"].as_str().unwrap_or_default(),
            title: item["title"].as_str().unwrap_or_default(),
            summary: item["summary"].as_str().unwrap_or_default(),
            display_text: item["displayText"].as_str().unwrap_or_default(),
            file_paths: &paths,
            im_rich_segments: segments,
        },
    )
}

/// The panel paints one icon, not every image in the content plan. Only that
/// icon's actual payload source can register a thumbnail, never a generic SVG
/// or an unrelated user-selected favorite icon.
fn panel_icon_hover_target(
    item: &Value,
    plan: &crate::hover::RowHoverContent,
) -> Option<crate::hover::HoverThumbnailTarget> {
    use crate::hover::{HoverThumbnailTarget, ImageSource};
    if item["isFolder"].as_bool() == Some(true) {
        return None;
    }
    if item["kind"].as_str() == Some("ImageClipItem") {
        return plan
            .thumbnail(HoverThumbnailTarget::ItemImage)
            .map(|entry| entry.target);
    }
    let icon = item["icon"].as_str().unwrap_or_default();
    let path = icon.strip_prefix("image:").unwrap_or(icon);
    plan.thumbnails.iter().find_map(|thumbnail| {
        matches!(&thumbnail.source, ImageSource::Local(source) if source == path)
            .then_some(thumbnail.target)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Lifetime {
    display: u64,
    page: u64,
    presentation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsLeaveDisposition {
    Keep,
    Back,
    Dismiss,
}

/// Finish the batch regardless of display changes, but scope disposal/dismiss
/// to the UI intent that is still current. A successful old save is not a new
/// user's close request; external Hide/Show never revives that close intent.
fn settings_leave_disposition(
    batch: &Lifetime,
    close: Option<&Lifetime>,
    current: &Lifetime,
    visible_current: bool,
    same_forms: bool,
    saved: bool,
) -> SettingsLeaveDisposition {
    if !saved || !same_forms {
        return SettingsLeaveDisposition::Keep;
    }
    if let Some(close) = close {
        if visible_current && close == current {
            SettingsLeaveDisposition::Dismiss
        } else {
            SettingsLeaveDisposition::Keep
        }
    } else if batch == current {
        SettingsLeaveDisposition::Back
    } else {
        SettingsLeaveDisposition::Keep
    }
}

fn snapshot_presentation(snapshot: &Value) -> u64 {
    snapshot["presentationGeneration"].as_u64().unwrap_or(0)
}

fn visible(snapshot: &Value) -> bool {
    snapshot["visible"].as_bool() == Some(true)
}

fn input_generation(snapshot: &Value) -> (u64, u64) {
    (
        snapshot["windowInputGeneration"].as_u64().unwrap_or(0),
        snapshot["inputGeneration"].as_u64().unwrap_or(0),
    )
}

struct PanelActionReply {
    operation: u64,
    owner_hwnd: isize,
    presentation: u64,
}

struct SettledPanelAction {
    release_picker_overlay: bool,
    result: Option<Result<Value, String>>,
}

impl PanelActionReply {
    fn settle(
        &self,
        current_operation: u64,
        picker_guard: &mut bool,
        snapshot: &Value,
        accepts_page: bool,
        result: Result<Value, String>,
    ) -> Option<SettledPanelAction> {
        if self.operation != current_operation {
            return None;
        }
        // A folder change can retire the page while this same modal still
        // owns the overlay. Settle that ownership before filtering its reply.
        let owned_picker = std::mem::take(picker_guard);
        let release_picker_overlay = owned_picker
            && self.owner_hwnd != 0
            && snapshot["hwnd"].as_i64().unwrap_or(0) as isize == self.owner_hwnd
            && snapshot_presentation(snapshot) == self.presentation;
        Some(SettledPanelAction {
            release_picker_overlay,
            result: accepts_page.then_some(result),
        })
    }
}

fn decode_parameter(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let source = value.as_bytes();
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'%' {
            let pair = std::str::from_utf8(source.get(i + 1..i + 3)?).ok()?;
            bytes.push(u8::from_str_radix(pair, 16).ok()?);
            i += 3;
        } else {
            bytes.push(if source[i] == b'+' { b' ' } else { source[i] });
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn route_selection(route: &str) -> (String, Option<String>) {
    let mut root = String::new();
    let mut folder = None;
    for pair in route.split_once('?').map_or("", |(_, q)| q).split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let Some(value) = decode_parameter(value) else {
            continue;
        };
        if key == "favorId" {
            root = value;
        } else if key == "folderId" && !value.is_empty() {
            folder = Some(value);
        }
    }
    (root, folder)
}

#[derive(Clone)]
struct NavRow {
    root: String,
    id: String,
    title: String,
    depth: usize,
    has_children: bool,
    parent: Option<String>,
    path: Vec<String>,
}

fn flatten_nav(
    entries: &[Value],
    root: &str,
    depth: usize,
    expanded: &HashSet<String>,
    output: &mut Vec<NavRow>,
    parent_path: &[String],
) {
    for entry in entries {
        let id = text(entry, "favorItemId");
        if id.is_empty() {
            continue;
        }
        let children = entry["children"].as_array().cloned().unwrap_or_default();
        let mut path = parent_path.to_vec();
        path.push(id.clone());
        output.push(NavRow {
            root: root.into(),
            id: id.clone(),
            title: text(entry, "title"),
            depth,
            has_children: !children.is_empty(),
            parent: parent_path.last().cloned(),
            path: path.clone(),
        });
        if expanded.contains(&id) {
            flatten_nav(&children, root, depth + 1, expanded, output, &path);
        }
    }
}

fn root_for_folder(layout: &Value, folder: &str) -> Option<String> {
    fn contains(entry: &Value, id: &str, visited: &mut HashSet<String>) -> bool {
        let own = text(entry, "favorItemId");
        if own == id {
            return true;
        }
        if !visited.insert(own) {
            return false;
        }
        entry["children"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|child| contains(child, id, visited))
    }
    let default = text(layout, "defaultFavorItemId");
    if layout["mode"].as_str() != Some("multiRoot") {
        return Some(default);
    }
    layout["navEntries"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| contains(entry, folder, &mut HashSet::new()))
        .map(|entry| text(entry, "favorItemId"))
}

fn number(appearance: &Value, name: &str, default: f32) -> f32 {
    appearance[name]
        .as_f64()
        .filter(|n| n.is_finite())
        .map(|n| (n as f32).clamp(1.0, 1024.0))
        .unwrap_or(default)
}

fn panel_style(page: &Value) -> Style {
    match page["appearance"]["panelStyle"].as_str().unwrap_or("") {
        "PanelStyle1" | "1" => Style::Big,
        "PanelStyle3" | "3" => Style::Chips,
        "PanelStyle4" | "4" => Style::Icons,
        _ => Style::List,
    }
}

fn tile_width(item: &Value, style: Style, appearance: &Value, available: f32) -> f32 {
    match style {
        Style::List => available,
        Style::Big => number(appearance, "bigIconWidth", 60.0).min(available),
        Style::Icons => number(appearance, "onlyIconSize", 30.0).min(available),
        Style::Chips => {
            let font = number(appearance, "smallIconFontSize", 15.0);
            let title = item["title"].as_str().unwrap_or("");
            let units: f32 = title
                .chars()
                .map(|c| if c.is_ascii() { 0.58 } else { 1.0 })
                .sum();
            let max = (number(appearance, "smallWidth", 80.0) * 2.5).max(320.0);
            (units * font + 44.0).clamp(44.0, max).min(available)
        }
    }
}

/// Virtualize *rows*, including wrapping styles, so a large directory does
/// not construct thousands of buttons. Directory order is never re-sorted.
fn row_ranges(
    items: &[Value],
    style: Style,
    appearance: &Value,
    available: f32,
) -> Vec<std::ops::Range<usize>> {
    if style == Style::List {
        return (0..items.len()).map(|i| i..i + 1).collect();
    }
    let mut result = Vec::new();
    let mut start = 0;
    let mut used = 0.0;
    let gap = if style == Style::Icons { 4.0 } else { 6.0 };
    for (index, item) in items.iter().enumerate() {
        let width = tile_width(item, style, appearance, available);
        if index > start && used + gap + width > available {
            result.push(start..index);
            start = index;
            used = 0.0;
        }
        used += if used > 0.0 { gap } else { 0.0 } + width;
    }
    if start < items.len() {
        result.push(start..items.len());
    }
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelScope {
    display: u64,
    presentation: u64,
    root: String,
    folder: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PanelRowKey {
    members: Vec<(String, usize)>,
    fingerprint: String,
}

fn panel_row_keys(items: &[Value], rows: &[std::ops::Range<usize>], metrics: &str, tree: &Value) -> Vec<PanelRowKey> {
    let mut occurrences = HashMap::<String, usize>::new();
    rows.iter().map(|range| {
        let mut hash = blake3::Hasher::new();
        hash.update(metrics.as_bytes());
        let members = range.clone().filter_map(|index| items.get(index)).map(|item| {
            let id = text(item, "id");
            let occurrence = occurrences.entry(id.clone()).or_default();
            let member = (id.clone(), *occurrence);
            *occurrence += 1;
            hash.update(item.to_string().as_bytes());
            hash.update(tree["items"][&id]["childrenCount"].to_string().as_bytes());
            member
        }).collect();
        PanelRowKey { members, fingerprint:hash.finalize().to_hex().to_string() }
    }).collect()
}

fn panel_refreshed_anchor(old: &[PanelRowKey], new: &[PanelRowKey], anchor: gpui::ListOffset) -> gpui::ListOffset {
    let top = gpui::ListOffset { item_ix:0, offset_in_item:px(0.0) };
    if new.is_empty() || (anchor.item_ix == 0 && anchor.offset_in_item <= px(0.0)) { return top; }
    let positions: HashMap<_, _> = new.iter().enumerate()
        .flat_map(|(row, key)| key.members.iter().map(move |member| (member, row))).collect();
    let start = anchor.item_ix.min(old.len());
    if let Some(row) = old.get(start).and_then(|key| key.members.iter().find_map(|member| positions.get(member))) {
        return gpui::ListOffset { item_ix:*row, offset_in_item:anchor.offset_in_item };
    }
    let next = old[start..].iter().flat_map(|key| key.members.iter())
        .chain(old[..start].iter().rev().flat_map(|key| key.members.iter().rev()))
        .find_map(|member| positions.get(member)).copied();
    gpui::ListOffset { item_ix:next.unwrap_or_else(|| anchor.item_ix.min(new.len()-1)), offset_in_item:px(0.0) }
}

fn panel_selection_after_refresh(
    old: &[Value], new: &[Value], selected: &HashSet<String>, index: Option<usize>, anchor: Option<usize>,
) -> (HashSet<String>, Option<usize>, Option<usize>) {
    let positions: HashMap<_, _> = new.iter().enumerate().map(|(index, item)| (text(item,"id"), index)).collect();
    let at = |old_index: usize| old.get(old_index).and_then(|item| positions.get(&text(item,"id"))).copied();
    let focus = index.and_then(|old_index| {
        at(old_index).or_else(|| (old_index.saturating_add(1)..old.len()).find_map(at))
            .or_else(|| (0..old_index.min(old.len())).rev().find_map(at))
            .or_else(|| (!new.is_empty()).then(|| old_index.min(new.len()-1)))
    });
    let mut kept: HashSet<_> = selected.iter().filter(|id| positions.contains_key(*id)).cloned().collect();
    if kept.is_empty() && !selected.is_empty() {
        if let Some(index) = focus { kept.insert(text(&new[index],"id")); }
    }
    (kept, focus, anchor.and_then(at).or(focus))
}

fn panel_icon_revision(item: &Value, tree: &Value) -> Value {
    let id = text(item, "id");
    json!([item["kind"], item["icon"], tree["items"][&id]["revision"],
        tree["items"][&id]["updateTime"], tree["items"][&id]["imagePath"]])
}

#[derive(Clone)]
enum Dialog {
    Rename {
        id: String,
        revision: String,
    },
    AddFolder {
        parent: Option<String>,
    },
    AddText {
        parent: String,
        batch: bool,
    },
    Move {
        ids: Vec<String>,
    },
    Delete {
        ids: Vec<String>,
        descendants: usize,
    },
}

struct LoadedDirectory {
    catalog: Value,
    layout: Value,
    page: Value,
    tree: Value,
    root: String,
    folder: Option<String>,
    config: Value,
}

#[derive(Clone)]
struct DragNavigation {
    root: String,
    folder: Option<String>,
    path: Vec<String>,
    query: String,
    selected: HashSet<String>,
    index: Option<usize>,
    anchor: Option<usize>,
    scroll: gpui::ListOffset,
}

#[derive(Clone)]
struct DragHover {
    id: String,
    root: String,
    folder: Option<String>,
    since: Instant,
    navigate: bool,
    bounds: gpui::Bounds<gpui::Pixels>,
}

struct DragScroll {
    sidebar: bool,
    velocity: f32,
    last: Instant,
    bounds: gpui::Bounds<gpui::Pixels>,
}

struct NativeDragOffer {
    operation_id: String,
    target: DropTarget,
    path: Option<Vec<String>>,
    lifetime: Lifetime,
}

pub struct PanelWindow {
    window: AnyWindowHandle,
    native_host: Option<Arc<NativeHost>>,
    #[cfg(windows)]
    interactions: Option<NativeInteractions>,
    native_session: u64,
    #[cfg(windows)]
    native_lifetime: Option<Lifetime>,
    #[cfg(windows)]
    native_hover_tickets: hover_binding::HoverBindings,
    native_dragging: bool,
    native_drag_over: Option<u64>,
    host: Arc<HostState>,
    tokens: DesignTokens,
    base_tokens: DesignTokens,
    dark: bool,
    hwnd: isize,
    active: bool,
    display_generation: u64,
    page_generation: u64,
    presentation: u64,
    root: String,
    folder: Option<String>,
    path: Vec<String>,
    catalog: Value,
    layout: Value,
    page: Value,
    tree: Value,
    config: Value,
    committed_scope: Option<PanelScope>,
    filtered_query: String,
    background: Option<Arc<PreparedBackground>>,
    background_options: WindowBackgroundOptions,
    expanded: HashSet<String>,
    nav_visible: bool,
    focus: FocusHandle,
    search: Entity<SearchInput>,
    name: Entity<SearchInput>,
    body: Entity<MultilineInput>,
    _subscriptions: Vec<Subscription>,
    query: String,
    items: Vec<Value>,
    selected: HashSet<String>,
    index: Option<usize>,
    anchor: Option<usize>,
    list: ListState,
    rows: Vec<std::ops::Range<usize>>,
    row_keys: Vec<PanelRowKey>,
    content_width: f32,
    geometry: Option<platform::Geometry>,
    initialized: bool,
    no_activate: Option<bool>,
    loading: bool,
    busy: bool,
    operation_generation: u64,
    error: Option<String>,
    load: Option<Task<()>>,
    loaded: Option<(Lifetime, LoadedDirectory)>,
    refresh_needed: bool,
    picker_guard: bool,
    icons: HashMap<String, Option<Arc<Image>>>,
    icon_generation: u64,
    menu: Option<WindowHandle<ContextMenuPopup>>,
    menu_lease: Option<(u64, Vec<String>)>,
    menu_request_generation: u64,
    dialog: Option<Dialog>,
    dialog_generation: u64,
    dialog_scroll: ScrollHandle,
    settings_forms: Option<(Entity<SettingsForm>, Entity<SettingsForm>)>,
    settings_scroll: ScrollHandle,
    settings_leave_pending: bool,
    pending_local_close: Option<Lifetime>,
    dragging: bool,
    active_content_drag: Option<ContentDrag>,
    source_drag_operation: Option<String>,
    drop_target: Option<DropTarget>,
    drop_candidate: Option<DropTarget>,
    drop_path: Option<Vec<String>>,
    native_drag_offer: Option<NativeDragOffer>,
    drag_hover: Option<DragHover>,
    drag_navigation: Option<DragNavigation>,
    drag_expanded_before: Option<HashSet<String>>,
    restore_drag_navigation: Option<DragNavigation>,
    pending_drag_search: Option<String>,
    drag_scroll: Option<DragScroll>,
    sidebar_scroll: ScrollHandle,
    pending_drag_selection: Vec<String>,
    drag_undo: Option<(String, String)>,
    suppress_click_until: Option<Instant>,
    hovered: HashSet<SharedString>,
    motion: RefCell<Tweens<SharedString>>,
}

impl PanelWindow {
    fn request_local_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() {
            return;
        }
        self.cancel_native_hover();
        if self.settings_forms.is_none() {
            self.host.dismiss_native_window("panel");
            return;
        }
        // An X while Back is already awaiting writes upgrades only the current
        // close intent. The existing batch stays locked and is not submitted twice.
        self.pending_local_close = Some(self.lifetime());
        self.close_settings(window, cx);
    }
    /// Exit captures both still-mounted settings forms in one UI turn. Their
    /// common preflight rejects IME/invalid input before any leave writes;
    /// failed persistence keeps the exact editor entities and their drafts.
    pub(crate) fn save_embedded_settings_for_exit(
        handle: WindowHandle<Self>,
        cx: &mut App,
    ) -> crate::settings::SettingsCloseResult {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        let result = handle.update(cx, |this, window, cx| {
            let Some((panel, global)) = &this.settings_forms else {
                let _ = sender.take().unwrap().send(Ok(()));
                return;
            };
            let ticket =
                match SettingsForm::prepare_leave(&[panel.clone(), global.clone()], window, cx) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        this.error = Some(error.clone());
                        let _ = sender.take().unwrap().send(Err(error));
                        cx.notify();
                        return;
                    }
                };
            let sender = sender.take().unwrap();
            cx.spawn_in(window, async move |this, cx| {
                let result = ticket.wait().await;
                let finished = this
                    .update_in(cx, |this, _, cx| {
                        let result = result.finish(cx);
                        if let Err(error) = &result {
                            this.error = Some(error.clone());
                            cx.notify();
                        }
                        result
                    })
                    .unwrap_or_else(|error| Err(error.to_string()));
                let _ = sender.send(finished);
            })
            .detach();
        });
        if let Err(error) = result {
            if let Some(sender) = sender.take() {
                let _ = sender.send(Err(error.to_string()));
            }
        }
        receiver
    }
    pub(crate) fn bind_native_host(
        handle: WindowHandle<Self>,
        host: Arc<NativeHost>,
        cx: &mut App,
    ) {
        let _ = handle.update(cx, |this, _, cx| {
            this.native_host = Some(host);
            this.sync_native_interactions(cx);
        });
    }

    fn cancel_native_hover(&mut self) {
        #[cfg(windows)]
        {
            if let Some(native) = &self.interactions {
                native.cancel_hover();
            }
            self.native_hover_tickets.clear();
        }
    }
    fn retire_native_interactions(&mut self) {
        self.cancel_native_hover();
        self.native_session = next_native_session();
        self.native_drag_over = None;
        self.native_dragging = false;
        #[cfg(windows)]
        if let Some(native) = &mut self.interactions {
            let _ = native.sync_session(
                self.native_session,
                false,
                DropDestination {
                    label: "panel",
                    parent_id: self.folder.clone().or_else(|| Some(self.root.clone())),
                    insert_index: -1,
                },
            );
        }
    }
    fn sync_native_interactions(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        {
            let Some(service) = self.native_host.clone() else {
                return;
            };
            if self.hwnd == 0 || cliphost_core::is_headless() {
                return;
            }
            let lifetime = self.lifetime();
            if self.native_lifetime.as_ref() != Some(&lifetime) && !self.dragging && !self.native_dragging {
                self.native_lifetime = Some(lifetime);
                self.native_session = next_native_session();
                self.native_hover_tickets.clear();
                self.native_drag_over = None;
                self.native_dragging = false;
                cx.notify();
            }
            let parent = self.folder.clone().unwrap_or_else(|| self.root.clone());
            let destination = DropDestination {
                label: "panel",
                parent_id: Some(parent.clone()),
                insert_index: -1,
            };
            if self.interactions.is_none() {
                match NativeInteractions::attach(
                    service,
                    self.hwnd,
                    self.native_session,
                    destination.clone(),
                ) {
                    Ok(native) => self.interactions = Some(native),
                    Err(error) => {
                        self.error = Some(error);
                        cx.notify();
                        return;
                    }
                }
            }
            let accepting = self.current()
                && self.initialized
                && !self.busy
                && self.dialog.is_none()
                && self.settings_forms.is_none()
                && self.menu_lease.is_none()
                && !parent.is_empty()
                && parent != ORPHANS;
            if let Some(native) = &mut self.interactions {
                if let Err(error) = native.sync_session(self.native_session, accepting, destination)
                {
                    self.error = Some(error);
                    cx.notify();
                }
                let _ = native.set_menu_open(self.menu_lease.is_some());
            }
        }
        #[cfg(not(windows))]
        let _ = cx;
    }
    fn drain_native_interactions(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(windows)]
        {
            let events = self
                .interactions
                .as_mut()
                .map(NativeInteractions::drain)
                .unwrap_or_default();
            for event in events {
                match event {
                    NativeInteractionEvent::DropSaved { request, result } => {
                        if request.page_session() != self.native_session
                            || !self.host.native_drop_request_is_current(&request)
                        {
                            continue;
                        }
                        self.native_drag_over = None;
                        match result {
                            Ok(_) => self.reload(cx),
                            Err(error) => self.error = Some(error),
                        }
                    }
                    NativeInteractionEvent::DragOver { ticket, over } => {
                        if over && ticket.session() == self.native_session && ticket.is_current() {
                            self.native_drag_over = Some(ticket.id());
                        } else if !over && self.native_drag_over == Some(ticket.id()) {
                            self.native_drag_over = None;
                        }
                    }
                    NativeInteractionEvent::DragFinished { session, result } => {
                        if session != self.native_session {
                            continue;
                        }
                        self.native_dragging = false;
                        self.suppress_click_until =
                            Some(Instant::now() + Duration::from_millis(300));
                        if let Err(error) = result {
                            self.error = Some(error);
                        }
                    }
                    NativeInteractionEvent::ContentSaved { operation_id, result } => {
                        self.native_content_saved(operation_id, result, cx);
                    }
                    NativeInteractionEvent::Error(error) => self.error = Some(error),
                }
                cx.notify();
            }
        }
        #[cfg(not(windows))]
        let _ = (_window, cx);
    }
    fn drag_outside(
        &mut self,
        event: &gpui::DragMoveEvent<ContentDrag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(windows)]
        {
            let size = window.viewport_size();
            let position = event.event.position;
            if position.x >= px(0.)
                && position.y >= px(0.)
                && position.x < size.width
                && position.y < size.height
            {
                return;
            }
            let drag = event
                .dragged_item()
                .downcast_ref::<ContentDrag>()
                .expect("Content drag type");
            if self.source_drag_operation.as_deref() != Some(&drag.operation_id)
                || !drag.active() || self.native_dragging {
                return;
            }
            self.cancel_native_hover();
            self.drop_target = None;
            self.drag_hover = None;
            self.drag_scroll = None;
            if let Some(native) = &mut self.interactions {
                match native.start_content_drag(drag.clone()) {
                    Ok(()) => self.native_dragging = true,
                    Err(error) => self.error = Some(error),
                }
                cx.notify();
            }
        }
        #[cfg(not(windows))]
        let _ = (event, window, cx);
    }
    #[cfg(windows)]
    fn native_item_hover_binding(
        &self,
        index: usize,
        id: &str,
        lifetime: &Lifetime,
        session: u64,
        plan: crate::hover::RowHoverContent,
    ) -> Option<hover_binding::HoverBinding> {
        if cliphost_core::is_headless() || !self.accepts(lifetime) || self.native_session != session
        {
            return None;
        }
        self.items.get(index).filter(|item| {
            item["id"].as_str() == Some(id) && item["isFolder"].as_bool() != Some(true)
        })?;
        Some(self.native_hover_tickets.row(
            id,
            index,
            lifetime.page,
            session,
            plan,
            self.config["clipListImageHoverPreviewEnabled"].as_bool() != Some(false),
        ))
    }

    fn hover_native_item(
        &mut self,
        index: usize,
        id: &str,
        lifetime: &Lifetime,
        session: u64,
        event: hover_binding::HoverEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(windows)]
        {
            if !self.accepts(lifetime)
                || self.native_session != session
                || self.dialog.is_some()
                || self.settings_forms.is_some()
                || self.menu_lease.is_some()
                || self.loading
                || self.busy
                || self.native_dragging
                || self.dragging
                || !self.items.get(index).is_some_and(|item| {
                    item["id"].as_str() == Some(id) && item["isFolder"].as_bool() != Some(true)
                })
            {
                event.reject();
                return;
            }
            let owner = self
                .interactions
                .as_ref()
                .and_then(NativeInteractions::hover_owner);
            let Some(owner) = owner else {
                event.reject();
                return;
            };
            if let Err(error) = event.apply(owner, f64::from(window.scale_factor()), self.dark) {
                tracing::debug!(%error, "Hover preview request unavailable");
                cx.notify();
            }
        }
        #[cfg(not(windows))]
        let _ = (index, id, lifetime, session, event, window, cx);
    }
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut App,
    ) -> Result<(), String> {
        if !visible(&host.panel_native_snapshot()) {
            return Ok(());
        }
        let incoming_tokens = tokens.clone();
        let handle = if let Some(handle) = existing {
            *handle
        } else {
            let build_host = host.clone();
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        focus: false,
                        show: false,
                        ..Default::default()
                    },
                    move |window, cx| {
                        window.set_window_title(tr("超级面板", "Favorite panel"));
                        let view =
                            cx.new(|cx| Self::new(build_host.clone(), tokens, dark, window, cx));
                        let weak = view.downgrade();
                        window.on_window_should_close(cx, move |window, cx| {
                            let _ =
                                weak.update(cx, |this, cx| this.request_local_close(window, cx));
                            false
                        });
                        view
                    },
                )
                .map_err(|error| error.to_string())?;
            *existing = Some(handle);
            handle
        };
        handle
            .update(cx, |this, window, cx| {
                this.set_design_tokens(incoming_tokens, dark, cx);
                let snapshot = host.panel_native_snapshot();
                let guard_host = host.clone();
                let presentation = snapshot_presentation(&snapshot);
                let _guard = platform::guard_condition(move || {
                    let snapshot = guard_host.panel_native_snapshot();
                    visible(&snapshot) && snapshot_presentation(&snapshot) == presentation
                });
                platform::configure_host(window)?;
                #[cfg(windows)]
                {
                    this.hwnd = platform::hwnd(window)?.0 as isize;
                }
                host.set_panel_hwnd(this.hwnd);
                cliphost_core::window_chrome::install_html_titlebar_passthrough(this.hwnd);
                let drag_host = host.clone();
                cliphost_core::window_chrome::set_caption_drag_observer(this.hwnd, move |moving| {
                    drag_host.apply_window_action_for("setSpaOverlayOpen", moving, "panel");
                });
                host.set_panel_web_view_ready(false);
                if this.active_content_drag.is_some() {
                    crate::favorite_drag::cancel_active(window, cx);
                    this.finish_drag(cx);
                }
                this.active = true;
                this.pending_local_close = None;
                this.dark = dark;
                this.display_generation = this.display_generation.wrapping_add(1);
                this.operation_generation = this.operation_generation.wrapping_add(1);
                this.busy = false;
                this.name
                    .update(cx, |input, cx| input.set_read_only(false, cx));
                this.body
                    .update(cx, |input, cx| input.set_read_only(false, cx));
                this.presentation = snapshot_presentation(&snapshot);
                this.initialized = false;
                this.close_menu(cx);
                this.close_dialog(window, cx);
                // External Hide retains mounted settings and unsaved inputs.
                // Show never disposes or implicitly saves those forms; restore
                // the core input-layer guard before the display becomes ready.
                if this.settings_forms.is_some() {
                    host.apply_window_action_for("setSpaOverlayOpen", true, "panel");
                }
                platform::set_no_activate(window, true)?;
                this.no_activate = Some(true);
                let restored =
                    host.restored_host_window_rect_with_client_size("panel", Some(PANEL_SIZE));
                let saved_position =
                    restored
                        .and_then(|(position, _)| position)
                        .map(|position| match position {
                            cliphost_core::window_state::WindowPosition::Physical(x, y) => (x, y),
                            cliphost_core::window_state::WindowPosition::Logical(x, y) => {
                                let scale = f64::from(window.scale_factor());
                                ((f64::from(x) * scale) as i32, (f64::from(y) * scale) as i32)
                            }
                        });
                platform::set_geometry(
                    window,
                    host.place_panel_window_logical(PANEL_SIZE.0, PANEL_SIZE.1)
                        .or(saved_position),
                    Some(PANEL_SIZE),
                )?;
                platform::show(window, true, false)?;
                let route = host.take_pending_nav("panel");
                if let Some(route) = route {
                    let (root, folder) = route_selection(&route);
                    this.root = root;
                    this.folder = folder;
                } else if let Some(folder) = snapshot["folderId"].as_str() {
                    this.root = folder.to_string();
                    this.folder = None;
                }
                this.query.clear();
                this.search
                    .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                this.reload(cx);
                Ok::<_, String>(())
            })
            .map_err(|error| error.to_string())??;
        Ok(())
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| {
            this.suspend(window, cx);
            platform::hide(window)
        });
    }

    /// Called by the root's one service pump. The panel never consumes the
    /// shared event/chrome queue or installs a second keyboard hook.
    pub fn maintain(handle: WindowHandle<Self>, host: &HostState, dark: bool, cx: &mut App) {
        let snapshot = host.panel_native_snapshot();
        let drag_active = cx.has_active_drag();
        let _ = handle.update(cx, |this, window, cx| {
            if this.dragging && !drag_active {
                this.finish_drag(cx);
            }
            this.maintain_content_drag(window, cx);
            if !visible(&snapshot) || snapshot_presentation(&snapshot) != this.presentation {
                if this.active {
                    this.suspend(window, cx);
                    let _ = platform::hide(window);
                }
                return;
            }
            if let Some(error) = platform::take_error(window).ok().flatten() {
                this.error = Some(error);
                host.dismiss_native_window("panel");
                this.suspend(window, cx);
                let _ = platform::hide(window);
                return;
            }
            // Consume immutable native-drop replies before a queued generic
            // favorites refresh can replace their accepted page lifetime.
            this.drain_native_interactions(window, cx);
            if this.active && !platform::has_pending(window).unwrap_or(true) {
                if !this.initialized {
                    this.initialized = true;
                    host.set_panel_web_view_ready(true);
                    this.commit_loaded(cx);
                }
                if let Ok(geometry) = platform::geometry(window) {
                    if this.geometry != Some(geometry) && geometry.width > 0 && geometry.height > 0
                    {
                        host.remember_physical_host_window_rect(
                            "panel",
                            geometry.left,
                            geometry.top,
                            geometry.width,
                            geometry.height,
                            geometry.scale,
                        );
                        this.geometry = Some(geometry);
                        this.rebuild_rows(cx);
                    }
                }
                let no_activate = snapshot["noFocus"].as_bool().unwrap_or(true);
                if this.no_activate != Some(no_activate) {
                    let host = this.host.clone();
                    let presentation = this.presentation;
                    let _guard = platform::guard_condition(move || {
                        let snapshot = host.panel_native_snapshot();
                        visible(&snapshot)
                            && snapshot_presentation(&snapshot) == presentation
                            && snapshot["noFocus"].as_bool() == Some(no_activate)
                    });
                    match platform::set_no_activate(window, no_activate) {
                        Ok(()) => this.no_activate = Some(no_activate),
                        Err(error) => this.error = Some(error),
                    }
                }
                if this.refresh_needed
                    && !this.busy
                    && this.load.is_none()
                    && !this.dragging
                    && this.dialog.is_none()
                    && this.settings_forms.is_none()
                {
                    this.refresh_needed = false;
                    this.reload(cx);
                }
            }
            if this
                .menu_lease
                .as_ref()
                .is_some_and(|(lease, _)| !host.native_context_menu_is_current(*lease))
            {
                this.close_menu(cx);
            }
            this.sync_native_interactions(cx);
            if this.dark != dark {
                this.cancel_native_hover();
                this.set_theme(dark, cx);
            }
        });
    }

    pub fn refresh(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, _, cx| {
            if this.current() {
                // An in-flight load (e.g. right after a drop) is committed
                // first; maintenance then reloads behind the shown snapshot
                // instead of discarding it.
                if this.dialog.is_some() || this.busy || this.dragging || this.settings_forms.is_some() || this.load.is_some() {
                    this.refresh_needed = true;
                } else {
                    this.reload(cx);
                }
            }
        });
    }

    pub(crate) fn invalidate_content_resources(
        handle: WindowHandle<Self>, ids: Option<&HashSet<String>>, cx: &mut App,
    ) {
        let _ = handle.update(cx, |this, _, cx| {
            this.icon_generation = this.icon_generation.wrapping_add(1);
            // Retire all pending reads with the old resource epoch. Resolved
            // unchanged IDs remain reusable even when another item changed.
            this.icons.retain(|id, image| image.is_some() && ids.is_some_and(|ids| !ids.contains(id)));
            cx.notify();
        });
    }

    pub fn handle_chrome_input(handle: WindowHandle<Self>, task: ChromeTask, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| {
            if !this.host.prepare_chrome_task(task) {
                return;
            }
            let _guard = platform::guard_task(&this.host, task);
            if this.dragging && matches!(task.command(), ChromeCommand::PanelBack
                | ChromeCommand::PanelRefresh | ChromeCommand::PanelSettings
                | ChromeCommand::PanelNavKey(_) | ChromeCommand::BeginCaptionDrag("panel")) {
                return;
            }
            match task.command() {
                ChromeCommand::PanelBack => {
                    if this.settings_forms.is_some() {
                        this.close_settings(window, cx);
                    } else {
                        this.back(cx);
                    }
                }
                ChromeCommand::PanelRefresh => {
                    if this.dialog.is_some() || this.busy || this.settings_forms.is_some() {
                        this.refresh_needed = true;
                    } else {
                        this.reload(cx);
                    }
                }
                ChromeCommand::PanelSettings => this.open_settings(window, cx),
                ChromeCommand::PanelNavKey(key) => this.navigate_key(key, Some(task), cx),
                ChromeCommand::HostKeyboardFocus("panel") => {
                    if this.host.host_keyboard_focus_allowed("panel") {
                        if let Err(error) = platform::set_no_activate(window, false)
                            .and_then(|_| platform::activate(window))
                        {
                            this.error = Some(error);
                        }
                    }
                }
                ChromeCommand::BeginCaptionDrag("panel") => {
                    if let Err(error) = platform::begin_host_move(window, &this.host, "panel") {
                        this.error = Some(error);
                    }
                }
                ChromeCommand::DismissContextMenu(lease) => {
                    if this.menu_lease.as_ref().is_some_and(|(id, _)| *id == lease) {
                        this.close_menu(cx);
                    }
                }
                _ => {}
            }
            cx.notify();
        });
    }

    fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let colors = input_colors(&tokens, dark);
        let search = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(search_colors(&tokens, dark), cx);
            input.set_metrics(
                tokens.layout.search_height,
                tokens.typography.font_size,
                0.0,
                cx,
            );
            input.set_placeholder(tr("搜索当前目录", "Search this folder"), cx);
            input
        });
        let name = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(
                tokens.settings.control_height,
                tokens.typography.font_size,
                8.0,
                cx,
            );
            input.set_double_click_clears(false);
            input
        });
        let body = cx.new(|cx| {
            let mut input = MultilineInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(160.0, tokens.typography.font_size, 8.0, cx);
            input
        });
        let search_subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(query) => {
                    this.query = query.clone();
                    this.filter_items(cx);
                }
                SearchEvent::Submit => {
                    window.blur();
                    this.focus.focus(window);
                    this.execute_selected(None, cx);
                }
                SearchEvent::Escape => {
                    window.blur();
                    this.focus.focus(window);
                }
            });
        let name_subscription =
            cx.subscribe_in(&name, window, |this, _, event, window, cx| match event {
                SearchEvent::Submit => this.submit_dialog(window, cx),
                SearchEvent::Escape => this.close_dialog(window, cx),
                SearchEvent::Changed(_) => {}
            });
        let body_subscription =
            cx.subscribe_in(&body, window, |this, _, event, window, cx| match event {
                MultilineEvent::Save => this.submit_dialog(window, cx),
                MultilineEvent::Escape => this.close_dialog(window, cx),
                MultilineEvent::Changed(_)
                | MultilineEvent::Submit
                | MultilineEvent::PasteRequested(_) => {}
            });
        Self {
            window: window.window_handle(),
            native_host: None,
            #[cfg(windows)]
            interactions: None,
            native_session: next_native_session(),
            #[cfg(windows)]
            native_lifetime: None,
            #[cfg(windows)]
            native_hover_tickets: hover_binding::HoverBindings::default(),
            native_dragging: false,
            native_drag_over: None,
            host,
            base_tokens: tokens.clone(),
            tokens,
            dark,
            hwnd: 0,
            active: false,
            display_generation: 0,
            page_generation: 0,
            presentation: 0,
            root: String::new(),
            folder: None,
            path: Vec::new(),
            catalog: Value::Null,
            layout: Value::Null,
            page: Value::Null,
            tree: Value::Null,
            config: Value::Null,
            committed_scope: None,
            filtered_query: String::new(),
            background: None,
            background_options: WindowBackgroundOptions::default(),
            expanded: HashSet::new(),
            nav_visible: false,
            focus: cx.focus_handle(),
            search,
            name,
            body,
            _subscriptions: vec![search_subscription, name_subscription, body_subscription],
            query: String::new(),
            items: Vec::new(),
            selected: HashSet::new(),
            index: None,
            anchor: None,
            list: ListState::new(0, ListAlignment::Top, px(100.0)),
            rows: Vec::new(),
            row_keys: Vec::new(),
            content_width: 400.0,
            geometry: None,
            initialized: false,
            no_activate: None,
            loading: false,
            busy: false,
            operation_generation: 0,
            error: None,
            load: None,
            loaded: None,
            refresh_needed: false,
            picker_guard: false,
            icons: HashMap::new(),
            icon_generation: 0,
            menu: None,
            menu_lease: None,
            menu_request_generation: 0,
            dialog: None,
            dialog_generation: 0,
            dialog_scroll: ScrollHandle::new(),
            settings_forms: None,
            settings_scroll: ScrollHandle::new(),
            settings_leave_pending: false,
            pending_local_close: None,
            dragging: false,
            active_content_drag: None,
            source_drag_operation: None,
            drop_target: None,
            drop_candidate: None,
            drop_path: None,
            native_drag_offer: None,
            drag_hover: None,
            drag_navigation: None,
            drag_expanded_before: None,
            restore_drag_navigation: None,
            pending_drag_search: None,
            drag_scroll: None,
            sidebar_scroll: ScrollHandle::new(),
            pending_drag_selection: Vec::new(),
            drag_undo: None,
            suppress_click_until: None,
            hovered: HashSet::new(),
            motion: RefCell::new(Tweens::default()),
        }
    }

    fn lifetime(&self) -> Lifetime {
        Lifetime {
            display: self.display_generation,
            page: self.page_generation,
            presentation: self.presentation,
        }
    }

    fn current(&self) -> bool {
        let snapshot = self.host.panel_native_snapshot();
        self.active
            && visible(&snapshot)
            && snapshot_presentation(&snapshot) == self.presentation
            && snapshot["hwnd"].as_i64().unwrap_or(0) as isize == self.hwnd
    }

    fn accepts(&self, lifetime: &Lifetime) -> bool {
        self.current() && self.lifetime() == *lifetime
    }

    fn action_available(&self) -> bool {
        action_available(self.busy, self.loading, self.current())
    }

    fn suspend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.committed_scope = None;
        crate::favorite_drag::clear_target(window);
        self.native_drag_offer = None;
        if let Some(drag) = self.active_content_drag.take() {
            drag.cancel();
        }
        self.drag_hover = None;
        self.drag_scroll = None;
        self.drag_navigation = None;
        self.drag_expanded_before = None;
        self.restore_drag_navigation = None;
        self.pending_drag_search = None;
        self.source_drag_operation = None;
        self.active = false;
        self.pending_local_close = None;
        self.retire_native_interactions();
        self.display_generation = self.display_generation.wrapping_add(1);
        self.page_generation = self.page_generation.wrapping_add(1);
        self.operation_generation = self.operation_generation.wrapping_add(1);
        self.busy = false;
        self.name
            .update(cx, |input, cx| input.set_read_only(false, cx));
        self.body
            .update(cx, |input, cx| input.set_read_only(false, cx));
        self.loading = false;
        self.load = None;
        self.loaded = None;
        self.refresh_needed = false;
        self.picker_guard = false;
        self.dragging = false;
        self.drop_target = None;
        self.drop_candidate = None;
        self.drop_path = None;
        self.suppress_click_until = None;
        self.host.set_panel_web_view_ready(false);
        self.close_menu(cx);
        self.close_dialog(window, cx);
        // Core-driven Hide keeps settings drafts mounted. Accepted writes may
        // settle, but this retired display cannot dismiss a later Show.
        self.icons.clear();
        self.hovered.clear();
        self.motion.borrow_mut().clear();
        cx.notify();
    }

    fn refresh_input_colors(&mut self, cx: &mut Context<Self>) {
        let colors = input_colors(&self.tokens, self.dark);
        self.search
            .update(cx, |input, cx| input.set_colors(search_colors(&self.tokens, self.dark), cx));
        self.name
            .update(cx, |input, cx| input.set_colors(colors, cx));
        self.body
            .update(cx, |input, cx| input.set_colors(colors, cx));
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark != dark {
            self.dark = dark;
            self.refresh_window_surfaces(cx);
        }
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
        self.background = snapshot;
        self.background_options = options;
        self.base_tokens = base_tokens;
        self.dark = dark;
        self.refresh_window_surfaces(cx);
    }

    fn refresh_window_surfaces(&mut self, cx: &mut Context<Self>) {
        // Start with the authoritative resolved palette on every refresh;
        // applying glass to the previously faded tokens would compound alpha.
        self.tokens = if self.background.is_some() {
            let mut tokens = crate::window_glass::tokens(&self.base_tokens, &self.background_options);
            tokens.themes.light.neutral_active = surfaces::glass_neutral(&self.base_tokens.themes.light, &self.background_options);
            tokens.themes.dark.neutral_active = surfaces::glass_neutral(&self.base_tokens.themes.dark, &self.background_options);
            tokens
        } else {
            self.base_tokens.clone()
        };
        self.refresh_input_colors(cx);
        if let Some((panel, global)) = &self.settings_forms {
            panel.update(cx, |form, cx| {
                form.set_window_background_theme(
                    self.background.is_some(),
                    self.background_options.clone(),
                    self.base_tokens.clone(),
                    self.dark,
                    cx,
                )
            });
            global.update(cx, |form, cx| {
                form.set_window_background_theme(
                    self.background.is_some(),
                    self.background_options.clone(),
                    self.base_tokens.clone(),
                    self.dark,
                    cx,
                )
            });
        }
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        crate::favorite_drag::clear_target_hwnd(self.hwnd);
        if !self.current() {
            return;
        }
        let retain_snapshot = self.committed_scope.as_ref().is_some_and(|scope| {
            *scope == PanelScope { display:self.display_generation, presentation:self.presentation,
                root:self.root.clone(), folder:self.folder.clone().unwrap_or_else(|| self.root.clone()) }
        });
        self.page_generation = self.page_generation.wrapping_add(1);
        if !self.dragging && !self.native_dragging { self.retire_native_interactions(); }
        let lifetime = self.lifetime();
        let (host, root, folder) = (self.host.clone(), self.root.clone(), self.folder.clone());
        self.loading = true;
        self.error = None;
        if retain_snapshot { self.icons.retain(|_, image| image.is_some()); }
        else { self.icons.clear(); }
        self.close_menu(cx);
        self.loaded = None;
        self.refresh_needed = false;
        if !retain_snapshot {
            self.committed_scope = None;
            self.page = Value::Null;
            self.tree = Value::Null;
            self.items.clear();
            self.rows.clear();
            self.row_keys.clear();
            self.list.reset(0);
            self.selected.clear();
            self.index = None;
            self.anchor = None;
        }
        self.load = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let catalog = host.panel_catalog()?;
                    let layout = host.panel_layout()?;
                    let root = if root.is_empty() {
                        text(&layout, "defaultFavorItemId")
                    } else {
                        root
                    };
                    // External core ShowPanel routes identify the displayed folder.
                    // Resolve its actual root from the existing catalog/tree.
                    let (root, folder) = if folder.is_none() && root != ORPHANS {
                        match root_for_folder(&layout, &root) {
                            Some(panel) if panel != root => (panel, Some(root)),
                            _ => (root, folder),
                        }
                    } else {
                        (root, folder)
                    };
                    // The rendered members and their structural revision must
                    // describe one snapshot, even when another window edits
                    // while this directory is loading.
                    let mut tree = host.favor_tree()?;
                    let mut page = host.panel_page(&json!({"favorId":root,"folderId":folder}))?;
                    if host.favorite_drag_revision()? != text(&tree, "dragRevision") {
                        tree = host.favor_tree()?;
                        page = host.panel_page(&json!({"favorId":root,"folderId":folder}))?;
                        if host.favorite_drag_revision()? != text(&tree, "dragRevision") {
                            return Err(tr("收藏已发生变化，请刷新后重试", "Favorites changed; refresh and try again").into());
                        }
                    }
                    let config = host.config_get();
                    Ok::<_, String>(LoadedDirectory {
                        catalog,
                        layout,
                        page,
                        tree,
                        root,
                        folder,
                        config,
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.accepts(&lifetime) {
                    return;
                }
                this.load = None;
                match result {
                    Ok(data) => {
                        this.loaded = Some((lifetime.clone(), data));
                        this.commit_loaded(cx);
                    }
                    Err(error) => {
                        this.loading = false;
                        if !retain_snapshot || matches!(error.as_str(), "Panel page not found." | "Panel layout not found.") {
                            this.committed_scope = None;
                            this.page = Value::Null;
                            this.tree = Value::Null;
                            this.items.clear();
                            this.rows.clear();
                            this.row_keys.clear();
                            this.list.reset(0);
                            this.icons.clear();
                            this.selected.clear();
                            this.index = None;
                            this.anchor = None;
                        }
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn commit_loaded(&mut self, cx: &mut Context<Self>) {
        if !self.initialized {
            return;
        }
        let Some((lifetime, data)) = self.loaded.take() else {
            return;
        };
        if !self.accepts(&lifetime) {
            return;
        }
        // Validate the HWND and display generation under the same core lock as
        // the chrome write. A focus handoff does not cancel a directory read.
        if !self.host.apply_native_panel_page(
            lifetime.presentation,
            self.hwnd,
            data.page["canGoBack"].as_bool().unwrap_or(false),
            data.page["currentFavorId"].as_str().unwrap_or(""),
        ) {
            return;
        }
        self.loading = false;
        self.icons.retain(|id, image| {
            image.is_some() && self.page["items"].as_array().into_iter().flatten().find(|item| item["id"].as_str() == Some(id))
                .zip(data.page["items"].as_array().into_iter().flatten().find(|item| item["id"].as_str() == Some(id)))
                .is_some_and(|(old, new)| panel_icon_revision(old, &self.tree) == panel_icon_revision(new, &data.tree))
        });
        self.catalog = data.catalog;
        self.layout = data.layout;
        self.page = data.page;
        self.tree = data.tree;
        self.config = data.config;
        self.root = data.root;
        self.folder = data.folder;
        self.committed_scope = Some(PanelScope { display:self.display_generation, presentation:self.presentation,
            root:self.root.clone(), folder:text(&self.page,"currentFavorId") });
        let current = text(&self.page, "currentFavorId");
        if self.path.first() != Some(&self.root) || self.path.last() != Some(&current) {
            self.path = drag_support::occurrence_path(&self.tree, &self.root, &current)
                .unwrap_or_else(|| vec![self.root.clone(), current]);
        }
        self.expanded.insert(self.root.clone());
        self.filter_items(cx);
        if let Some(origin) = self.restore_drag_navigation.take() {
            self.selected = origin.selected;
            self.index = origin.index.filter(|index| *index < self.items.len());
            self.anchor = origin.anchor.filter(|index| *index < self.items.len());
            self.list.scroll_to(origin.scroll);
        }
        if !self.pending_drag_selection.is_empty() {
            self.selected = self.pending_drag_selection.drain(..).filter(|id| {
                self.items.iter().any(|item| item["id"].as_str() == Some(id))
            }).collect();
            self.index = self.items.iter().position(|item| self.selected.contains(&text(item, "id")));
            self.anchor = self.index;
            if let Some(index) = self.index {
                if let Some(row) = self.rows.iter().position(|range| range.contains(&index)) {
                    self.list.scroll_to_reveal_item(row);
                }
            }
        }
        cx.notify();
    }

    fn filter_items(&mut self, cx: &mut Context<Self>) {
        if !self.dragging && !self.native_dragging { self.retire_native_interactions(); }
        let query = self.query.trim().to_lowercase();
        let query_changed = self.filtered_query != query;
        let previous = std::mem::take(&mut self.items);
        self.items = self.page["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                query.is_empty()
                    || format!("{}\n{}", text(item, "title"), text(item, "summary"))
                        .to_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect();
        (self.selected, self.index, self.anchor) = panel_selection_after_refresh(
            &previous, &self.items, &self.selected, self.index, self.anchor,
        );
        self.filtered_query = query;
        self.rebuild_rows(cx);
        if query_changed && !self.rows.is_empty() {
            self.list.scroll_to(gpui::ListOffset { item_ix:0, offset_in_item:px(0.0) });
        }
        cx.notify();
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let width = self
            .geometry
            .map(|g| g.width as f32 / g.scale as f32)
            .unwrap_or(PANEL_SIZE.0 as f32);
        self.content_width = (width
            - 2.0 * self.tokens.layout.content_padding_x
            - if self.nav_visible {
                sidebar_width(width) + self.tokens.layout.toolbar_gap
            } else {
                0.0
            }
            - if panel_style(&self.page) == Style::List {
                2.0 * self.tokens.layout.row_inset_x
            } else {
                0.0
            })
        .max(30.0);
        let rows = row_ranges(
            &self.items,
            panel_style(&self.page),
            &self.page["appearance"],
            self.content_width,
        );
        let metrics = json!([format!("{:?}",panel_style(&self.page)), self.content_width,
            self.page["appearance"], self.tokens.layout, self.tokens.typography, self.config["fontFamilyString"]]).to_string();
        let keys = panel_row_keys(&self.items, &rows, &metrics, &self.tree);
        let anchor = panel_refreshed_anchor(&self.row_keys, &keys, self.list.logical_scroll_top());
        if crate::list_reconcile::reconcile(&self.list, &self.row_keys, &keys) {
            self.list.scroll_to(anchor);
        }
        self.rows = rows;
        self.row_keys = keys;
        cx.notify();
    }

    fn navigate(&mut self, root: String, folder: Option<String>, cx: &mut Context<Self>) {
        if !self.current()
            || self.dialog.is_some()
            || self.settings_forms.is_some()
            || root.is_empty()
        {
            return;
        }
        let target = folder.as_deref().unwrap_or(&root);
        if self.path.first() == Some(&root) {
            if let Some(index) = self.path.iter().rposition(|id| id == target) {
                self.path.truncate(index + 1);
            } else if self.path.last().is_some_and(|parent| self.tree["childrenById"][parent]
                .as_array().is_some_and(|children| children.iter().any(|id| id.as_str() == Some(target)))) {
                self.path.push(target.into());
            } else {
                self.path = drag_support::occurrence_path(&self.tree, &root, target).unwrap_or_default();
            }
        } else {
            self.path = drag_support::occurrence_path(&self.tree, &root, target).unwrap_or_default();
        }
        self.root = root;
        self.folder = folder.filter(|folder| folder != &self.root);
        self.query.clear();
        self.reload(cx);
        let lifetime = self.lifetime();
        let owner = self.window;
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if !this.accepts(&lifetime)
                        || !this.query.is_empty()
                        || this.search.read(cx).is_composing()
                    {
                        return;
                    }
                    this.search
                        .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                });
            });
        });
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if self.page["canGoBack"].as_bool() != Some(true) {
            return;
        }
        let parent = self.parent_folder()
            .filter(|p| p != &self.root);
        self.navigate(self.root.clone(), parent, cx);
    }

    fn parent_folder(&self) -> Option<String> {
        if self.path.last().map(String::as_str) == self.page["currentFavorId"].as_str() && self.path.len() > 1 {
            return self.path.get(self.path.len() - 2).cloned();
        }
        self.page["parentFolderId"].as_str().map(str::to_owned)
    }

    fn selected_ids(&self, include_folders: bool) -> Vec<String> {
        self.items
            .iter()
            .filter(|item| include_folders || item["isFolder"].as_bool() != Some(true))
            .filter_map(|item| item["id"].as_str())
            .filter(|id| self.selected.contains(*id))
            .map(str::to_string)
            .collect()
    }

    fn select(&mut self, index: usize, control: bool, shift: bool, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        let id = text(item, "id");
        if shift {
            let anchor = self.anchor.unwrap_or(index);
            self.selected = self.items[anchor.min(index)..=anchor.max(index)]
                .iter()
                .map(|item| text(item, "id"))
                .collect();
        } else if control {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
            self.anchor = Some(index);
        } else {
            self.selected.clear();
            self.selected.insert(id);
            self.anchor = Some(index);
        }
        self.index = Some(index);
        if let Some(row) = self.rows.iter().position(|row| row.contains(&index)) {
            self.list.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    fn navigate_key(&mut self, key: &str, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        if !self.current()
            || self.loading
            || self.dialog.is_some()
            || self.settings_forms.is_some()
            || self.menu_lease.is_some()
            || self.items.is_empty()
        {
            return;
        }
        let last = self.items.len() - 1;
        let next = match key {
            "ArrowDown" | "ArrowRight" | "down" | "right" => {
                Some(self.index.map_or(0, |i| (i + 1).min(last)))
            }
            "ArrowUp" | "ArrowLeft" | "up" | "left" => {
                Some(self.index.map_or(last, |i| i.saturating_sub(1)))
            }
            "Home" | "home" => Some(0),
            "End" | "end" => Some(last),
            "Enter" | "enter" => {
                self.execute_selected(task, cx);
                None
            }
            _ => None,
        };
        if let Some(index) = next {
            self.select(index, false, false, cx);
        }
    }

    fn execute_selected(&mut self, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        let Some(item) = self.index.and_then(|index| self.items.get(index)).cloned() else {
            return;
        };
        self.execute_item(item, task, cx);
    }

    fn execute_item(&mut self, item: Value, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        if self.busy || self.loading || !self.current() {
            return;
        }
        let request = match self.host.capture_native_panel_action(task) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let root = self.root.clone();
        self.run(false, cx, move |host| {
            host.panel_invoke_native(&json!({"favorId":root,"itemId":item["id"]}), &request)
        });
    }

    fn run(
        &mut self,
        refresh: bool,
        cx: &mut Context<Self>,
        action: impl FnOnce(Arc<HostState>) -> Result<Value, String> + Send + 'static,
    ) {
        if !self.action_available() {
            return;
        }
        self.busy = true;
        self.error = None;
        self.operation_generation = self.operation_generation.wrapping_add(1);
        let reply = PanelActionReply {
            operation: self.operation_generation,
            owner_hwnd: self.hwnd,
            presentation: self.presentation,
        };
        let lifetime = self.lifetime();
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action(host) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let accepts_page = this.accepts(&lifetime);
                let snapshot = this.host.panel_native_snapshot();
                let Some(settled) = reply.settle(
                    this.operation_generation,
                    &mut this.picker_guard,
                    &snapshot,
                    accepts_page,
                    result,
                ) else { return; };
                this.busy = false;
                if settled.release_picker_overlay {
                    this.host
                        .apply_window_action_for("setSpaOverlayOpen", false, "panel");
                }
                let Some(result) = settled.result else {
                    cx.notify();
                    return;
                };
                match result {
                    Ok(result) if result["ok"].as_bool() == Some(false) => {
                        this.error = Some(tr("操作未完成", "Operation did not complete").into());
                    }
                    Ok(result) if result["action"].as_str() == Some("navigate") => {
                        if let Some(folder) = result["navigateFolderId"].as_str() {
                            this.navigate(this.root.clone(), Some(folder.into()), cx);
                        }
                    }
                    Ok(_) if refresh => this.reload(cx),
                    Ok(_) => {}
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn paste_selection(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let request = match self.host.capture_native_panel_paste(None) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.run(false, cx, move |host| {
            for id in ids {
                host.favor_paste_panel_native(&json!({"id":id}), &request)?;
            }
            Ok(json!({"ok":true}))
        });
    }

    fn copy_selection(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let request = match self.host.capture_native_panel_action(None) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.run(false, cx, move |host| {
            for id in ids {
                host.favor_copy_panel_native(&json!({"id":id}), &request)?;
            }
            Ok(json!({"ok":true}))
        });
    }

    fn finish_drag(&mut self, cx: &mut Context<Self>) {
        crate::favorite_drag::clear_target_hwnd(self.hwnd);
        if !self.dragging && self.active_content_drag.is_none() {
            return;
        }
        let completed = self.active_content_drag.take().is_some_and(|drag| {
            if drag.is_claimed() { true } else { drag.cancel(); false }
        });
        self.dragging = false;
        self.source_drag_operation = None;
        self.drop_target = None;
        self.drop_candidate = None;
        self.drop_path = None;
        self.drag_hover = None;
        self.drag_scroll = None;
        if let Some(expanded) = self.drag_expanded_before.take() {
            if !completed { self.expanded = expanded; }
        }
        self.suppress_click_until = Some(Instant::now() + Duration::from_millis(300));
        self.host.apply_window_action_for(
            "setSpaOverlayOpen",
            self.dialog.is_some() || self.settings_forms.is_some(),
            "panel",
        );
        if let Some(origin) = self.drag_navigation.take().filter(|_| !completed) {
            self.root = origin.root.clone();
            self.folder = origin.folder.clone();
            self.path = origin.path.clone();
            self.query = origin.query.clone();
            self.pending_drag_search = Some(origin.query.clone());
            self.restore_drag_navigation = Some(origin);
            self.reload(cx);
        }
        cx.notify();
    }

    fn begin_content_drag(&mut self, drag: &ContentDrag, cx: &mut Context<Self>) {
        if !self.current() || self.busy || self.dialog.is_some() || self.settings_forms.is_some()
            || !drag.active()
        {
            return;
        }
        self.cancel_native_hover();
        self.dragging = true;
        self.active_content_drag = Some(drag.clone());
        self.host.apply_window_action_for("setSpaOverlayOpen", true, "panel");
        cx.notify();
    }

    fn can_drag_order(&self) -> bool {
        self.query.trim().is_empty()
            && self.page["canReorderChildren"].as_bool() != Some(false)
            && text(&self.page, "currentFavorId") != ORPHANS
    }

    fn set_drag_target(
        &mut self,
        drag: &ContentDrag,
        target: Option<DropTarget>,
        copy: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.current() || self.busy || self.loading || !drag.active() {
            return;
        }
        self.begin_content_drag(drag, cx);
        self.drop_candidate = target.clone();
        self.drop_path = target.as_ref().and_then(|target| target.parent_id.as_ref()).map(|parent| {
            if let Some(index) = self.path.iter().rposition(|id| id == parent) {
                self.path[..=index].to_vec()
            } else if self.path.last().is_some_and(|current| self.tree["childrenById"][current]
                .as_array().is_some_and(|children| children.iter().any(|child| child.as_str() == Some(parent)))) {
                let mut path = self.path.clone(); path.push(parent.clone()); path
            } else {
                drag_support::occurrence_path(&self.tree, &self.root, parent).unwrap_or_default()
            }
        });
        let target = target.filter(|target| drag_support::valid_target(&self.tree, drag, target, copy));
        if self.drop_target != target {
            self.drop_target = target;
            cx.notify();
        }
    }

    fn offer_content_target(&mut self, drag: &ContentDrag, bounds: gpui::Bounds<gpui::Pixels>, window: &Window) {
        crate::favorite_drag::clear_target(window);
        if !drag.active() || !self.current() || self.busy || self.loading { return; }
        let Some(target) = self.drop_target.clone() else { return; };
        crate::favorite_drag::offer_target(drag, &target, bounds, window, self.host.clone());
        self.native_drag_offer = Some(NativeDragOffer {
            operation_id:drag.operation_id.clone(), target, path:self.drop_path.clone(), lifetime:self.lifetime(),
        });
    }

    fn native_content_saved(&mut self, operation_id: String, result: Result<Value, String>, cx: &mut Context<Self>) {
        let Some(offer) = self.native_drag_offer.take_if(|offer| offer.operation_id == operation_id) else { return; };
        if !self.accepts(&offer.lifetime) { return; }
        self.drag_navigation = None;
        self.finish_drag(cx);
        match result {
            Ok(result) => {
                self.drag_undo = result["undoId"].as_str().map(|id| (id.into(), text(&result, "revision")));
                if self.busy || self.dialog.is_some() || self.settings_forms.is_some() {
                    self.refresh_needed = true;
                    cx.notify();
                    return;
                }
                self.pending_drag_selection = result["selectedIds"].as_array().into_iter().flatten()
                    .filter_map(Value::as_str).map(str::to_owned).collect();
                if let Some(path) = offer.path.filter(|path| !path.is_empty()) {
                    self.root = path[0].clone();
                    self.path = path;
                } else if let Some(root) = offer.target.parent_id.as_deref().and_then(|id| root_for_folder(&self.layout, id)) {
                    self.root = root;
                }
                self.folder = offer.target.parent_id.filter(|id| id != &self.root);
                self.query.clear();
                self.pending_drag_search = Some(String::new());
                self.reload(cx);
            }
            Err(error) => {
                if self.busy || self.dialog.is_some() || self.settings_forms.is_some() { self.refresh_needed = true; }
                else { self.reload(cx); }
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    fn drop_content(
        &mut self,
        drag: &ContentDrag,
        target: Option<DropTarget>,
        copy: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.current() || self.busy || self.loading || !drag.active() {
            self.finish_drag(cx);
            return;
        }
        let Some(target) = target.filter(|target| drag_support::valid_target(&self.tree, drag, target, copy)) else {
            self.finish_drag(cx);
            return;
        };
        if !drag.claim() { return; }
        let accepted = match crate::native_interactions::accept_content_write() {
            Ok(accepted) => accepted,
            Err(error) => {
                self.finish_drag(cx);
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let target_path = self.drop_path.clone();
        self.begin_content_drag(drag, cx);
        self.drag_navigation = None;
        self.finish_drag(cx);
        self.busy = true;
        self.error = None;
        self.operation_generation = self.operation_generation.wrapping_add(1);
        let operation = self.operation_generation;
        let lifetime = self.lifetime();
        let request = drag.request(&target, copy);
        let host = self.host.clone();
        let root = target_path.as_ref().and_then(|path| path.first()).cloned().or_else(|| target.parent_id.as_deref()
            .and_then(|id| root_for_folder(&self.layout, id))
        ).unwrap_or_else(|| self.root.clone());
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                let result = host.favorite_drag_apply(&request);
                accepted.finish(&result);
                result
            }).await;
            let _ = this.update(cx, |this, cx| {
                if this.operation_generation != operation { return; }
                this.busy = false;
                if !this.current() || this.display_generation != lifetime.display
                    || this.presentation != lifetime.presentation { return; }
                match result {
                    Ok(result) => {
                        this.pending_drag_selection = result["selectedIds"].as_array()
                            .into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                        this.drag_undo = result["undoId"].as_str()
                            .map(|id| (id.to_owned(), text(&result, "revision")));
                        this.root = root;
                        if let Some(path) = target_path { this.path = path; }
                        this.folder = target.parent_id.filter(|id| id != &this.root);
                        this.query.clear();
                        this.pending_drag_search = Some(String::new());
                        this.reload(cx);
                    }
                    Err(error) => {
                        this.reload(cx);
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn undo_drag(&mut self, cx: &mut Context<Self>) {
        let Some((undo, revision)) = self.drag_undo.clone() else { return; };
        if !self.current() || self.busy || self.loading { return; }
        let accepted = match crate::native_interactions::accept_content_write() {
            Ok(accepted) => accepted,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.drag_undo = None;
        self.run(true, cx, move |host| {
            let result = host.favorite_drag_undo(&json!({"undoId":undo, "expectedRevision":revision}));
            accepted.finish(&result);
            result
        });
    }

    fn drag_hover_folder(&mut self, id: String, root: String, folder: Option<String>, navigate: bool, bounds: gpui::Bounds<gpui::Pixels>) {
        if self.drag_hover.as_ref().is_some_and(|hover| hover.id == id && hover.navigate == navigate) {
            self.drag_hover.as_mut().unwrap().bounds = bounds;
            return;
        }
        self.drag_hover = Some(DragHover { id, root, folder, since: Instant::now(), navigate, bounds });
    }

    fn maintain_content_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(query) = self.pending_drag_search.take() {
            self.search.update(cx, |input, cx| input.set_text(query, window, cx));
        }
        if !self.dragging { return; }
        if self.active_content_drag.as_ref().is_some_and(|drag| !drag.active()) {
            self.drag_hover = None;
            self.drag_scroll = None;
            return;
        }
        let pointer = drag_support::current_pointer(window);
        if self.drag_scroll.as_ref().is_some_and(|scroll| !scroll.bounds.contains(&pointer)) {
            self.drag_scroll = None;
        }
        if let Some(scroll) = &mut self.drag_scroll {
            let now = Instant::now();
            let step = scroll.velocity * now.duration_since(scroll.last).as_secs_f32().min(0.1);
            scroll.last = now;
            if scroll.sidebar {
                let max = f32::from(self.sidebar_scroll.max_offset().height);
                let offset = self.sidebar_scroll.offset();
                self.sidebar_scroll.set_offset(point(offset.x, px((f32::from(offset.y) - step).clamp(-max, 0.0))));
            } else {
                let max = f32::from(self.list.max_offset_for_scrollbar().height);
                let offset = self.list.scroll_px_offset_for_scrollbar();
                self.list.set_offset_from_scrollbar(point(offset.x, px((f32::from(offset.y) - step).clamp(-max, 0.0))));
            }
            // Scrolling changes which row is under a stationary pointer. Re-hit
            // the target after the current entity/window borrows have ended.
            let owner = self.window;
            cx.defer(move |cx| {
                if !cx.has_active_drag() { return; }
                let _ = owner.update(cx, |_, window, cx| {
                    let position = drag_support::current_pointer(window);
                    let modifiers = window.modifiers();
                    window.dispatch_input(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position, pressed_button:Some(MouseButton::Left), modifiers,
                    }), cx);
                });
            });
            cx.notify();
        }
        if self.drag_hover.as_ref().is_some_and(|hover| !hover.bounds.contains(&pointer)) {
            self.drag_hover = None;
        }
        let Some(hover) = self.drag_hover.clone().filter(|hover| hover.since.elapsed() >= Duration::from_millis(650)) else { return; };
        self.drag_hover = None;
        if hover.navigate {
            if text(&self.page, "currentFavorId") == hover.id || self.loading { return; }
            if self.drag_navigation.is_none() {
                self.drag_navigation = Some(DragNavigation {
                    root: self.root.clone(), folder: self.folder.clone(), query: self.query.clone(),
                    path: self.path.clone(),
                    selected: self.selected.clone(), index: self.index, anchor: self.anchor,
                    scroll: self.list.logical_scroll_top(),
                });
            }
            self.navigate(hover.root, hover.folder, cx);
            self.search.update(cx, |input, cx| input.set_text(String::new(), window, cx));
        } else if !self.expanded.contains(&hover.id) {
            self.drag_expanded_before.get_or_insert_with(|| self.expanded.clone());
            self.expanded.insert(hover.id);
            cx.notify();
        }
    }

    fn set_drag_scroll(&mut self, sidebar: bool, velocity: f32, bounds: gpui::Bounds<gpui::Pixels>) {
        if velocity == 0.0 { self.drag_scroll = None; return; }
        let last = self.drag_scroll.as_ref().filter(|scroll| scroll.sidebar == sidebar)
            .map_or_else(Instant::now, |scroll| scroll.last);
        self.drag_scroll = Some(DragScroll { sidebar, velocity, last, bounds });
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        if let Some(native) = &self.interactions {
            let _ = native.set_menu_open(false);
        }
        self.menu_request_generation = self.menu_request_generation.wrapping_add(1);
        if let Some((lease, _)) = self.menu_lease.take() {
            self.host.release_native_context_menu(lease);
        }
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
    }

    fn open_menu(&mut self, ids: Vec<String>, entries: Vec<MenuEntry>, cx: &mut Context<Self>) {
        let Some(point) = platform::cursor_position() else {
            return;
        };
        self.open_menu_at(ids, entries, point, cx);
    }

    fn open_menu_at(
        &mut self,
        ids: Vec<String>,
        entries: Vec<MenuEntry>,
        point: (i32, i32),
        cx: &mut Context<Self>,
    ) {
        if !self.current()
            || self.loading
            || self.busy
            || self.settings_forms.is_some()
            || self.dialog.is_some()
            || self.search.read(cx).is_composing()
        {
            return;
        }
        self.close_menu(cx);
        let lease = crate::menu::next_lease();
        if !self.host.acquire_native_context_menu(lease, "panel") {
            return;
        }
        self.cancel_native_hover();
        #[cfg(windows)]
        if let Some(native) = &self.interactions {
            let _ = native.set_menu_open(true);
        }
        self.menu_lease = Some((lease, ids));
        let view = cx.weak_entity();
        let on_select: SelectHandler = Rc::new(move |lease, key, cx| {
            let _ = view.update(cx, |this, cx| this.menu_selected(lease, key, cx));
        });
        let view = cx.weak_entity();
        let lifetime = self.lifetime();
        cx.defer(move |cx| {
            let _ = view.update(cx, |this, cx| {
                if !this.accepts(&lifetime)
                    || !this.host.native_context_menu_is_current(lease)
                    || this.menu_lease.as_ref().map(|(id, _)| *id) != Some(lease)
                {
                    return;
                }
                if let Err(error) = ContextMenuPopup::show(
                    &mut this.menu,
                    &this.host,
                    this.hwnd,
                    lease,
                    point,
                    entries,
                    this.tokens.clone(),
                    this.dark,
                    on_select,
                    cx,
                ) {
                    this.close_menu(cx);
                    this.error = Some(error);
                    cx.notify();
                }
            });
        });
    }

    fn item_menu(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(index) = self
            .items
            .iter()
            .position(|item| item["id"].as_str() == Some(&id))
        {
            if !self.selected.contains(&id) {
                self.select(index, false, false, cx);
            }
        }
        let ids = if self.selected.contains(&id) {
            self.selected_ids(true)
        } else {
            vec![id.clone()]
        };
        let mut entries = vec![
            MenuAction::tile("paste", tr("粘贴", "Paste")),
            MenuAction::tile("copy-only", tr("复制", "Copy")),
            MenuAction::danger_tile("delete", tr("移除", "Remove")),
        ];
        if ids.len() == 1 {
            entries.extend([
                MenuEntry::Separator,
                MenuAction::row("rename", tr("编辑", "Edit")),
            ]);
            if self.tree["items"][&id]["filePaths"]
                .as_array()
                .is_some_and(|paths| !paths.is_empty())
            {
                entries.extend([
                    MenuAction::row("open-file", tr("打开", "Open")),
                    MenuAction::row("reveal-file", tr("定位文件", "Reveal file")),
                    MenuAction::row("reveal-folder", tr("定位文件夹", "Reveal folder")),
                ]);
            }
        }
        entries.push(MenuAction::row("move", tr("移动到…", "Move to…")));
        self.open_menu(ids, entries, cx);
    }

    fn shell_menu(&mut self, cx: &mut Context<Self>) {
        if self.root == ORPHANS
            || !self.current()
            || self.loading
            || self.busy
            || self.dialog.is_some()
            || self.settings_forms.is_some()
            || self.search.read(cx).is_composing()
        {
            return;
        }
        let Some(point) = platform::cursor_position() else {
            return;
        };
        self.close_menu(cx);
        let request = self.menu_request_generation;
        let lifetime = self.lifetime();
        let input = input_generation(&self.host.panel_native_snapshot());
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let options = cx
                .background_executor()
                .spawn(async move { host.favor_clipboard_options() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.accepts(&lifetime)
                    || this.menu_request_generation != request
                    || input_generation(&this.host.panel_native_snapshot()) != input
                {
                    return;
                }
                let mut entries = Vec::new();
                for option in options["options"].as_array().into_iter().flatten() {
                    let key = match option["key"].as_str() {
                        Some("action") => "clipboard-action",
                        Some("files") => "clipboard-files",
                        Some("text") => "clipboard-text",
                        _ => continue,
                    };
                    entries.push(MenuAction::row(key, text(option, "label")));
                }
                if !entries.is_empty() {
                    entries.push(MenuEntry::Separator);
                }
                entries.extend([
                    MenuEntry::Label(tr("添加", "Add").into()),
                    MenuAction::tile("add-text", tr("文本", "Text")),
                    MenuAction::tile("new-folder", tr("收藏夹", "Favorite folder")),
                    MenuAction::tile("add-action-hint", tr("动作", "Action")),
                    MenuAction::tile(
                        "add-link",
                        tr("添加当前打开网址", "Add current browser URL"),
                    ),
                    MenuAction::tile("add-files", tr("文件", "Files")),
                    MenuAction::tile("add-directories", tr("文件夹", "Directories")),
                    MenuAction::tile("add-batch-text", tr("批量文本", "Text line by line")),
                    MenuEntry::Separator,
                    MenuAction::row("refresh", tr("刷新", "Refresh")),
                ]);
                if this.drag_undo.is_some() {
                    entries.push(MenuAction::row("undo-drag", tr("撤销上次整理", "Undo last organization")));
                }
                this.open_menu_at(Vec::new(), entries, point, cx);
            });
        })
        .detach();
    }

    fn menu_selected(&mut self, lease: u64, key: &'static str, cx: &mut Context<Self>) {
        let Some((_, ids)) = self.menu_lease.take_if(|(current, _)| *current == lease) else {
            return;
        };
        #[cfg(windows)]
        if let Some(native) = &self.interactions {
            let _ = native.set_menu_open(false);
        }
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
        if !self.host.release_native_context_menu(lease) || !self.current() {
            return;
        }
        let leaves: Vec<_> = ids
            .iter()
            .filter(|id| self.tree["items"][*id]["isFolder"].as_bool() != Some(true))
            .cloned()
            .collect();
        match key {
            "paste" => self.paste_selection(leaves, cx),
            "copy-only" => self.copy_selection(leaves, cx),
            "refresh" => self.reload(cx),
            "undo-drag" => self.undo_drag(cx),
            "clipboard-action" | "clipboard-files" | "clipboard-text" => {
                let parent = text(&self.page, "currentFavorId");
                let option = key.trim_start_matches("clipboard-").to_string();
                self.run(true, cx, move |host| {
                    host.favor_from_clipboard(&json!({"parentId":parent,"optionKey":option}))
                });
            }
            "add-action-hint" => {
                self.error = Some(
                    tr(
                        "请从 Quicker 面板上拖拽添加动作",
                        "Drag an action from the Quicker panel",
                    )
                    .into(),
                )
            }
            "add-clipboard" | "add-link" => {
                let parent = text(&self.page, "currentFavorId");
                self.run(true, cx, move |host| {
                    if key == "add-link" {
                        host.favor_add_link(&json!({"parentId":parent}))
                    } else {
                        host.favor_from_clipboard(&json!({"parentId":parent}))
                    }
                });
            }
            "add-files" | "add-directories" => {
                // Reserve the overlay only when run can accept the picker.
                // A menu can outlive the load/action state it opened under.
                if !self.action_available() {
                    return;
                }
                let request = match self.host.capture_native_picker_window(self.hwnd) {
                    Ok(request) => request,
                    Err(error) => {
                        self.error = Some(error);
                        cx.notify();
                        return;
                    }
                };
                let presentation = self.presentation;
                let parent = text(&self.page, "currentFavorId");
                self.host
                    .apply_window_action_for("setSpaOverlayOpen", true, "panel");
                self.picker_guard = true;
                self.run(true, cx, move |host| {
                    (|| {
                        // The typed request owns HWND/window/runtime checks;
                        // this additional scope is the UI display that accepted
                        // the click, even if Core replaced it before capture.
                        let same_display = || {
                            snapshot_presentation(&host.panel_native_snapshot()) == presentation
                        };
                        let pick = crate::native_dialog::run_blocking(|| {
                            if key == "add-files" {
                                host.pick_files_native_window_guarded(&json!({}), &request, &same_display)
                            } else {
                                host.pick_directories_native_window_guarded(&json!({}), &request, &same_display)
                            }
                        })?;
                        if !host.native_picker_window_is_current(&request)
                            || !same_display()
                            || pick["cancelled"].as_bool() == Some(true)
                        {
                            return Ok(json!({"ok":true}));
                        }
                        host.favor_add_paths(&json!({"parentId":parent,"paths":pick["paths"]}))
                    })()
                });
            }
            "open-file" | "reveal-file" | "reveal-folder" => {
                let Some(path) = ids
                    .first()
                    .and_then(|id| self.tree["items"][id]["filePaths"][0].as_str())
                    .map(str::to_string)
                else {
                    return;
                };
                let path = if key == "reveal-folder" {
                    std::path::Path::new(&path)
                        .parent()
                        .map(|parent| parent.to_string_lossy().to_string())
                        .unwrap_or(path)
                } else {
                    path
                };
                self.run(false, cx, move |host| {
                    if key == "open-file" {
                        host.shell_open_path(&json!({"path":path}))
                    } else {
                        host.shell_reveal(&json!({"path":path}))
                    }
                });
            }
            "rename" | "delete" | "move" | "new-folder" | "new-root-folder" | "add-text"
            | "add-batch-text" => {
                let view = cx.weak_entity();
                let lifetime = self.lifetime();
                let owner = self.window;
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if !this.accepts(&lifetime) {
                                return;
                            }
                            match key {
                                "rename" => {
                                    if let Some(id) = ids.first() {
                                        this.rename(id.clone(), window, cx);
                                    }
                                }
                                "delete" => this.delete(ids, window, cx),
                                "move" => this.open_dialog(
                                    Dialog::Move { ids },
                                    String::new(),
                                    window,
                                    cx,
                                ),
                                "new-folder" | "new-root-folder" => this.open_dialog(
                                    Dialog::AddFolder {
                                        parent: (key == "new-folder").then(|| {
                                            ids.first().cloned().unwrap_or_else(|| {
                                                text(&this.page, "currentFavorId")
                                            })
                                        }),
                                    },
                                    String::new(),
                                    window,
                                    cx,
                                ),
                                _ => this.open_dialog(
                                    Dialog::AddText {
                                        parent: text(&this.page, "currentFavorId"),
                                        batch: key == "add-batch-text",
                                    },
                                    String::new(),
                                    window,
                                    cx,
                                ),
                            }
                        });
                    });
                });
            }
            _ => {}
        }
        cx.notify();
    }

    fn request_focus(&self, window: &mut Window) -> Result<(), String> {
        self.host.chrome_web_action(&json!({"kind":"clipboardHostChrome", "sourceHostWindow":"panel", "requestHostKeyboardFocus":true}));
        if !self.host.host_keyboard_focus_allowed("panel") {
            return Err(tr("面板输入会话已结束", "Panel input session has ended").into());
        }
        let host = self.host.clone();
        let _guard = platform::guard_condition(move || host.host_keyboard_focus_allowed("panel"));
        platform::set_no_activate(window, false)?;
        platform::activate(window)
    }

    fn open_dialog(
        &mut self,
        dialog: Dialog,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_native_hover();
        if !self.current() || self.busy {
            return;
        }
        self.close_menu(cx);
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        self.dialog = Some(dialog);
        self.error = None;
        self.host
            .apply_window_action_for("setSpaOverlayOpen", true, "panel");
        self.name
            .update(cx, |input, cx| input.set_text(initial, window, cx));
        self.body
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.dialog_scroll.set_offset(point(px(0.0), px(0.0)));
        cx.defer_in(window, |this, window, cx| {
            if this.dialog.is_none() || !this.current() {
                return;
            }
            match this.request_focus(window) {
                Ok(()) => {
                    if matches!(this.dialog, Some(Dialog::AddText { .. })) {
                        this.body.read(cx).focus_handle().focus(window);
                    } else {
                        this.name.read(cx).focus_handle().focus(window);
                    }
                }
                Err(error) => this.error = Some(error),
            }
            cx.notify();
        });
        cx.notify();
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy && self.active {
            return;
        }
        self.dialog = None;
        self.name
            .update(cx, |input, cx| input.set_read_only(false, cx));
        self.body
            .update(cx, |input, cx| input.set_read_only(false, cx));
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        self.host
            .apply_window_action_for("setSpaOverlayOpen", false, "panel");
        window.blur();
        self.focus.focus(window);
        cx.notify();
    }

    fn rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        let generation = self.dialog_generation;
        let lifetime = self.lifetime();
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.favor_get(&json!({"id":id})) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.accepts(&lifetime) || this.dialog_generation != generation {
                    return;
                }
                match result {
                    Ok(detail) => this.open_dialog(
                        Dialog::Rename {
                            id: text(&detail, "id"),
                            revision: text(&detail, "revision"),
                        },
                        text(&detail, "title"),
                        window,
                        cx,
                    ),
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn delete(&mut self, ids: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let descendants = ids.iter().map(|id| descendant_count(&self.tree, id)).sum();
        if descendants > 0 {
            self.open_dialog(
                Dialog::Delete { ids, descendants },
                String::new(),
                window,
                cx,
            );
        } else {
            self.run(true, cx, move |host| {
                for id in ids {
                    host.favor_delete(&json!({"id":id}))?;
                }
                Ok(json!({"ok":true}))
            });
        }
    }

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy
            || !self.current()
            || self.name.read(cx).is_composing()
            || self.body.read(cx).is_composing()
        {
            return;
        }
        let title = self.name.read(cx).text().to_string();
        let body = self.body.read(cx).text().to_string();
        let action: Box<dyn FnOnce(Arc<HostState>) -> Result<Value, String> + Send> =
            match self.dialog.clone() {
                Some(Dialog::Rename { id, revision }) => Box::new(move |host| {
                    host.favor_update(&json!({"id":id,"title":title,"expectedRevision":revision}))
                }),
                Some(Dialog::AddFolder { parent }) if !title.trim().is_empty() => {
                    Box::new(move |host| {
                        host.favor_folder_create(&json!({"parentId":parent,"title":title}))
                    })
                }
                Some(Dialog::AddText { parent, batch }) if !body.trim().is_empty() => {
                    Box::new(move |host| {
                        host.favor_add_text_items(
                            &json!({"parentId":parent,"text":body,"lineByLine":batch}),
                        )
                    })
                }
                Some(Dialog::Delete { ids, .. }) => Box::new(move |host| {
                    for id in ids {
                        host.favor_delete(&json!({"id":id}))?;
                    }
                    Ok(json!({"ok":true}))
                }),
                _ => {
                    self.error = Some(tr("请输入内容", "Enter content").into());
                    cx.notify();
                    return;
                }
            };
        self.run_dialog(action, window, cx);
    }

    fn run_dialog(
        &mut self,
        action: impl FnOnce(Arc<HostState>) -> Result<Value, String> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || !self.current() {
            return;
        }
        self.busy = true;
        self.error = None;
        self.operation_generation = self.operation_generation.wrapping_add(1);
        let operation = self.operation_generation;
        let generation = self.dialog_generation;
        let lifetime = self.lifetime();
        let host = self.host.clone();
        let submitted_name = self.name.read(cx).text().to_string();
        let submitted_body = self.body.read(cx).text().to_string();
        self.name
            .update(cx, |input, cx| input.set_read_only(true, cx));
        self.body
            .update(cx, |input, cx| input.set_read_only(true, cx));
        window.blur();
        self.focus.focus(window);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action(host) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if operation != this.operation_generation {
                    return;
                }
                this.busy = false;
                this.name
                    .update(cx, |input, cx| input.set_read_only(false, cx));
                this.body
                    .update(cx, |input, cx| input.set_read_only(false, cx));
                if !this.accepts(&lifetime) || generation != this.dialog_generation {
                    return;
                }
                match result {
                    Ok(result) if result["ok"].as_bool() == Some(false) => {
                        this.error = Some(tr("操作未完成", "Operation did not complete").into())
                    }
                    Ok(result) => {
                        let draft_changed = this.name.read(cx).text() != submitted_name
                            || this.body.read(cx).text() != submitted_body
                            || this.name.read(cx).is_composing()
                            || this.body.read(cx).is_composing();
                        if draft_changed {
                            if let Some(Dialog::Rename { revision, .. }) = &mut this.dialog {
                                if let Some(saved) = result["revision"].as_str() {
                                    *revision = saved.to_string();
                                }
                            }
                            this.refresh_needed = true;
                            this.error = Some(
                                tr(
                                    "已保存提交内容；后续输入尚未保存。",
                                    "Submitted content was saved; later edits are still unsaved.",
                                )
                                .into(),
                            );
                        } else {
                            this.close_dialog(window, cx);
                            this.reload(cx);
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current()
            || self.busy
            || self.dialog.is_some()
            || self.settings_forms.is_some()
            || self.settings_leave_pending
        {
            return;
        }
        self.cancel_native_hover();
        self.close_menu(cx);
        self.host
            .apply_window_action_for("setSpaOverlayOpen", true, "panel");
        if let Err(error) = self.request_focus(window) {
            self.host
                .apply_window_action_for("setSpaOverlayOpen", false, "panel");
            self.error = Some(error);
            cx.notify();
            return;
        }
        let (host, tokens, dark) = (self.host.clone(), self.base_tokens.clone(), self.dark);
        let panel = cx.new(|cx| {
            let mut form = SettingsForm::new(host.clone(), tokens.clone(), dark, None, window, cx);
            form.load("panel", window, cx);
            form
        });
        let global = cx.new(|cx| {
            let mut form = SettingsForm::new(host, tokens, dark, None, window, cx);
            form.load("global", window, cx);
            form
        });
        self.settings_forms = Some((panel, global));
        self.settings_scroll.set_offset(point(px(0.0), px(0.0)));
        self.refresh_window_surfaces(cx);
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_leave_pending {
            return;
        }
        let Some((panel, global)) = &self.settings_forms else {
            return;
        };
        let forms = vec![panel.clone(), global.clone()];
        let ticket = match SettingsForm::prepare_leave(&forms, window, cx) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.pending_local_close = None;
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.settings_leave_pending = true;
        let accepted_lifetime = self.lifetime();
        self.cancel_native_hover();
        cx.spawn_in(window, async move |this, cx| {
            let result = ticket.wait().await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.settings_leave_pending = false;
                let result = result.finish(cx);
                let same_forms = this
                    .settings_forms
                    .as_ref()
                    .is_some_and(|(panel, global)| panel == &forms[0] && global == &forms[1]);
                let intent = this.pending_local_close.take();
                let disposition = settings_leave_disposition(
                    &accepted_lifetime,
                    intent.as_ref(),
                    &this.lifetime(),
                    this.current(),
                    same_forms,
                    result.is_ok(),
                );
                match result {
                    Ok(()) if disposition != SettingsLeaveDisposition::Keep => {
                        this.settings_forms = None;
                        this.host.apply_window_action_for(
                            "setSpaOverlayOpen",
                            this.dialog.is_some(),
                            "panel",
                        );
                        if disposition == SettingsLeaveDisposition::Dismiss {
                            this.host.dismiss_native_window("panel");
                        } else if this.current() {
                            window.blur();
                            this.focus.focus(window);
                            this.reload(cx);
                        }
                    }
                    Ok(()) => {}
                    Err(error) => this.error = Some(error),
                }
                this.sync_native_interactions(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_icon(&mut self, item: &Value, cx: &mut Context<Self>) {
        if self.loading { return; }
        let id = text(item, "id");
        if self.icons.contains_key(&id) {
            return;
        }
        let path = text(item, "icon");
        let image_item = item["kind"].as_str() == Some("ImageClipItem");
        if !image_item && (path.is_empty() || path.starts_with("fa:") || path.starts_with("emoji:"))
        {
            return;
        }
        self.icons.insert(id.clone(), None);
        let revision = panel_icon_revision(item, &self.tree);
        let icon_generation = self.icon_generation;
        let host = self.host.clone();
        let lifetime = self.lifetime();
        cx.spawn(async move |this, cx| {
            let load_id = id.clone();
            let image = cx
                .background_executor()
                .spawn(async move {
                    let data = if image_item {
                        host.favor_image_bytes(&load_id).ok()
                    } else {
                        host.local_image_bytes(path.strip_prefix("image:").unwrap_or(&path))
                    };
                    data.and_then(|(bytes, mime)| {
                        ImageFormat::from_mime_type(&mime)
                            .map(|format| Arc::new(Image::from_bytes(format, bytes)))
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.accepts(&lifetime) || this.icon_generation != icon_generation {
                    return;
                }
                if !this.items.iter().find(|item| item["id"].as_str() == Some(&id))
                    .is_some_and(|item| panel_icon_revision(item, &this.tree) == revision) { return; }
                this.icons.insert(id, image);
                cx.notify();
            });
        })
        .detach();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && crate::favorite_drag::cancel_active(window, cx) {
            self.finish_drag(cx);
            cx.stop_propagation();
            return;
        }
        if self.dragging {
            cx.stop_propagation();
            return;
        }
        if self.dialog.is_some()
            || self.settings_forms.is_some()
            || self.search.read(cx).focus_handle().is_focused(window)
        {
            return;
        }
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.control {
            match key {
                "f" => {
                    if let Err(error) = self.request_focus(window) {
                        self.error = Some(error);
                    } else {
                        self.search.read(cx).focus_handle().focus(window);
                    }
                }
                "a" => {
                    self.selected = self.items.iter().map(|item| text(item, "id")).collect();
                    cx.notify();
                }
                "c" => self.copy_selection(self.selected_ids(false), cx),
                "z" => self.undo_drag(cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.number_of_modifiers() != 0 {
            return;
        }
        if event.is_held && matches!(key, "enter" | "delete") {
            cx.stop_propagation();
            return;
        }
        match key {
            "escape" => self.host.dismiss_native_window("panel"),
            "delete" => self.delete(self.selected_ids(true), window, cx),
            "backspace" => self.back(cx),
            "down" | "right" | "up" | "left" | "home" | "end" | "enter" => {
                self.navigate_key(key, None, cx)
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let hover = color(&t.hover);
        rsx! {
            <div id={id} px={px(8.0)} py={px(4.0)} rounded={px(6.0)} flex_none cursor_pointer />
        }
            .hover(move |style| style.bg(hover))
            .child(label.into())
    }

    fn chrome_button(
        &self,
        id: &'static str,
        icon: &str,
        active: bool,
        tip: &'static str,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let l = &self.tokens.layout;
        let key = SharedString::from(id);
        let background = self.hover_background(&key,
            if active { color(&t.neutral_active) } else { gpui::transparent_black() },
            if active { color(&t.neutral_active) } else { color(&t.hover) }, window);
        let tokens = self.tokens.clone();
        let dark = self.dark;
        rsx! {
            <div id={id} size={px(l.icon_button_size)} flex_none flex items_center justify_center
                rounded={px(l.chip_radius)} bg={background} cursor_pointer />
        }
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hovered.insert(key.clone());
                } else {
                    this.hovered.remove(&key);
                }
                cx.notify();
            }))
            .child(
                svg()
                    .path(self.tokens.icons.get(icon).cloned().unwrap_or_default())
                    .size(px(l.chrome_icon_size))
                    .flex_none()
                    .text_color(color(if active { &t.text } else { &t.text_muted })),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| PanelTooltip { text: tip.into(), tokens: tokens.clone(), dark })
                    .into()
            })
    }

    fn hover_background(
        &self,
        key: &SharedString,
        normal: Hsla,
        hover: Hsla,
        window: &mut Window,
    ) -> Hsla {
        let now = Instant::now();
        let mut motion = self.motion.borrow_mut();
        let amount = motion.value(
            key.clone(),
            if self.hovered.contains(key) { 1.0 } else { 0.0 },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if motion.any_active(now) {
            window.request_animation_frame();
        }
        motion::mix_hsla(normal, hover, amount)
    }

    fn tile(&mut self, index: usize, item: Value, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.load_icon(&item, cx);
        let t = self.tokens.theme(self.dark).clone();
        let style = panel_style(&self.page);
        let appearance = &self.page["appearance"];
        let id = text(&item, "id");
        let hover_lifetime = self.lifetime();
        let hover_session = self.native_session;
        let hover_plan = panel_hover_plan(&item);
        let icon_hover_target = panel_icon_hover_target(&item, &hover_plan);
        #[cfg(windows)]
        let hover_binding =
            self.native_item_hover_binding(index, &id, &hover_lifetime, hover_session, hover_plan);
        #[cfg(not(windows))]
        let hover_binding: Option<hover_binding::HoverBinding> = None;
        let selected = self.selected.contains(&id);
        let folder = item["isFolder"].as_bool() == Some(true);
        let foreground = color(&t.text);
        let font = number(
            appearance,
            if style == Style::Big {
                "bigIconFontSize"
            } else {
                "smallIconFontSize"
            },
            15.0,
        );
        let icon_size = match style {
            Style::Big => (number(appearance, "bigIconWidth", 60.0).min(number(
                appearance,
                "bigIconHeight",
                60.0,
            )) * 0.55)
                .max(16.0),
            Style::Icons => number(appearance, "onlyIconSize", 30.0).max(16.0),
            Style::List => self.tokens.layout.icon_size,
            _ => (font * 1.25).max(16.0),
        };
        let kind = item["kind"].as_str().unwrap_or("");
        let icon_key = if folder {
            if style == Style::List { "folder-closed" } else { "folder" }
        } else if kind.contains("Image") {
            "image"
        } else if kind.contains("File") {
            "file"
        } else if kind.contains("Link") {
            "link"
        } else if kind.contains("Action") || kind.contains("Step") {
            "code"
        } else {
            "text"
        };
        let icon = if let Some(image) = self.icons.get(&id).and_then(Option::as_ref) {
            let element = img(image.clone())
                .size(px(icon_size))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element();
            if let Some(target) = icon_hover_target {
                hover_binding::wrap_hover_thumbnail(
                    element,
                    hover_binding.clone(),
                    target,
                    gpui::ImageSource::Image(image.clone()),
                )
            } else {
                element
            }
        } else {
            svg()
                .path(self.tokens.icons.get(icon_key).cloned().unwrap_or_default())
                .size(px(icon_size))
                .flex_none()
                .text_color(if style == Style::List {
                    color(if folder { &t.text_secondary } else { &t.text_muted })
                } else { foreground })
                .into_any_element()
        };
        let hover_id = id.clone();
        let hover_owner = cx.weak_entity();
        let menu_id = id.clone();
        let select_id = id.clone();
        let target_id = id.clone();
        let drop_id = id.clone();
        let drag = ContentDrag::favorite(
            Some(text(&self.page, "currentFavorId")),
            if self.selected.contains(&id) {
                self.selected_ids(true)
            } else {
                vec![id.clone()]
            },
            (if self.selected.contains(&id) { self.selected_ids(true) } else { vec![id.clone()] })
                .into_iter().map(|id| { let mut path = self.path.clone(); path.push(id); path }).collect(),
            text(&self.tree, "dragRevision"), self.page_generation,
            self.native_session, text(&item, "title"),
        );
        let drag_view = cx.weak_entity();
        let can_drag = text(&self.page, "currentFavorId") != ORPHANS;
        let target_parent = text(&self.page, "currentFavorId");
        let folder_root = self.root.clone();
        let row_colors = surfaces::row_colors(
            self.base_tokens.theme(self.dark),
            &t,
            style,
            self.background.is_some(),
        );
        let row_key = SharedString::from(format!("panel-item-{id}"));
        let hover = row_colors.hover;
        let list_background = self.hover_background(
            &row_key,
            if selected { color(&t.neutral_active) } else { row_colors.normal },
            if selected { color(&t.neutral_active) } else { hover },
            window,
        );
        let width = tile_width(&item, style, appearance, self.content_width);
        let l = &self.tokens.layout;
        let show_divider = style == Style::List
            && index + 1 < self.items.len()
            && !selected
            && !self.hovered.contains(&row_key)
            && !self.items.get(index + 1).is_some_and(|next| {
                let id = text(next, "id");
                self.selected.contains(&id)
                    || self.hovered.contains(&SharedString::from(format!("panel-item-{id}")))
            });
        let mut tile = rsx! {
            <div id={row_key.clone()} relative w={px(width)} flex_none flex items_center
                justify_center />
        }
            .min_h(px(number(appearance, "smallMinHeight", 28.0).max(
                if style == Style::List { l.icon_button_size + 2.0 * l.file_chip_padding_y } else { 30.0 },
            )))
            .px(px(if style == Style::List { l.minimal_row_padding_x } else if style == Style::Chips { 8.0 } else { 10.0 }))
            .py(px(6.0))
            .gap(px(if style == Style::List { l.search_icon_inset } else { 6.0 }))
            .rounded(px(if style == Style::Chips { 24.0 } else { l.row_radius }))
            .when(style != Style::List, |e| e.border_1().border_color(color(&t.border)))
            .bg(if folder {
                row_colors.folder
            } else {
                row_colors.normal
            })
            .text_color(foreground)
            .text_size(px(font))
            .cursor_pointer()
            .overflow_hidden()
            .when(selected && style != Style::List, |e| e.border_color(color(&t.accent)))
            .when(selected && style != Style::List, |e| {
                e.bg(row_colors.selected)
            })
            .when(style == Style::List, |e| e.bg(list_background))
            .when(style != Style::List, |e| e.hover(move |style| style.bg(hover)))
            .when(style == Style::List, |e| e.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered { this.hovered.insert(row_key.clone()); } else { this.hovered.remove(&row_key); }
                cx.notify();
            })))
            .when(style == Style::List || style == Style::Chips, |e| {
                e.justify_start()
            })
            .when(style == Style::Big, |e| {
                e.flex_col()
                    .min_h(px(number(appearance, "bigIconHeight", 60.0)))
                    .px(px(4.0))
            })
            .when(style == Style::Icons, |e| {
                e.h(px(width)).min_h(px(width)).p(px(2.0))
            })
            .child(icon);
        if style != Style::Icons {
            tile = tile.child(
                div()
                    .min_w_0()
                    .when(style != Style::Big, |e| e.flex_1().truncate())
                    .when(style == Style::Big, |e| e.text_center().line_clamp(3))
                    .child(text(&item, "title")),
            );
        }
        if folder && style == Style::List {
            let count = self.tree["items"][&id]["childrenCount"].as_u64();
            tile = tile.child(
                rsx! {
                    <div flex_none flex items_center gap={px(l.toolbar_gap)}
                        text_size={px(self.tokens.typography.small_font_size)}
                        text_color={color(&t.text_muted)} />
                }
                    .when_some(count, |e, count| e.child(count.to_string()))
                    .child(svg().path(self.tokens.icons.get("chevron-right").cloned().unwrap_or_default())
                        .size(px(12.0)).flex_none().text_color(color(&t.text_muted))),
            );
        }
        if show_divider {
            tile = tile.child(rsx! {
                <div absolute bottom_0 left={px(l.minimal_row_padding_x)}
                    right={px(l.minimal_row_padding_x)} h={px(1.0)}
                    bg={color(&t.border).opacity(0.7)} />
            });
        }
        if let Some(target) = &self.drop_target {
            // Into tints the folder (distinct from the selected accent border);
            // only Before/After draw a line (DESIGN 2026-10-15).
            if target.placement == Placement::Into && target.parent_id.as_deref() == Some(&id) {
                tile = tile.child(crate::favorite_drag::into_highlight(
                    if style == Style::Chips { 24.0 } else { l.row_radius },
                    color(&t.accent),
                ));
            } else if target.anchor_id.as_deref() == Some(&id) {
                let marker = div().absolute().bg(color(&t.accent));
                tile = tile.child(if style == Style::List {
                    crate::favorite_drag::insertion_marker(
                        target.placement,
                        l.minimal_row_padding_x - 3.0,
                        l.minimal_row_padding_x,
                        color(&t.accent),
                    )
                } else {
                    marker.top_0().bottom_0().w(px(2.0))
                        .when(target.placement == Placement::Before, |line| line.left_0())
                        .when(target.placement == Placement::After, |line| line.right_0())
                        .into_any_element()
                });
            }
        }
        let tile = tile
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if this.busy || this.dialog.is_some() {
                        return;
                    }
                    window.blur();
                    this.focus.focus(window);
                    let modified = event.modifiers.control
                        || event.modifiers.platform
                        || event.modifiers.shift;
                    if modified || !this.selected.contains(&select_id) {
                        this.select(
                            index,
                            event.modifiers.control || event.modifiers.platform,
                            event.modifiers.shift,
                            cx,
                        );
                    } else {
                        this.index = Some(index);
                        cx.notify();
                    }
                }),
            )
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                cx.stop_propagation();
                if this.native_dragging {
                    return;
                }
                this.cancel_native_hover();
                if this
                    .suppress_click_until
                    .is_some_and(|until| Instant::now() < until)
                {
                    return;
                }
                let modifiers = event.modifiers();
                if !modifiers.control && !modifiers.platform && !modifiers.shift {
                    this.select(index, false, false, cx);
                    this.execute_item(item.clone(), None, cx);
                }
            }))
            .when(can_drag, move |tile| {
                tile.on_drag(drag, move |drag: &ContentDrag, _, window, cx| {
                    let started = drag_view.update(cx, |this, cx| {
                        if this.current() && drag.generation == this.page_generation
                            && drag.session == this.native_session && !this.loading {
                            this.begin_content_drag(drag, cx);
                            this.source_drag_operation = Some(drag.operation_id.clone());
                            return true;
                        }
                        false
                    }).unwrap_or(false);
                    if started { crate::favorite_drag::activate_drag(drag, window, cx); } else { drag.cancel(); }
                    cx.new(|_| drag.clone())
                })
            })
            .on_drag_move(cx.listener(
                move |this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                    if !event.bounds.contains(&event.event.position) { return; }
                    let Some(drag) = event.dragged_item().downcast_ref::<ContentDrag>() else { return; };
                    let (offset, length) = if style == Style::List {
                        (f32::from(event.event.position.y - event.bounds.origin.y), f32::from(event.bounds.size.height))
                    } else {
                        (f32::from(event.event.position.x - event.bounds.origin.x), f32::from(event.bounds.size.width))
                    };
                    let placement = crate::favorite_drag::hit_placement(folder, offset, length, this.can_drag_order());
                    let target = placement.map(|placement| DropTarget {
                        parent_id: Some(if placement == Placement::Into { target_id.clone() } else { target_parent.clone() }),
                        anchor_id: (placement != Placement::Into).then(|| target_id.clone()), placement,
                    });
                    this.set_drag_target(drag, target, event.event.modifiers.control, cx);
                    let bounds = this.drop_target.as_ref().map_or(event.bounds, |target| {
                        drag_support::placement_bounds(event.bounds, folder, style == Style::List, this.can_drag_order(), target.placement)
                    });
                    this.offer_content_target(drag, bounds, window);
                    if folder && this.drop_target.as_ref().is_some_and(|target| target.placement == Placement::Into) {
                        this.drag_hover_folder(target_id.clone(), folder_root.clone(), Some(target_id.clone()), true, event.bounds);
                    } else {
                        this.drag_hover = None;
                    }
                },
            ))
            .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
                let target = this.drop_candidate.clone().filter(|target|
                    target.anchor_id.as_deref() == Some(&drop_id)
                        || (target.placement == Placement::Into && target.parent_id.as_deref() == Some(&drop_id)));
                this.drop_content(drag, target, window.modifiers().control, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.item_menu(menu_id.clone(), cx);
                }),
            )
            .into_any_element();
        hover_binding::wrap_hover_row(
            tile,
            hover_binding,
            Rc::new(move |event, window, cx| {
                let _ = hover_owner.update(cx, |this, cx| {
                    this.hover_native_item(
                        index,
                        &hover_id,
                        &hover_lifetime,
                        hover_session,
                        event,
                        window,
                        cx,
                    )
                });
            }),
        )
    }

    fn virtual_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(range) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let style = panel_style(&self.page);
        let row_id = SharedString::from(format!("panel-row:{}:{}", text(&self.page,"currentFavorId"),
            self.row_keys.get(index).map(|key| serde_json::to_string(&key.members).unwrap_or_default()).unwrap_or_default()));
        let mut row = rsx! {
            <div id={row_id} w_full flex items_start />
        }
            .gap(px(if style == Style::Icons { 4.0 } else { 6.0 }))
            .px(px(if style == Style::List { self.tokens.layout.row_inset_x } else { 0.0 }))
            .pb(px(if style == Style::List { 0.0 } else { 6.0 }));
        for index in range {
            if let Some(item) = self.items.get(index).cloned() {
                row = row.child(self.tile(index, item, window, cx));
            }
        }
        row.into_any_element()
    }

    fn nav_rows(&self) -> Vec<NavRow> {
        self.nav_rows_with(&self.expanded)
    }

    fn nav_rows_with(&self, expanded: &HashSet<String>) -> Vec<NavRow> {
        let mut rows = Vec::new();
        let entries = self.layout["navEntries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if self.layout["mode"].as_str() == Some("multiRoot") {
            for entry in &entries {
                let id = text(entry, "favorItemId");
                let children = entry["children"].as_array().cloned().unwrap_or_default();
                rows.push(NavRow {
                    root: id.clone(),
                    id: id.clone(),
                    title: text(entry, "title"),
                    depth: 0,
                    has_children: !children.is_empty(),
                    parent: None,
                    path: vec![id.clone()],
                });
                if expanded.contains(&id) {
                    flatten_nav(&children, &id, 1, expanded, &mut rows, &[id.clone()]);
                }
            }
        } else {
            let root = text(&self.layout, "defaultFavorItemId");
            rows.push(NavRow {
                root: root.clone(),
                id: root.clone(),
                title: text(&self.layout, "rootTitle"),
                depth: 0,
                has_children: !entries.is_empty(),
                parent: None,
                path: vec![root.clone()],
            });
            if expanded.contains(&root) {
                flatten_nav(&entries, &root, 1, expanded, &mut rows, std::slice::from_ref(&root));
            }
        }
        if self.layout["detachedOrphansCount"].as_u64().unwrap_or(0) > 0 {
            rows.push(NavRow {
                root: ORPHANS.into(),
                id: ORPHANS.into(),
                title: tr("独立收藏", "Detached favorites").into(),
                depth: 0,
                has_children: false,
                parent: None,
                path: vec![ORPHANS.into()],
            });
        }
        rows
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let current = self.folder.as_deref().unwrap_or(&self.root);
        let width = self.geometry.map(|g| g.width as f32 / g.scale as f32).unwrap_or(PANEL_SIZE.0 as f32);
        let mut sidebar = rsx! {
            <div id={"panel-folders"} w={px(sidebar_width(width))} h_full flex_none />
        }
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll)
            .smooth_wheel("panel-folders", &self.sidebar_scroll, cx)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .border_r_1()
            .border_color(color(&t.border))
            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                if !event.bounds.contains(&event.event.position) { return; }
                let Some(drag) = event.dragged_item().downcast_ref::<ContentDrag>() else { return; };
                let velocity = drag_support::edge_velocity(f32::from(event.event.position.y), f32::from(event.bounds.origin.y), f32::from(event.bounds.size.height));
                this.set_drag_scroll(true, velocity, event.bounds);
                this.set_drag_target(drag, Some(DropTarget { parent_id: None, anchor_id: None, placement: Placement::Into }), event.event.modifiers.control, cx);
                this.offer_content_target(drag, event.bounds, window);
            }))
            .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                let target = this.drop_candidate.clone().filter(|target| target.parent_id.is_none() && target.anchor_id.is_none());
                this.drop_content(drag, target, window.modifiers().control, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    if this.root == ORPHANS {
                        return;
                    }
                    this.open_menu(
                        vec![this.root.clone()],
                        vec![
                            MenuAction::tile("new-folder", tr("添加收藏夹", "Add favorite folder")),
                            MenuAction::tile(
                                "new-root-folder",
                                tr("在根节点添加收藏夹", "Add root favorite folder"),
                            ),
                        ],
                        cx,
                    );
                }),
            );
        let rows = self.nav_rows();
        let next_paths: Vec<Vec<String>> = rows.iter().skip(1).map(|row| row.path.clone()).collect();
        for (index, row) in rows.into_iter().enumerate() {
            let expanded = self.expanded.contains(&row.id);
            let first_child = expanded
                .then(|| crate::favorite_drag::first_child_below(&row.path, next_paths.get(index).map(Vec::as_slice)))
                .flatten();
            let indent = 4.0 + row.depth as f32 * 8.0;
            let row_parent = row.parent.clone();
            let active = current == row.id;
            let toggle = row.id.clone();
            let menu_id = row.id.clone();
            let drop_id = row.id.clone();
            let target_id = row.id.clone();
            let target_parent = row.parent.clone();
            let source_id = row.id.clone();
            let source_parent = row.parent.clone();
            let drag_root = row.root.clone();
            let target_path = row.path.clone();
            let drag = ContentDrag::favorite(source_parent, vec![source_id.clone()], vec![row.path.clone()],
                text(&self.tree, "dragRevision"), self.page_generation, self.native_session, row.title.clone());
            let drag_view = cx.weak_entity();
            let row_identity = serde_json::to_string(&row.path).unwrap_or_default();
            let hover = color(&t.hover);
            sidebar = sidebar.child(
                rsx! {
                    <div id={SharedString::from(format!("panel-nav:{row_identity}"))} relative
                        flex items_center min_h={px(self.tokens.layout.icon_button_size)}
                        py={px(2.0)} pl={px(indent)} pr={px(4.0)} gap={px(4.0)} cursor_pointer
                        rounded={px(self.tokens.layout.row_radius)}
                        text_size={px(self.tokens.typography.category_font_size)} />
                }
                    .when(active, |e| {
                        e.bg(color(&t.neutral_active)).text_color(color(&t.text))
                    })
                    .when(!active, |e| e.hover(move |s| s.bg(hover)))
                    // Into tints the folder; lines start at the inserted level's
                    // icon column, matched by parent so shared folders mark once.
                    .when(self.drop_target.as_ref().is_some_and(|target| target.placement == Placement::Into && target.parent_id.as_deref() == Some(&row.id)),
                        |e| e.child(crate::favorite_drag::into_highlight(self.tokens.layout.row_radius, color(&t.accent))))
                    .when_some(self.drop_target.as_ref().filter(|target| target.placement != Placement::Into
                        && target.anchor_id.as_deref() == Some(&row.id) && target.parent_id == row_parent), |e, target| {
                        e.child(crate::favorite_drag::insertion_marker(target.placement, indent - 3.0, 4.0, color(&t.accent)))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("panel-nav-toggle:{row_identity}")))
                            .size(px(self.tokens.layout.icon_size))
                            .flex_none()
                            .child(svg().path(self.tokens.icons.get(if row.has_children && expanded {
                                "folder-open"
                            } else { "folder-closed" }).cloned().unwrap_or_default())
                                .size(px(self.tokens.layout.icon_size)).text_color(color(&t.text_secondary)))
                            .when(row.has_children, |e| e.on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if this.dragging || this.suppress_click_until.is_some_and(|until| Instant::now() < until) { return; }
                                if !this.expanded.remove(&toggle) {
                                    this.expanded.insert(toggle.clone());
                                }
                                cx.notify();
                            }))),
                    )
                    .child(rsx! {
                        <div flex_1 min_w_0 truncate>
                            {row.title}
                        </div>
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.dragging || this.suppress_click_until.is_some_and(|until| Instant::now() < until) { return; }
                        this.path = row.path.clone();
                        this.search
                            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                        this.navigate(row.root.clone(), Some(row.id.clone()), cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if menu_id == ORPHANS {
                                return;
                            }
                            this.open_menu(
                                vec![menu_id.clone()],
                                vec![
                                    MenuAction::row("rename", tr("编辑", "Edit")),
                                    MenuEntry::Separator,
                                    MenuAction::tile(
                                        "new-folder",
                                        tr("添加收藏夹", "Add favorite folder"),
                                    ),
                                    MenuAction::tile(
                                        "new-root-folder",
                                        tr("在根节点添加收藏夹", "Add root favorite folder"),
                                    ),
                                    MenuEntry::Separator,
                                    MenuAction::danger_tile("delete", tr("移除", "Remove")),
                                ],
                                cx,
                            );
                        }),
                    )
                    .when(source_id != ORPHANS, |e| e.on_drag(drag, move |drag: &ContentDrag, _, window, cx| {
                        let started = drag_view.update(cx, |this, cx| {
                            if this.current() && drag.generation == this.page_generation && drag.session == this.native_session && !this.loading {
                                this.begin_content_drag(drag, cx);
                                this.source_drag_operation = Some(drag.operation_id.clone());
                                return true;
                            }
                            false
                        }).unwrap_or(false);
                        if started { crate::favorite_drag::activate_drag(drag, window, cx); } else { drag.cancel(); }
                        cx.new(|_| drag.clone())
                    }))
                    .on_drag_move(cx.listener(move |this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                        if !event.bounds.contains(&event.event.position) { return; }
                        let Some(drag) = event.dragged_item().downcast_ref::<ContentDrag>() else { return; };
                        let placement = crate::favorite_drag::hit_placement(true,
                            f32::from(event.event.position.y - event.bounds.origin.y), f32::from(event.bounds.size.height),
                            this.query.trim().is_empty());
                        let target = placement.map(|placement| crate::favorite_drag::retarget_expanded_edge(DropTarget {
                            parent_id: if placement == Placement::Into { Some(target_id.clone()) } else { target_parent.clone() },
                            anchor_id: (placement != Placement::Into).then(|| target_id.clone()), placement,
                        }, &target_id, first_child.as_deref()));
                        this.set_drag_target(drag, target, event.event.modifiers.control, cx);
                        if let Some(target) = &this.drop_candidate {
                            this.drop_path = Some(if target.parent_id.as_deref() == Some(&target_id) { target_path.clone() }
                                else { target_path[..target_path.len().saturating_sub(1)].to_vec() });
                        }
                        // Offer the zone that was hit; a retargeted lower edge
                        // is still this row's bottom band.
                        let bounds = match (this.drop_target.as_ref(), placement) {
                            (Some(_), Some(placement)) => drag_support::placement_bounds(event.bounds, true, true, this.query.trim().is_empty(), placement),
                            _ => event.bounds,
                        };
                        this.offer_content_target(drag, bounds, window);
                        if this.drop_target.as_ref().is_some_and(|target| target.placement == Placement::Into) {
                            this.drag_hover_folder(target_id.clone(), drag_root.clone(), Some(target_id.clone()), false, event.bounds);
                        } else { this.drag_hover = None; }
                    }))
                    .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
                        // A retargeted lower edge anchors on the first child
                        // with this folder as parent.
                        let target = this.drop_candidate.clone().filter(|target| target.anchor_id.as_deref() == Some(&drop_id)
                            || target.parent_id.as_deref() == Some(&drop_id));
                        this.drop_content(drag, target, window.modifiers().control, cx);
                    })),
            );
        }
        sidebar.into_any_element()
    }

    fn dialog_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.dialog.clone()?;
        let t = self.tokens.theme(self.dark).clone();
        let heading = match dialog {
            Dialog::Rename { .. } => tr("编辑标题", "Edit title"),
            Dialog::AddFolder { .. } => tr("添加收藏夹", "Add favorite folder"),
            Dialog::AddText { batch: true, .. } => tr("批量添加文本", "Add text line by line"),
            Dialog::AddText { .. } => tr("添加文本", "Add text"),
            Dialog::Move { .. } => tr("移动到收藏夹", "Move to favorite folder"),
            Dialog::Delete { .. } => tr("移除收藏夹及其内容", "Remove folders and their contents"),
        };
        let mut card = rsx! {
            <div w_full max_w={px(360.0)} max_h_full p={px(16.0)} rounded={px(10.0)}
                bg={color(&t.surface)} border_1 border_color={color(&t.border)} shadow_lg flex
                flex_col gap={px(12.0)}>
                <div text_size={px(15.0)}>
                    {heading}
                </div>
            </div>
        };
        match dialog {
            Dialog::Rename { .. } | Dialog::AddFolder { .. } => {
                card = card.child(self.name.clone())
            }
            Dialog::AddText { .. } => card = card.child(self.body.clone()),
            Dialog::Delete { descendants, .. } => {
                card = card.child(cliphost_core::trf!(
                    "将同时移除 {descendants} 个子项。此操作不可撤销。",
                    "This also removes {descendants} descendants. This cannot be undone."
                ))
            }
            Dialog::Move { ids } => {
                let mut folders = div()
                    .id("panel-move-folders")
                    .max_h(px(240.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.dialog_scroll)
                    .smooth_wheel("panel-move-folders", &self.dialog_scroll, cx)
                    .flex()
                    .flex_col()
                    .gap(px(2.0));
                let expanded = self.tree["items"]
                    .as_object()
                    .into_iter()
                    .flat_map(|items| items.iter())
                    .filter(|(_, item)| item["isFolder"].as_bool() == Some(true))
                    .map(|(id, _)| id.clone())
                    .collect();
                for row in self
                    .nav_rows_with(&expanded)
                    .into_iter()
                    .filter(|row| row.id != ORPHANS && !ids.contains(&row.id))
                {
                    let title = row.title;
                    let target = row.id;
                    let moved = ids.clone();
                    let hover = color(&t.hover);
                    folders = folders.child(
                        rsx! {
                            <div id={SharedString::from(format!("panel-move-{target}"))}
                                pl={px(6.0 + row.depth as f32 * 12.0)} py={px(4.0)}
                                cursor_pointer />
                        }
                            .hover(move |s| s.bg(hover))
                            .child(title)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let (ids, target) = (moved.clone(), target.clone());
                                this.run_dialog(
                                    move |host| {
                                        for id in ids {
                                            host.panel_move(
                                                &json!({"itemId":id,"targetFolderId":target}),
                                            )?;
                                        }
                                        Ok(json!({"ok":true}))
                                    },
                                    window,
                                    cx,
                                );
                            })),
                    );
                }
                card = card.child(folders);
            }
        }
        if let Some(error) = &self.error {
            card = card.child(div().text_color(rgb(0xc43d47)).child(error.clone()));
        }
        let mut actions = div().flex().justify_end().gap(px(8.0));
        actions = actions.child(
            self.button("panel-dialog-cancel", tr("取消", "Cancel"), &t)
                .on_click(cx.listener(|this, _, window, cx| {
                    if !this.busy {
                        this.close_dialog(window, cx);
                    }
                })),
        );
        if !matches!(self.dialog, Some(Dialog::Move { .. })) {
            actions = actions.child(
                self.button(
                    "panel-dialog-submit",
                    if self.busy {
                        tr("保存中…", "Saving…")
                    } else {
                        tr("确定", "Confirm")
                    },
                    &t,
                )
                .on_click(cx.listener(|this, _, window, cx| this.submit_dialog(window, cx))),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .p(px(16.0))
                .bg(rgba(0x00000070))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(card.child(actions))
                .into_any_element(),
        )
    }
}

fn descendant_count(tree: &Value, id: &str) -> usize {
    fn collect(tree: &Value, id: &str, seen: &mut HashSet<String>) {
        for child in tree["items"][id]["children"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if seen.insert(child.to_string()) {
                collect(tree, child, seen);
            }
        }
    }
    let mut seen = HashSet::from([id.to_string()]);
    collect(tree, id, &mut seen);
    seen.len().saturating_sub(1)
}

fn input_colors(tokens: &DesignTokens, dark: bool) -> InputColors {
    let t = tokens.theme(dark);
    InputColors {
        background: color(&t.search_background),
        foreground: color(&t.text),
        placeholder: color(&t.text_muted),
        border: color(&t.border_secondary),
        selection: color(&t.tab_active),
        caret: color(&t.accent),
    }
}

fn search_colors(tokens: &DesignTokens, dark: bool) -> InputColors {
    InputColors {
        background: gpui::transparent_black(),
        ..input_colors(tokens, dark)
    }
}

impl Drop for PanelWindow {
    fn drop(&mut self) {
        self.cancel_native_hover();
        #[cfg(windows)]
        {
            self.interactions = None;
        }
        if self.host.panel_native_snapshot()["hwnd"]
            .as_i64()
            .unwrap_or(0) as isize
            == self.hwnd
        {
            self.host.retire_window_presentation("panel");
            self.host.set_panel_web_view_ready(false);
            self.host.set_panel_hwnd(0);
        }
        if let Some((lease, _)) = self.menu_lease {
            self.host.release_native_context_menu(lease);
        }
    }
}

impl Render for PanelWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::favorite_drag::register_surface(window, self.host.clone(), "panel", cx);
        self.sync_native_interactions(cx);
        let t = self.tokens.theme(self.dark).clone();
        let l = &self.tokens.layout;
        let title = if self.settings_forms.is_some() {
            tr("面板设置", "Panel settings").to_string()
        } else {
            self.page["title"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(tr("超级面板", "Favorite panel"))
                .to_string()
        };
        window.set_window_title(&title);
        let pinned = self.host.panel_window_state_dto()["isPinned"]
            .as_bool()
            .unwrap_or(false);
        let titlebar = rsx! {
            <div h={px(l.title_bar_height)} flex_none px={px(l.title_bar_padding_x)}
                gap={px(l.title_bar_gap)} flex items_center border_b_1
                border_color={color(&t.border)} bg={color(&t.surface)} />
        }
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if event.click_count == 1 {
                        if let Err(error) = platform::begin_host_move(window, &this.host, "panel") {
                            this.error = Some(error);
                            cx.notify();
                        }
                    }
                }),
            )
            .child(svg().path(self.tokens.icons.get("folder-open").cloned().unwrap_or_default())
                .size(px(l.chrome_icon_size)).flex_none().text_color(color(&t.text_muted)))
            .child(rsx! {
                <div flex_1 min_w_0 truncate text_size={px(self.tokens.typography.font_size)}>
                    {title}
                </div>
            })
            .child(
                div().flex().gap(px(l.chrome_button_gap))
                    .child(
                        self.chrome_button("panel-pin", "pin", pinned,
                            if pinned { tr("取消固定", "Unpin") } else { tr("固定", "Pin") }, window, cx)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_native_hover();
                            let pinned = this.host.panel_window_state_dto()["isPinned"].as_bool().unwrap_or(false);
                            this.host.apply_window_action_for("setPinned", !pinned, "panel");
                            cx.notify();
                        })),
                    )
                    .child(
                        self.chrome_button("panel-settings", "gear", self.settings_forms.is_some(), tr("面板设置", "Panel settings"), window, cx)
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                    )
                    .child(
                        self.chrome_button("panel-close", "close", false, tr("关闭", "Close"), window, cx)
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, cx| this.request_local_close(window, cx))),
                    ),
            );
        let search_focused = self.search.read(cx).focus_handle().is_focused(window);
        let search = rsx! {
            <div h={px(l.toolbar_height)} px={px(l.content_padding_x + 2.0)}
                gap={px(l.toolbar_gap)} flex items_center flex_none />
        }
            .child(
                self.chrome_button("panel-back", "chevron-left", false, tr("上一级", "Parent folder"), window, cx)
                    .when(self.page["canGoBack"].as_bool() != Some(true), |button| button.opacity(0.35))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.dragging && !this.suppress_click_until.is_some_and(|until| Instant::now() < until) { this.back(cx); }
                    }))
                    .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                        if !event.bounds.contains(&event.event.position) || this.page["canGoBack"].as_bool() != Some(true) { return; }
                        let Some(drag) = event.dragged_item().downcast_ref::<ContentDrag>() else { return; };
                        let Some(parent) = this.parent_folder().filter(|parent| !parent.is_empty()) else { return; };
                        this.set_drag_target(drag, Some(DropTarget { parent_id:Some(parent.clone()), anchor_id:None, placement:Placement::Into }), event.event.modifiers.control, cx);
                        this.offer_content_target(drag, event.bounds, window);
                        if this.drop_target.is_some() {
                            this.drag_hover_folder(parent.clone(), this.root.clone(), Some(parent), true, event.bounds);
                        }
                    }))
                    .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                        let parent = this.parent_folder();
                        let target = this.drop_candidate.clone().filter(|target| target.placement == Placement::Into && target.parent_id == parent);
                        this.drop_content(drag, target, window.modifiers().control, cx);
                    })),
            )
            .child(
                rsx! {
                    <div flex_1 min_w_0 h={px(l.search_height)} flex items_center
                        gap={px(l.toolbar_gap)} pl={px(l.search_icon_inset)} pr={px(2.0)}
                        rounded={px(l.search_radius)} overflow_hidden bg={color(&t.secondary)}
                        border_1 />
                }
                    .border_color(if search_focused { color(&t.border_secondary) } else { gpui::transparent_black() })
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                        if let Err(error) = this.request_focus(window) {
                            this.error = Some(error);
                            cx.notify();
                        }
                    }))
                    .child(svg().path(self.tokens.icons.get("search").cloned().unwrap_or_default())
                        .size(px(l.search_icon_size)).flex_none().text_color(color(&t.text_muted)))
                    .child(self.search.clone()),
            );
        let footer = rsx! {
            <div h={px(l.footer_height)} flex_none flex items_center
                gap={px(l.chrome_button_gap)} px={px(l.content_padding_x)} border_t_1
                border_color={color(&t.border)}
                text_size={px(self.tokens.typography.small_font_size)}
                text_color={color(&t.text_muted)} />
        }
            .child(
                self.chrome_button("panel-nav-toggle", "layout-sidebar", self.nav_visible,
                    if self.nav_visible { tr("隐藏目录栏", "Hide folders") } else { tr("显示目录栏", "Show folders") }, window, cx)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.nav_visible = !this.nav_visible;
                        this.rebuild_rows(cx);
                    })),
            )
            .child(div().pl(px(l.chip_gap)).child(cliphost_core::trf!("{} 项", "{} items", self.items.len())))
            .child(div().flex_1().min_w_0().truncate().child(if self.busy {
                tr("正在执行…", "Working…")
            } else if self.loading { tr("正在加载…", "Loading…") } else { "" }))
            .when(self.drag_undo.is_some(), |footer| footer.child(
                self.chrome_button("panel-drag-undo", "refresh", false, tr("撤销上次整理", "Undo last organization"), window, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.undo_drag(cx))),
            ))
            .child(self.chrome_button("panel-add", "plus", false, tr("添加", "Add"), window, cx)
                .on_click(cx.listener(|this, _, _, cx| this.shell_menu(cx))))
            .child(self.chrome_button("panel-refresh", "refresh", false, tr("刷新", "Refresh"), window, cx)
                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))));
        let view = cx.weak_entity();
        let virtual_list = list(self.list.clone(), move |index, window, cx| {
            view.update(cx, |this, cx| this.virtual_row(index, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h_0()
        .w_full();
        let mut body = rsx! {
            <div relative flex_1 min_h_0 flex gap={px(l.toolbar_gap)}
                px={px(l.content_padding_x)} pb={px(l.content_padding_y)} />
        };
        if self.nav_visible {
            body = body.child(self.sidebar(cx));
        }
        let mut content = rsx! {
            <div id={"panel-contents"} flex_1 min_w_0 min_h_0 flex flex_col gap={px(4.0)} />
        }
            .when(self.drop_target.as_ref().is_some_and(|target| target.placement == Placement::Into
                && target.parent_id.as_deref() == self.page["currentFavorId"].as_str()),
                |content| content.bg(color(&t.neutral_active).opacity(0.35)))
            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                if !event.bounds.contains(&event.event.position) { return; }
                let Some(drag) = event.dragged_item().downcast_ref::<ContentDrag>() else { return; };
                let viewport = this.list.viewport_bounds();
                let velocity = drag_support::edge_velocity(f32::from(event.event.position.y), f32::from(viewport.origin.y), f32::from(viewport.size.height));
                this.set_drag_scroll(false, velocity, viewport);
                this.set_drag_target(drag, Some(DropTarget { parent_id:Some(text(&this.page, "currentFavorId")), anchor_id:None, placement:Placement::Into }), event.event.modifiers.control, cx);
                this.offer_content_target(drag, event.bounds, window);
            }))
            .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                let parent = text(&this.page, "currentFavorId");
                let target = this.drop_candidate.clone().filter(|target| target.placement == Placement::Into && target.parent_id.as_deref() == Some(&parent));
                this.drop_content(drag, target, window.modifiers().control, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.shell_menu(cx);
                }),
            );
        if self.selected.len() > 1 {
            content = content.child(
                rsx! {
                    <div flex flex_wrap gap={px(4.0)} flex_none />
                }
                    .child(
                        self.button("panel-copy-selected", tr("复制所选", "Copy selected"), &t)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.copy_selection(this.selected_ids(false), cx)
                            })),
                    )
                    .child(
                        self.button("panel-paste-selected", tr("粘贴所选", "Paste selected"), &t)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.paste_selection(this.selected_ids(false), cx)
                            })),
                    )
                    .child(
                        self.button(
                            "panel-delete-selected",
                            tr("移除所选", "Remove selected"),
                            &t,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.delete(this.selected_ids(true), window, cx)
                        })),
                    ),
            );
        }
        if let Some(error) = &self.error {
            if self.dialog.is_none() {
                content = content.child(
                    rsx! {
                        <div flex_none text_color={rgb(0xc43d47)}>
                            {error.clone()}
                        </div>
                    },
                );
            }
        }
        if self.dragging {
            let destination = self.drop_target.as_ref().map(|target| {
                let parent = target.parent_id.as_deref().unwrap_or("");
                let name = self.tree["items"][parent]["title"].as_str().unwrap_or(tr("顶层", "Top level"));
                let verb = if self.active_content_drag.as_ref().is_some_and(|drag| matches!(drag.source, DragSource::History { .. })) {
                    tr("收藏到", "Save favorite in")
                } else if window.modifiers().control { tr("复制到", "Copy to") } else { tr("移动到", "Move to") };
                format!("{verb} {name}")
            }).unwrap_or_else(|| tr("此处不能放置；Esc 取消。切回原有顺序后可调整位置", "Cannot drop here; Esc cancels. Return to original order to reorder").into());
            content = content.child(rsx! {
                <div flex_none text_size={px(self.tokens.typography.small_font_size)}
                    text_color={color(&t.text_muted)}>
                    {destination}
                </div>
            });
        }
        if !self.loading && self.items.is_empty() {
            content = content.child(
                rsx! {
                    <div flex_1 flex items_center justify_center
                        text_color={color(&t.text_muted)} />
                }
                    .child(if self.query.is_empty() {
                        tr("暂无收藏项", "No favorites")
                    } else {
                        tr("没有匹配项", "No matches")
                    }),
            );
        } else {
            content = content.child(virtual_list);
        }
        body = body.child(content);
        let background = color(&self.base_tokens.theme(self.dark).background);
        let foreground = color(&t.text);
        let font = self.config["fontFamilyString"]
            .as_str()
            .filter(|font| !font.trim().is_empty())
            .unwrap_or(&self.tokens.typography.font_family)
            .to_string();
        let mut root = div()
            .relative()
            .size_full()
            .when(self.native_drag_over.is_some(), |root| {
                root.border_2().border_color(color(&t.accent))
            })
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && crate::favorite_drag::active(cx) {
                    crate::favorite_drag::cancel_active(window, cx);
                    this.finish_drag(cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.cancel_native_hover()))
            .on_drag_move(cx.listener(
                |this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                    crate::favorite_drag::clear_target(window);
                    if crate::favorite_drag::route_drag_move(event, window, cx) { return; }
                    if !event.bounds.contains(&event.event.position) {
                        this.drag_hover = None;
                        this.drag_scroll = None;
                        this.drop_target = None;
                        this.drop_candidate = None;
                    }
                    this.drag_outside(event, window, cx)
                },
            ))
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.cancel_native_hover();
                this.key_down(event, window, cx)
            }))
            .bg(background)
            .text_color(foreground)
            .text_size(px(self.tokens.typography.font_size))
            .font_family(SharedString::from(font));
        root = root.child(crate::favorite_drag::surface_events());
        if let Some(image) = &self.background {
            root = root
                .child(
                    crate::window_background::render_background_image(
                        image,
                        &self.background_options,
                    ),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(crate::window_glass::page_tint(
                            self.base_tokens.theme(self.dark),
                            &self.background_options,
                        )),
                );
        }
        root = root.child(titlebar);
        if let Some((panel, global)) = &self.settings_forms {
            root = root
                .child(
                    div().px(px(8.0)).py(px(4.0)).flex_none().child(
                        self.button("panel-settings-back", tr("返回面板", "Back to panel"), &t)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_settings(window, cx)),
                            ),
                    ),
                )
                .child(
                    div()
                        .id("panel-settings-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.settings_scroll)
                        .smooth_wheel("panel-settings-scroll", &self.settings_scroll, cx)
                        .p(px(8.0))
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(panel.clone())
                        .child(global.clone()),
                );
        } else {
            root = root.child(search).child(body).child(footer);
        }
        if let Some(dialog) = self.dialog_view(cx) {
            root = root.child(dialog);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_picker_admission_rejects_busy_loading_and_retired_displays() {
        let rejected_states = [
            (false, false, false),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ];
        for (busy, loading, current) in rejected_states {
            assert!(
                !action_available(busy, loading, current),
                "a rejected picker must not acquire its overlay: busy={busy}, loading={loading}, current={current}",
            );
        }
        assert!(action_available(false, false, true));
    }

    #[test]
    fn panel_picker_settles_its_overlay_after_navigation_for_every_selection_outcome() {
        let reply = PanelActionReply { operation: 7, owner_hwnd: 17, presentation: 3 };
        let snapshot = json!({"hwnd":17,"presentationGeneration":3});
        for result in [
            Ok(json!({"paths":["selected.txt"],"cancelled":false})),
            Ok(json!({"paths":[],"cancelled":true})),
            Err("picker failed".into()),
        ] {
            let mut picker_guard = true;
            // Sidebar navigation increments only the UI page generation. Its
            // reply is obsolete, but the running picker still owns this guard.
            let settled = reply.settle(7, &mut picker_guard, &snapshot, false, result.clone()).unwrap();
            assert!(settled.release_picker_overlay, "{result:?}");
            assert!(!picker_guard);
            assert!(settled.result.is_none(), "navigation must discard the old page reply");

            let mut picker_guard = true;
            let settled = reply.settle(7, &mut picker_guard, &snapshot, true, result.clone()).unwrap();
            assert!(settled.release_picker_overlay);
            assert_eq!(settled.result, Some(result), "a current page retains success, cancel and errors");
        }
    }

    #[test]
    fn panel_picker_settlement_cannot_release_a_later_operation_or_core_display() {
        let reply = PanelActionReply { operation: 7, owner_hwnd: 17, presentation: 3 };
        let original = json!({"hwnd":17,"presentationGeneration":3});
        let mut new_operation_guard = true;
        assert!(reply.settle(8, &mut new_operation_guard, &original, true, Ok(json!({}))).is_none());
        assert!(new_operation_guard, "an old callback cannot consume a newer operation's guard");

        // Core can replace its display before the UI processes the new Show.
        // The local operation still matches, so owner/presentation must also
        // protect the new Core overlay from this older callback.
        for replaced in [
            json!({"hwnd":18,"presentationGeneration":3}),
            json!({"hwnd":17,"presentationGeneration":4}),
        ] {
            let mut old_local_guard = true;
            let settled = reply.settle(7, &mut old_local_guard, &replaced, false, Err("picker retired".into())).unwrap();
            assert!(!settled.release_picker_overlay, "{replaced}");
            assert!(!old_local_guard, "discard the completed operation's stale local ownership");
            assert!(settled.result.is_none());
        }
    }

    #[test]
    fn panel_delete_reuses_surviving_rows_and_keeps_nearby_focus_and_reading_position() {
        let old: Vec<_> = ["a","b","c","d"].iter().map(|id| json!({"id":id,"title":id})).collect();
        let new = vec![old[0].clone(),old[2].clone(),old[3].clone()];
        let old_keys = panel_row_keys(&old, &row_ranges(&old,Style::List,&json!({}),300.0), "list", &Value::Null);
        let new_keys = panel_row_keys(&new, &row_ranges(&new,Style::List,&json!({}),300.0), "list", &Value::Null);
        assert_eq!(old_keys[2],new_keys[1]);
        assert_eq!(old_keys[3],new_keys[2]);
        let list = ListState::new(old_keys.len(),ListAlignment::Top,px(100.0));
        let previous = gpui::ListOffset { item_ix:3,offset_in_item:px(13.0) };
        list.scroll_to(previous.clone());
        let mapped = panel_refreshed_anchor(&old_keys,&new_keys,previous);
        crate::list_reconcile::reconcile(&list,&old_keys,&new_keys);
        list.scroll_to(mapped);
        assert_eq!(list.item_count(),3);
        assert_eq!(list.logical_scroll_top().item_ix,2);
        assert_eq!(list.logical_scroll_top().offset_in_item,px(13.0));
        let (selection,focus,anchor) = panel_selection_after_refresh(&old,&new,&HashSet::from(["b".into()]),Some(1),Some(1));
        assert_eq!(selection,HashSet::from(["c".into()]));
        assert_eq!((focus,anchor),(Some(1),Some(1)));
    }

    #[test]
    fn panel_grid_regrouping_tracks_members_and_only_content_changes_invalidate_their_rows() {
        let old: Vec<_> = ["a","b","c","d","e"].iter().map(|id| json!({"id":id,"title":id})).collect();
        let new = old[1..].to_vec();
        let old_keys = panel_row_keys(&old,&[0..2,2..4,4..5],"grid",&Value::Null);
        let new_keys = panel_row_keys(&new,&[0..2,2..4],"grid",&Value::Null);
        let anchor = panel_refreshed_anchor(&old_keys,&new_keys,gpui::ListOffset { item_ix:1,offset_in_item:px(7.0) });
        assert_eq!(anchor.item_ix,0, "the row containing old top member c remains visible");
        assert_eq!(anchor.offset_in_item,px(7.0));
        let mut edited = new.clone();
        edited[0]["title"] = json!("changed content");
        let edited_keys = panel_row_keys(&edited,&[0..2,2..4],"grid",&Value::Null);
        assert_ne!(new_keys[0],edited_keys[0]);
        assert_eq!(new_keys[1],edited_keys[1]);
        assert_eq!(new_keys[0].members,edited_keys[0].members,"render identity remains stable after editing");
    }

    #[test]
    fn panel_hover_preserves_the_row_and_matches_only_the_painted_payload_icon() {
        use crate::hover::{Content, HoverThumbnailTarget};
        let mut item = json!({
            "id":"original-favorite", "kind":"QQRichTextItem", "title":"QQ row",
            "icon":"image:C:\\second.gif", "imRichSegments":[
                {"kind":"text", "text":"before"},
                {"kind":"image", "path":"C:\\first.png"},
                {"kind":"file", "path":"C:\\file.txt", "original":{"keep":true}},
                {"kind":"image", "path":"C:\\second.gif"}
            ]
        });
        let plan = panel_hover_plan(&item);
        assert_eq!(plan.row, Content::FavoriteText("original-favorite".into()));
        assert_eq!(
            plan.im_rich_segments,
            item["imRichSegments"].as_array().unwrap().clone()
        );
        assert_eq!(
            panel_icon_hover_target(&item, &plan),
            Some(HoverThumbnailTarget::ImImage(3))
        );
        item["icon"] = json!("image:C:\\unrelated-user-icon.png");
        assert_eq!(panel_icon_hover_target(&item, &plan), None);
        item["icon"] = json!("fa:Light_Image");
        assert_eq!(panel_icon_hover_target(&item, &plan), None);
        item["isFolder"] = json!(true);
        item["icon"] = json!("image:C:\\second.gif");
        assert_eq!(panel_icon_hover_target(&item, &plan), None);
    }

    #[test]
    fn compact_panel_dto_has_no_invented_images_and_file_icon_keeps_original_index() {
        use crate::hover::{Content, HoverThumbnailTarget};
        let compact =
            json!({"id":"qq", "kind":"QQRichTextItem", "title":"QQ", "icon":"fa:Light_Text"});
        let plan = panel_hover_plan(&compact);
        assert_eq!(plan.row, Content::FavoriteText("qq".into()));
        assert!(plan.thumbnails.is_empty());
        assert_eq!(panel_icon_hover_target(&compact, &plan), None);

        let files = json!({"id":"files", "kind":"FileClipItem", "icon":"image:C:\\b.gif",
            "filePaths":["C:\\text.txt", "C:\\a.png", "C:\\b.gif"]});
        assert_eq!(
            panel_icon_hover_target(&files, &panel_hover_plan(&files)),
            Some(HoverThumbnailTarget::FileImage(2))
        );
        let image = json!({"id":"image", "kind":"ImageClipItem", "icon":"fa:Light_Image"});
        assert_eq!(
            panel_icon_hover_target(&image, &panel_hover_plan(&image)),
            Some(HoverThumbnailTarget::ItemImage)
        );
    }

    #[test]
    fn local_close_waits_for_success_and_keeps_failed_or_replaced_settings() {
        let current = Lifetime {
            display: 1,
            page: 2,
            presentation: 3,
        };
        assert_eq!(
            settings_leave_disposition(&current, Some(&current), &current, true, true, false),
            SettingsLeaveDisposition::Keep
        );
        assert_eq!(
            settings_leave_disposition(&current, Some(&current), &current, true, false, true),
            SettingsLeaveDisposition::Keep
        );
        assert_eq!(
            settings_leave_disposition(&current, Some(&current), &current, true, true, true),
            SettingsLeaveDisposition::Dismiss
        );
        assert_eq!(
            settings_leave_disposition(&current, None, &current, true, true, true),
            SettingsLeaveDisposition::Back
        );
    }

    #[test]
    fn external_hide_and_show_cannot_turn_old_settings_save_into_a_new_close() {
        let accepted = Lifetime {
            display: 1,
            page: 2,
            presentation: 3,
        };
        let reopened = Lifetime {
            display: 3,
            page: 4,
            presentation: 5,
        };
        assert_eq!(
            settings_leave_disposition(&accepted, Some(&accepted), &accepted, false, true, true),
            SettingsLeaveDisposition::Keep
        );
        assert_eq!(
            settings_leave_disposition(&accepted, Some(&accepted), &reopened, true, true, true),
            SettingsLeaveDisposition::Keep
        );
        assert_eq!(
            settings_leave_disposition(&accepted, None, &reopened, true, true, true),
            SettingsLeaveDisposition::Keep
        );
        // A new explicit X may await the original locked forms without
        // duplicating the batch, then dismiss precisely that new display.
        assert_eq!(
            settings_leave_disposition(&accepted, Some(&reopened), &reopened, true, true, true),
            SettingsLeaveDisposition::Dismiss
        );
    }

    #[test]
    fn external_panel_route_decodes_ids_without_losing_folder_selection() {
        assert_eq!(
            route_selection("/panel?favorId=root%2Bid&folderId=%E5%AD%90%E5%A4%B9&embed=1"),
            ("root+id".into(), Some("子夹".into()))
        );
        assert_eq!(decode_parameter("%GG"), None);
    }

    #[test]
    fn virtual_rows_keep_original_order_and_fit_each_style() {
        let items: Vec<_> = (0..1000)
            .map(|i| json!({"id":i.to_string(),"title":"收藏项"}))
            .collect();
        for style in [Style::Big, Style::List, Style::Chips, Style::Icons] {
            let rows = row_ranges(&items, style, &json!({}), 260.0);
            let indexes: Vec<_> = rows.iter().flat_map(Clone::clone).collect();
            assert_eq!(indexes, (0..1000).collect::<Vec<_>>());
            for row in rows {
                assert!(!row.is_empty());
            }
        }
    }

    #[test]
    fn root_resolution_preserves_nested_directory_and_orphan_is_independent() {
        let layout = json!({"mode":"multiRoot","defaultFavorItemId":"a","navEntries":[
            {"favorItemId":"a","children":[{"favorItemId":"nested","children":[]}]},
            {"favorItemId":"b","children":[]}]});
        assert_eq!(root_for_folder(&layout, "nested"), Some("a".into()));
        assert_eq!(root_for_folder(&layout, ORPHANS), None);
    }

    #[test]
    fn sidebar_drag_sources_keep_each_visible_occurrences_actual_parent() {
        let entries = json!([
            {"favorItemId":"a","title":"A","children":[{"favorItemId":"shared","children":[]}]},
            {"favorItemId":"b","title":"B","children":[{"favorItemId":"shared","children":[]}]}
        ]);
        let expanded = HashSet::from(["a".into(), "b".into()]);
        let mut rows = Vec::new();
        flatten_nav(entries.as_array().unwrap(), "root", 1, &expanded, &mut rows, &["root".into()]);
        let shared: Vec<_> = rows.iter().filter(|row| row.id == "shared").collect();
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0].parent.as_deref(), Some("a"));
        assert_eq!(shared[1].parent.as_deref(), Some("b"));
        assert_eq!(shared[0].path, ["root", "a", "shared"]);
        assert_eq!(shared[1].path, ["root", "b", "shared"]);
        assert_eq!(rows[0].parent.as_deref(), Some("root"));
    }
}
