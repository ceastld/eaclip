//! 2026-10-28 top-edge file line (proposal D), first native batch.
//!
//! An entry beside the Super menu's file category: rest the pointer at the
//! top screen edge and a click-through thread appears, then the file menu's
//! root folders drop as tags on a line. A topmost, never-activating overlay
//! holds the line and its sheets (its window region is the only part that
//! takes input); a separate layered, click-through strip draws the thread.
//! Nodes and clicks reuse the file menu through a detached core session.
//! Drag gestures and keyboard navigation share the same detached session.

use std::{sync::Arc, time::{Duration, Instant}};

use cliphost_core::{host::HostState, i18n::tr, screen_edge::{self, ScreenEdge}};
use gpui::{
    canvas, div, fill, point, prelude::*, px, size, AnimationExt, AnyElement, App, Bounds, Context, Entity,
    MouseButton, PathBuilder, SharedString, Subscription, Task, Timer, Window, WindowHandle,
    WindowOptions,
};
use serde_json::{json, Value};

use crate::{
    crisp_svg::svg,
    file_line_trigger::{Change, EdgeTrigger, Phase, Sample},
    motion, platform,
    preview::DesignTokens,
    super_menu::{enabled, expandable, primary_invokes},
    view::color,
};

#[cfg(all(windows, debug_assertions))]
#[path = "file_line_fixture.rs"]
pub mod fixture;

#[path = "file_line_interaction.rs"]
mod interaction;
use interaction::Interaction;
#[path = "file_line_drop.rs"]
mod native_drop;

const POLL: Duration = Duration::from_millis(40);
/// While the setting is off, only notice when it turns on.
const IDLE_POLL: Duration = Duration::from_millis(500);
const HOVER_OPEN: Duration = Duration::from_millis(160);
const HOVER_SWITCH: Duration = Duration::from_millis(90);
const HOVER_CLOSE: Duration = Duration::from_millis(300);

// Logical pixels, matching `GpuiAlignmentFileMenuLine.css`.
const BAND_HEIGHT: f32 = 74.0;
const CORD_Y: f32 = 12.0;
const SAG: f32 = 7.0;
const LINE_TOP: f32 = CORD_Y - 7.0;
const LINE_PAD: f32 = 14.0;
const TAG_GAP: f32 = 10.0;
const PEG_DROP: f32 = 9.0;
const TAG_HEIGHT: f32 = 30.0;
const TAG_MAX: f32 = 132.0;
const TAG_FONT: f32 = 12.0;
const KNOT: f32 = 8.0;
const ADD: f32 = 30.0;
const SHEET_WIDTH: f32 = 264.0;
const SHEET_HEADER: f32 = 38.0;
const ROW: f32 = 28.0;
const LOOSE_TOP: f32 = 16.0;
const MAX_PINS: usize = 3;
const PEEK_BAND: f64 = 30.0;
const THREAD_HEIGHT: f32 = 16.0;
const THREAD_Y: f32 = 4.0;
const THREAD_SAG: f32 = 2.0;

fn cord_y(x: f32, width: f32, top: f32, sag: f32) -> f32 {
    let t = (x / width.max(1.0)).clamp(0.0, 1.0);
    top + 4.0 * sag * t * (1.0 - t)
}

/// Explicit label widths, as the Super menu strip does: CJK at 1em, Latin ~0.62em.
fn label_width(label: &str) -> f32 {
    label.chars().map(|ch| if ch as u32 >= 0x2E80 { TAG_FONT } else { TAG_FONT * 0.62 }).sum::<f32>().ceil() + 2.0
}

fn text(node: &Value, key: &str) -> String {
    node[key].as_str().unwrap_or("").to_string()
}

fn node_path(node: &Value) -> String {
    node["payload"]["path"].as_str().unwrap_or("").to_string()
}

fn is_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() <= 3 && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn row_icon(node: &Value) -> &'static str {
    if expandable(node) {
        return "folder-closed";
    }
    let path = node_path(node).to_ascii_lowercase();
    match path.rsplit('.').next().unwrap_or("") {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => "image",
        "md" | "txt" | "docx" | "doc" | "pdf" => "text",
        "rs" | "ts" | "tsx" | "js" | "json" | "cs" | "py" => "code",
        "url" | "lnk" => "link",
        _ => "file",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagKind { Folder, Opened, Group, Drive, Recent }

#[derive(Clone)]
struct Tag {
    id: String,
    label: String,
    kind: TagKind,
    node: Value,
}

#[derive(Clone)]
enum Slot { Tag(Tag), Knot }

/// Root rows become tags; separators become knots. Maintenance rows stay in
/// the Super menu, except "add folder", which hangs as the trailing "+".
fn line_slots(items: &[Value]) -> (Vec<Slot>, Option<Value>) {
    let mut slots = Vec::new();
    let mut add = None;
    for node in items {
        let role = node["role"].as_str().unwrap_or("");
        if node["kind"].as_str() == Some("Separator") {
            if matches!(slots.last(), Some(Slot::Tag(_))) {
                slots.push(Slot::Knot);
            }
            continue;
        }
        if role == "file-add-dir" {
            add = Some(node.clone());
            continue;
        }
        if matches!(role, "file-add-opened-root" | "file-add-clipboard" | "setting") || !enabled(node) {
            continue;
        }
        let path = node_path(node);
        let kind = match role {
            "recent-files" => TagKind::Recent,
            "opened-folder" if path.is_empty() => TagKind::Group,
            "opened-folder" => TagKind::Opened,
            _ if is_drive(&path) => TagKind::Drive,
            _ => TagKind::Folder,
        };
        let label = match kind {
            TagKind::Recent => tr("最近", "Recent").to_string(),
            TagKind::Group => tr("已打开", "Open").to_string(),
            TagKind::Drive => path.trim_end_matches('\\').to_string(),
            _ => text(node, "title"),
        };
        slots.push(Slot::Tag(Tag { id: text(node, "id"), label, kind, node: node.clone() }));
    }
    while matches!(slots.last(), Some(Slot::Knot)) {
        slots.pop();
    }
    (slots, add)
}

struct Level {
    node: Value,
    items: Vec<Value>,
    cursor: Option<String>,
    has_more: bool,
    loading: bool,
    error: Option<String>,
    scroll: gpui::ScrollHandle,
}

impl Level {
    fn new(node: Value) -> Self {
        Self { node, items: Vec::new(), cursor: None, has_more: false, loading: true, error: None, scroll: gpui::ScrollHandle::new() }
    }
}

struct Sheet {
    tag: String,
    pinned: bool,
    levels: Vec<Level>,
    task: Option<Task<()>>,
}

#[derive(Clone, Copy)]
struct TagBox { left: f32, width: f32, top: f32 }

#[derive(Clone, Copy)]
struct SheetBox { left: f32, top: f32, height: f32 }

/// Background work can finish after its GPUI task is cancelled. Keep ownership
/// with the reply until the live line accepts it, so that case closes the menu.
struct PendingMenuSession {
    host: Arc<HostState>,
    id: Option<String>,
}

impl PendingMenuSession {
    fn accept(mut self) -> String {
        self.id.take().unwrap()
    }
}

impl Drop for PendingMenuSession {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.host.menu_close_session(&id);
        }
    }
}

/// The model behind both windows. `LineView` and `ThreadView` only render it.
pub struct FileLine {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    trigger: EdgeTrigger,
    edge: Option<ScreenEdge>,
    session: Option<String>,
    root_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    hover_task: Option<Task<()>>,
    slots: Vec<Slot>,
    add: Option<Value>,
    sheets: Vec<Sheet>,
    status: Option<String>,
    epoch: u64,
    mouse_down: bool,
    interaction: Interaction,
    native_drop: native_drop::FileLineDrop,
    target: i64,
    suspended: bool,
    shown_edge: Option<ScreenEdge>,
    line_window: Option<WindowHandle<LineView>>,
    thread_window: Option<WindowHandle<ThreadView>>,
    line_shown: bool,
    thread_shown: bool,
    _poll: Task<()>,
}

impl FileLine {
    pub fn start(host: Arc<HostState>, tokens: DesignTokens, dark: bool, cx: &mut App) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let poll = cx.spawn(async move |this, cx| loop {
                let Ok(delay) = this.update(cx, |line, cx| line.tick(cx)) else { break };
                Timer::after(delay).await;
            });
            Self {
                host, tokens, dark,
                trigger: EdgeTrigger::default(),
                edge: None, session: None, root_task: None, action_task: None, hover_task: None,
                slots: Vec::new(), add: None, sheets: Vec::new(), status: None, epoch: 0,
                mouse_down: false, interaction: Interaction::default(), native_drop: native_drop::FileLineDrop::default(), target: 0, suspended: false, shown_edge: None, line_window: None, thread_window: None,
                line_shown: false, thread_shown: false, _poll: poll,
            }
        })
    }

    pub fn set_theme(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        cx.notify();
    }

    fn width(&self) -> f32 {
        self.edge.map_or(1.0, |edge| (edge.work.width / edge.dpi_scale) as f32)
    }

    fn height(&self) -> f32 {
        self.edge.map_or(1.0, |edge| (edge.work.height / edge.dpi_scale) as f32)
    }

    fn pinned(&self) -> bool {
        self.sheets.iter().any(|sheet| sheet.pinned)
    }

    fn tick(&mut self, cx: &mut Context<Self>) -> Duration {
        if !self.host.file_line_enabled() {
            let was_visible = self.trigger.phase() != Phase::Hidden;
            self.host.clear_native_file_line_keyboard(self.epoch);
            if self.line_shown || self.thread_shown || self.interaction.task.is_some() {
                self.epoch = self.epoch.wrapping_add(1);
            }
            self.interaction.task = None;
            self.native_drop.cancel();
            self.interaction.gesture = None;
            self.interaction.undo = None;
            self.interaction.keyboard_mode = false;
            self.trigger.retract(); // Also discard a not-yet-visible edge dwell.
            if was_visible || self.line_shown || self.thread_shown || !self.sheets.is_empty()
                || self.session.is_some() || self.root_task.is_some()
                || self.hover_task.is_some() || self.action_task.is_some()
            {
                self.hover_task = None;
                self.action_task = None;
                self.sheets.clear();
                self.close_session();
                self.sync_windows(cx);
            }
            return IDLE_POLL;
        }
        self.expire_undo(cx);
        let Some((x, y)) = platform::cursor_position() else { return POLL };
        let phase = self.trigger.phase();
        let external_drag = self.native_drop.active_drag();
        // The line stays on the monitor it dropped on; resting picks a monitor.
        let edge = match (phase, self.edge) {
            (Phase::Down, Some(edge)) => Some(edge),
            _ => screen_edge::screen_edge_from_point(x, y),
        };
        let Some(edge) = edge else { return POLL };
        let monitor_top = edge.monitor.top;
        let at_edge = f64::from(y) <= monitor_top + 1.0
            && f64::from(x) >= edge.monitor.left && f64::from(x) < edge.monitor.left + edge.monitor.width;
        let local_edge = if self.line_shown { self.edge.unwrap_or(edge) } else { edge };
        let local = (((f64::from(x) - local_edge.work.left) / local_edge.dpi_scale) as f32,
            ((f64::from(y) - local_edge.work.top) / local_edge.dpi_scale) as f32);
        let in_peek_band = at_edge || (f64::from(y) - monitor_top) / edge.dpi_scale <= PEEK_BAND
            && f64::from(x) >= edge.monitor.left && f64::from(x) < edge.monitor.left + edge.monitor.width;
        let in_line = phase == Phase::Down && self.edge == Some(edge) && (self.in_zone(local) || self.interaction.keyboard_mode);
        // Only ask about full-screen programs when the edge could trigger.
        let blocked = (phase != Phase::Hidden || at_edge || self.pinned()) && screen_edge::foreground_is_fullscreen();
        if self.suspended != blocked {
            self.suspended = blocked;
            self.sync_windows(cx);
        }
        if external_drag && at_edge && !blocked && phase != Phase::Down {
            self.edge = Some(edge);
            self.trigger.drop_now(true);
            self.drop_line(cx);
            return POLL;
        }
        self.poll_drop(cx);
        self.poll_keyboard(local, cx);
        if self.poll_gesture(local, cx) { return POLL; }
        // Native pickers and accepted writes own their lifecycle until completion.
        if self.action_task.is_some() || self.interaction.task.is_some() || self.native_drop.hovering() || self.native_drop.outgoing_active() { return POLL; }
        let pressed = mouse_pressed();
        let clicked = pressed && !self.mouse_down;
        self.mouse_down = pressed;
        if phase == Phase::Hidden && !self.pinned() || phase == Phase::Peek {
            self.edge = Some(edge);
        }
        let change = if clicked && phase == Phase::Peek {
            // A click on the thread belongs to the window underneath.
            self.trigger.retract();
            Change::Hide
        } else if clicked && phase == Phase::Down && !in_line {
            self.trigger.retract();
            Change::Hide
        } else {
            self.trigger.step(Sample { now: Instant::now(), x, at_edge, in_peek_band, in_line, blocked })
        };
        match change {
            Change::None => {}
            Change::Peek => self.sync_windows(cx),
            Change::Drop => self.drop_line(cx),
            Change::Hide => self.fold(cx),
        }
        POLL
    }

    fn drop_line(&mut self, cx: &mut Context<Self>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.status = None;
        self.target = self.host.file_line_target();
        self.load_root(cx);
        self.sync_windows(cx);
    }

    /// Back to the edge: the loose sheet closes, clipped sheets stay on the
    /// thread, and the session ends once nothing hangs any more.
    fn fold(&mut self, cx: &mut Context<Self>) {
        self.hover_task = None;
        self.interaction.keyboard_mode = false;
        self.sheets.retain(|sheet| sheet.pinned);
        if self.sheets.is_empty() {
            self.host.clear_native_file_line_keyboard(self.epoch);
            self.close_session();
        }
        self.sync_windows(cx);
    }

    fn close_session(&mut self) {
        self.root_task = None;
        self.slots.clear();
        self.add = None;
        if let Some(id) = self.session.take() {
            let host = self.host.clone();
            std::thread::spawn(move || { host.menu_close_session(&id); });
        }
    }

    fn load_root(&mut self, cx: &mut Context<Self>) {
        let host = self.host.clone();
        let old = self.session.take();
        let target = self.target;
        let epoch = self.epoch;
        let restore: Vec<_> = self.sheets.iter().filter_map(|sheet| {
            let tag = self.tag(&sheet.tag)?;
            Some((tag.node.clone(), sheet.pinned, sheet.levels.iter().skip(1).map(|level| node_path(&level.node)).collect::<Vec<_>>()))
        }).collect();
        for sheet in &mut self.sheets { sheet.task = None; }
        self.root_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                if let Some(old) = old { host.menu_close_session(&old); }
                let session = host.file_line_open_session(target);
                let id = session["sessionId"].as_str().unwrap_or("").to_string();
                let pending = PendingMenuSession { host: host.clone(), id: Some(id.clone()) };
                let root = host.menu_nodes_ex(&id, "", "children", None);
                let mut sheets = Vec::new();
                if let Ok(root) = &root {
                    let items = root["items"].as_array().cloned().unwrap_or_default();
                    for (previous, pinned, trail) in restore {
                        if let Some(node) = items.iter().find(|node| same_root(node, &previous)) {
                            let mut levels = Vec::new();
                            let mut current = node.clone();
                            for next in trail.iter().map(Some).chain(std::iter::once(None)) {
                                let page = host.menu_nodes_ex(&id, &text(&current, "id"), "children", None);
                                let child = next.and_then(|path| page.as_ref().ok()?.get("items")?.as_array()?.iter().find(|node| node_path(node).eq_ignore_ascii_case(path))).cloned();
                                levels.push((current.clone(), page));
                                if let Some(child) = child { current = child; } else { break; }
                            }
                            sheets.push((text(node, "id"), pinned, levels));
                        }
                    }
                }
                (pending, root, sheets)
            }).await;
            let _ = this.update(cx, |line, cx| {
                let (pending, root, sheets) = result;
                if line.epoch != epoch { return; }
                line.root_task = None;
                if line.trigger.phase() != Phase::Down && !sheets.iter().any(|(_, pinned, _)| *pinned) {
                    return;
                }
                line.session = Some(pending.accept());
                let sheets = sheets.into_iter().filter(|(_,pinned,_)| line.trigger.phase()==Phase::Down || *pinned).collect::<Vec<_>>();
                line.sheets = sheets.into_iter().map(|(tag, pinned, levels)| Sheet {
                    tag, pinned, task: None,
                    levels: levels.into_iter().map(|(node, page)| {
                        let mut level = Level::new(node);
                        level.loading = false;
                        match page {
                            Ok(page) => {
                                level.items = page["items"].as_array().cloned().unwrap_or_default();
                                level.has_more = page["hasMore"].as_bool().unwrap_or(false);
                                level.cursor = page["nextCursor"].as_str().map(str::to_string);
                            }
                            Err(error) => level.error = Some(error),
                        }
                        level
                    }).collect(),
                }).collect();
                match root {
                    Ok(root) => {
                        let items = root["items"].as_array().cloned().unwrap_or_default();
                        (line.slots, line.add) = line_slots(&items);
                    }
                    Err(error) => line.status = Some(error),
                }
                line.sync_windows(cx);
            });
        }));
    }

    fn tag(&self, id: &str) -> Option<&Tag> {
        self.slots.iter().find_map(|slot| match slot { Slot::Tag(tag) if tag.id == id => Some(tag), _ => None })
    }

    /// Tag boxes left to right; the trailing "+" sits at the right end.
    fn layout(&self) -> (Vec<(usize, TagBox)>, Vec<f32>, f32) {
        let width = self.width();
        let mut x = LINE_PAD;
        let mut tags = Vec::new();
        let mut knots = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            match slot {
                Slot::Knot => {
                    knots.push(x + KNOT / 2.0 - TAG_GAP / 2.0);
                    x += KNOT;
                }
                Slot::Tag(tag) => {
                    let w = tag_width(tag);
                    let sag = cord_y(x + w / 2.0, width, CORD_Y, SAG) - CORD_Y;
                    tags.push((index, TagBox { left: x, width: w, top: LINE_TOP + sag + PEG_DROP }));
                    x += w + TAG_GAP;
                }
            }
        }
        (tags, knots, (width - LINE_PAD - ADD).max(x))
    }

    fn sheet_boxes(&self) -> Vec<(usize, SheetBox)> {
        let width = self.width();
        let height = self.height();
        let (tags, _, _) = self.layout();
        let down = self.trigger.phase() == Phase::Down;
        let mut placed: Vec<(usize, f32, f32)> = self.sheets.iter().enumerate().map(|(index, sheet)| {
            let tag = tags.iter().find(|(slot, _)| matches!(&self.slots[*slot], Slot::Tag(tag) if tag.id == sheet.tag)).map(|(_, tag)| *tag);
            let center = tag.map_or(width / 2.0, |tag| tag.left + tag.width / 2.0);
            let top = match (down, tag) { (true, Some(tag)) => tag.top + TAG_HEIGHT + 10.0, _ => LOOSE_TOP };
            (index, (center - SHEET_WIDTH / 2.0).clamp(8.0, (width - SHEET_WIDTH - 8.0).max(8.0)), top)
        }).collect();
        placed.sort_by(|a, b| a.1.total_cmp(&b.1));
        for index in 1..placed.len() {
            placed[index].1 = placed[index].1.max(placed[index - 1].1 + SHEET_WIDTH + 8.0);
        }
        for index in (0..placed.len()).rev() {
            let limit = if index + 1 == placed.len() { width - SHEET_WIDTH - 8.0 } else { placed[index + 1].1 - SHEET_WIDTH - 8.0 };
            placed[index].1 = placed[index].1.min(limit).max(8.0);
        }
        placed.into_iter().map(|(index, left, top)| {
            let level = self.sheets[index].levels.last();
            let rows = level.map_or(1, |level| level.items.len().max(1)) as f32;
            let natural = SHEET_HEADER + rows * ROW + 10.0 + if level.is_some_and(|level| level.has_more) { ROW } else { 0.0 };
            (index, SheetBox { left, top, height: natural.min(height - top - 12.0).max(SHEET_HEADER + ROW) })
        }).collect()
    }

    fn in_zone(&self, (x, y): (f32, f32)) -> bool {
        if y <= BAND_HEIGHT + 6.0 && x >= -8.0 && x <= self.width() + 8.0 {
            return true;
        }
        self.sheet_boxes().iter().any(|(_, sheet)| x >= sheet.left - 10.0 && x <= sheet.left + SHEET_WIDTH + 10.0
            && y >= sheet.top - 24.0 && y <= sheet.top + sheet.height + 10.0)
    }

    fn regions(&self) -> Vec<(f32, f32, f32, f32)> {
        let mut regions = Vec::new();
        if self.trigger.phase() == Phase::Down {
            regions.push((0.0, 0.0, self.width(), BAND_HEIGHT));
        }
        for (_, sheet) in self.sheet_boxes() {
            regions.push((sheet.left, sheet.top - 14.0, SHEET_WIDTH, sheet.height + 14.0));
        }
        if self.interaction.gesture.as_ref().is_some_and(|gesture| gesture.active && gesture.removable) { regions.push(self.tray_rect()); }
        if let Some(gesture) = self.interaction.gesture.as_ref().filter(|gesture| gesture.active) { regions.push(self.ghost_rect(gesture.point)); }
        regions
    }

    /// Window creation draws synchronously, and the views read this entity.
    /// Leave both the model update and any originating window event before
    /// creating/updating windows. Read the latest state when the defer runs so
    /// a fold or disable in this update cannot be undone by a stale show.
    fn sync_windows(&mut self, cx: &mut Context<Self>) {
        if self.edge.is_none() { return; }
        let line = cx.entity().downgrade();
        cx.defer(move |cx| {
            let Some(line) = line.upgrade() else { return };
            let result = Self::ensure_windows(&line, cx);
            line.update(cx, |line, cx| match result {
                Ok(()) => line.apply_windows(cx),
                Err(error) => { line.status = Some(error); cx.notify(); }
            });
        });
    }

    fn window_visibility(&self) -> (bool, bool) {
        let phase = self.trigger.phase();
        (
            !self.suspended && (phase == Phase::Down || !self.sheets.is_empty()),
            !self.suspended && (phase == Phase::Peek || phase == Phase::Hidden && self.pinned()),
        )
    }

    /// Do not call this from an entity update: open_view renders immediately.
    fn ensure_windows(line: &Entity<Self>, cx: &mut App) -> Result<(), String> {
        let (open_line, open_thread) = {
            let state = line.read(cx);
            if state.edge.is_none() { return Ok(()); }
            let (line_visible, thread_visible) = state.window_visibility();
            (line_visible && state.line_window.is_none(), thread_visible && state.thread_window.is_none())
        };
        if open_line {
            let handle = open_view(line.clone(), cx, LineView::new)?;
            line.update(cx, |line, _| line.line_window = Some(handle));
        }
        if open_thread {
            let handle = open_view(line.clone(), cx, ThreadView::new)?;
            line.update(cx, |line, _| line.thread_window = Some(handle));
        }
        Ok(())
    }

    /// Show, place and clip already-created windows outside their input events.
    fn apply_windows(&mut self, cx: &mut Context<Self>) {
        let (line_visible, thread_visible) = self.window_visibility();
        let Some(edge) = self.edge else { return };
        let reposition = self.shown_edge != Some(edge);
        self.shown_edge = Some(edge);
        let work = (edge.work.left as i32, edge.work.top as i32, edge.work.width as i32, edge.work.height as i32);
        if line_visible {
            let Some(handle) = self.line_window else { return };
            let regions = self.regions();
            let owner = self.host.host_hwnd();
            let show = !self.line_shown || reposition;
            let _ = handle.update(cx, |_, window, _| -> Result<(), String> {
                if show { platform::overlay_show(window, owner, work)?; }
                platform::overlay_set_regions(window, regions)?;
                Ok(())
            });
            self.line_shown = true;
        } else if self.line_shown {
            if let Some(handle) = self.line_window {
                let _ = handle.update(cx, |_, window, _| platform::hide_cleared(window));
            }
            self.line_shown = false;
        }
        if thread_visible {
            let Some(handle) = self.thread_window else { return };
            if !self.thread_shown || reposition {
                let strip = (work.0, work.1, work.2, (f64::from(THREAD_HEIGHT) * edge.dpi_scale).ceil() as i32);
                let _ = handle.update(cx, |_, window, _| platform::drag_preview_show(window, strip));
                self.thread_shown = true;
            }
        } else if self.thread_shown {
            if let Some(handle) = self.thread_window {
                let _ = handle.update(cx, |_, window, _| platform::hide(window));
            }
            self.thread_shown = false;
        }
        #[cfg(windows)]
        if let Some(handle) = self.line_window {
            if let Ok(Ok(hwnd)) = handle.update(cx, |_,window,_| platform::hwnd(window).map(|hwnd|hwnd.0 as isize)) {
                let accepting = self.line_shown && !self.suspended && self.host.file_line_enabled();
                if let Err(error) = self.native_drop.sync(hwnd, self.epoch, accepting, self.interaction.task.is_none() && self.action_task.is_none()) { self.status = Some(error); }
            }
        }
        cx.notify();
    }

    fn hover_tag(&mut self, id: String, hovered: bool, cx: &mut Context<Self>) {
        let loose = self.sheets.iter().position(|sheet| !sheet.pinned);
        if self.interaction.gesture.is_some() || self.interaction.keyboard_mode { return; }
        if !hovered {
            if loose.is_some() { self.schedule(None, HOVER_CLOSE, cx); }
            return;
        }
        if self.sheets.iter().any(|sheet| sheet.tag == id) {
            self.hover_task = None;
            return;
        }
        self.schedule(Some(id), if loose.is_some() { HOVER_SWITCH } else { HOVER_OPEN }, cx);
    }

    fn hover_sheet(&mut self, pinned: bool, hovered: bool, cx: &mut Context<Self>) {
        if self.interaction.keyboard_mode || self.interaction.gesture.is_some() { return; }
        if hovered { self.hover_task = None; } else if !pinned { self.schedule(None, HOVER_CLOSE, cx); }
    }

    /// Open `id` as the loose sheet (or close the loose sheet) after `delay`.
    fn schedule(&mut self, id: Option<String>, delay: Duration, cx: &mut Context<Self>) {
        self.hover_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(delay).await;
            let _ = this.update(cx, |line, cx| {
                line.hover_task = None;
                line.sheets.retain(|sheet| sheet.pinned);
                if let Some(id) = id { line.open_sheet(id, false, cx); } else { line.sync_windows(cx); }
            });
        }));
    }

    fn open_sheet(&mut self, id: String, pinned: bool, cx: &mut Context<Self>) {
        if self.trigger.phase() != Phase::Down || self.sheets.iter().any(|sheet| sheet.tag == id) { return; }
        let Some(tag) = self.tag(&id).cloned() else { return };
        if !pinned { self.sheets.retain(|sheet| sheet.pinned); }
        self.sheets.push(Sheet { tag: id.clone(), pinned, levels: vec![Level::new(tag.node)], task: None });
        self.load_level(id, false, cx);
        self.sync_windows(cx);
    }

    fn toggle_sheet(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(index) = self.sheets.iter().position(|sheet| sheet.tag == id) {
            self.sheets.remove(index);
            self.sync_windows(cx);
        } else {
            self.sheets.retain(|sheet| sheet.pinned);
            self.open_sheet(id, false, cx);
        }
    }

    fn toggle_pin(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.sheets.iter().position(|sheet| sheet.tag == id) else { return };
        if self.sheets[index].pinned {
            self.sheets[index].pinned = false;
            // Unclipped while the line is down, it stays as the loose sheet.
            if self.trigger.phase() != Phase::Down { self.sheets.remove(index); }
            else { let keep = self.sheets.remove(index); self.sheets.retain(|sheet| sheet.pinned); self.sheets.push(keep); }
        } else {
            let mut sheet = self.sheets.remove(index);
            sheet.pinned = true;
            self.sheets.push(sheet);
            let pins = self.sheets.iter().filter(|sheet| sheet.pinned).count();
            if pins > MAX_PINS {
                if let Some(oldest) = self.sheets.iter().position(|sheet| sheet.pinned) { self.sheets.remove(oldest); }
            }
        }
        self.sync_windows(cx);
    }

    fn drill(&mut self, id: &str, node: Value, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheets.iter_mut().find(|sheet| sheet.tag == id) else { return };
        sheet.levels.push(Level::new(node));
        self.load_level(id.to_string(), false, cx);
        self.sync_windows(cx);
    }

    fn back(&mut self, id: &str, depth: usize, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheets.iter_mut().find(|sheet| sheet.tag == id) else { return };
        sheet.levels.truncate(depth.max(1));
        sheet.task = None;
        if let Some(level) = sheet.levels.last_mut() { level.loading = false; }
        self.sync_windows(cx);
    }

    /// Load the deepest level of a sheet: its first page, or the next one.
    fn load_level(&mut self, id: String, more: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else { return };
        let Some(sheet) = self.sheets.iter_mut().find(|sheet| sheet.tag == id) else { return };
        let depth = sheet.levels.len();
        let Some(level) = sheet.levels.last_mut() else { return };
        if more && (level.loading || !level.has_more) { return; }
        level.loading = true;
        let parent = text(&level.node, "id");
        let cursor = if more { level.cursor.clone() } else { None };
        let host = self.host.clone();
        sheet.task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                host.menu_nodes_ex(&session, &parent, "children", cursor.as_deref())
            }).await;
            let _ = this.update(cx, |line, cx| {
                let Some(sheet) = line.sheets.iter_mut().find(|sheet| sheet.tag == id) else { return };
                if sheet.levels.len() != depth { return; }
                let level = sheet.levels.last_mut().unwrap();
                level.loading = false;
                match result {
                    Ok(page) => {
                        let items = page["items"].as_array().cloned().unwrap_or_default();
                        if more { level.items.extend(items); } else { level.items = items; }
                        level.has_more = page["hasMore"].as_bool().unwrap_or(false);
                        level.cursor = page["nextCursor"].as_str().map(str::to_string);
                        level.error = None;
                    }
                    Err(error) => level.error = Some(error),
                }
                sheet.task = None;
                if line.interaction.focus == Some(interaction::Focus::Sheet(id.clone(), None)) { line.focus_sheet(&id); }
                line.sync_windows(cx);
            });
        }));
    }

    /// The file menu's own click: locate in a file dialog, go there in
    /// Explorer, switch to an open folder, or open. Ctrl keeps the line down.
    fn invoke(&mut self, node: Value, keep: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else { return };
        if self.action_task.is_some() || !enabled(&node) { return; }
        let host = self.host.clone();
        let id = text(&node, "id");
        self.host.clear_native_file_line_keyboard(self.epoch);
        let epoch = self.epoch;
        let current_session = session.clone();
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = crate::native_dialog::run(move || host.menu_invoke(&session, &id, &json!({}))).await;
            let _ = this.update(cx, |line, cx| {
                line.action_task = None;
                if line.epoch != epoch || line.session.as_ref() != Some(&current_session) { return; }
                match result {
                    Ok(reply) if reply["browsePath"].is_string() => {
                        // "Open folders in Super menu" asks to browse instead.
                        let tag = line.sheets.iter().find(|sheet| sheet.levels.iter().any(|level| level.items.iter().any(|item| item["id"] == node["id"])))
                            .map(|sheet| sheet.tag.clone());
                        match tag {
                            Some(tag) => line.drill(&tag, node, cx),
                            None => { let id = text(&node, "id"); line.toggle_sheet(id, cx); }
                        }
                    }
                    Ok(reply) => {
                        if reply["refresh"].as_str() == Some("Root") {
                            line.load_root(cx);
                            return;
                        }
                        if !keep && reply["closeMenu"].as_bool().unwrap_or(true) {
                            line.trigger.retract();
                            line.fold(cx);
                        } else if let Some(message) = reply["message"].as_str().filter(|message| !message.is_empty()) {
                            line.status = Some(message.to_string());
                            cx.notify();
                        }
                    }
                    Err(error) => { line.status = Some(error); cx.notify(); }
                }
            });
        }));
    }

    fn activate_tag(&mut self, id: String, ctrl: bool, cx: &mut Context<Self>) {
        if self.click_is_suppressed() { return; }
        let Some(tag) = self.tag(&id).cloned() else { return };
        if matches!(tag.kind, TagKind::Group | TagKind::Recent) || !primary_invokes(&tag.node) {
            self.toggle_sheet(id, cx);
        } else {
            self.invoke(tag.node, ctrl, cx);
        }
    }

    fn activate_row(&mut self, id: &str, node: Value, ctrl: bool, cx: &mut Context<Self>) {
        if self.click_is_suppressed() { return; }
        if !enabled(&node) { return; }
        if expandable(&node) && !primary_invokes(&node) { self.drill(id, node, cx); } else { self.invoke(node, ctrl, cx); }
    }
}

impl Drop for FileLine {
    fn drop(&mut self) {
        self.host.clear_native_file_line_keyboard(self.epoch);
        self.native_drop.cancel();
        if let Some(session)=self.session.take() { self.host.menu_close_session(&session); }
    }
}

fn same_root(left: &Value, right: &Value) -> bool {
    (left["role"] != "opened-folder" || left["payload"]["windowHandle"] == right["payload"]["windowHandle"])
        && left["role"] == right["role"] && node_path(left).eq_ignore_ascii_case(&node_path(right))
        && ( !node_path(left).is_empty() || left["title"] == right["title"] )
}

fn tag_width(tag: &Tag) -> f32 {
    let icon = 14.0 + 6.0;
    let badge = if tag.kind == TagKind::Opened { 6.0 + 6.0 } else { 0.0 };
    (20.0 + icon + label_width(&tag.label) + badge).min(TAG_MAX)
}

#[cfg(windows)]
fn mouse_pressed() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    [VK_LBUTTON, VK_RBUTTON].into_iter().any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) } as u16 & 0x8000 != 0)
}
#[cfg(not(windows))]
fn mouse_pressed() -> bool { false }

fn open_view<V: 'static + Render>(
    line: Entity<FileLine>,
    cx: &mut App,
    build: fn(Entity<FileLine>, &mut Context<V>) -> V,
) -> Result<WindowHandle<V>, String> {
    cx.open_window(WindowOptions {
        titlebar: None, focus: false, show: false, is_resizable: false, is_minimizable: false,
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(80.0))))),
        window_background: gpui::WindowBackgroundAppearance::Transparent,
        ..Default::default()
    }, move |window, cx| {
        window.set_window_title(tr("文件晾衣绳", "File line"));
        cx.new(|cx| build(line, cx))
    }).map_err(|error| error.to_string())
}

pub struct LineView {
    line: Entity<FileLine>,
    _observe: Subscription,
}

impl LineView {
    fn new(line: Entity<FileLine>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&line, |_, _, cx| cx.notify());
        Self { line, _observe: observe }
    }
}

pub struct ThreadView {
    line: Entity<FileLine>,
    _observe: Subscription,
}

impl ThreadView {
    fn new(line: Entity<FileLine>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&line, |_, _, cx| cx.notify());
        Self { line, _observe: observe }
    }
}

fn icon(tokens: &DesignTokens, name: &str, size: f32) -> crate::crisp_svg::CrispSvg {
    svg().path(tokens.icons.get(name).cloned().unwrap_or_default()).size(px(size))
}

/// A clothespin: on the line it holds a tag; on a sheet it is the pin toggle.
fn peg(ink: gpui::Hsla, fill_color: gpui::Hsla, width: f32, height: f32, open: bool) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds, _, window, _| {
        let prong = width * 0.27;
        let spread = if open { width * 0.08 } else { 0.0 };
        for (dx, tilt) in [(width * 0.2, -spread), (width * 0.53, spread)] {
            let x = bounds.origin.x + px(dx + tilt);
            let prong_bounds = Bounds::new(point(x, bounds.origin.y), size(px(prong), px(height)));
            window.paint_quad(fill(prong_bounds, fill_color).corner_radii(px(prong / 2.0)).border_widths(px(0.9)).border_color(ink));
        }
        let spring = Bounds::new(point(bounds.origin.x + px(width / 2.0 - 1.8), bounds.origin.y + px(height * 0.45 - 1.8)), size(px(3.6), px(3.6)));
        window.paint_quad(fill(spring, fill_color).corner_radii(px(1.8)).border_widths(px(0.9)).border_color(ink));
    }).w(px(width)).h(px(height))
}

fn cord(ink: gpui::Hsla, width: f32, top: f32, sag: f32) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds, _, window, _| {
        let origin = bounds.origin;
        let mut path = PathBuilder::stroke(px(1.5));
        path.move_to(origin + point(px(0.0), px(top)));
        path.curve_to(origin + point(px(width), px(top)), origin + point(px(width / 2.0), px(top + sag * 2.0)));
        if let Ok(path) = path.build() { window.paint_path(path, ink); }
    }).absolute().left_0().top_0().w(px(width)).h(px(top + sag * 2.0 + 2.0))
}

/// Drop settle: the tag falls from above and swings to rest, staggered.
fn settle(local: f32) -> (f32, f32) {
    let local = local.clamp(0.0, 1.0);
    let offset = -14.0 * (-4.5 * local).exp() * (3.0 * std::f32::consts::PI * local).cos();
    (if local >= 1.0 { 0.0 } else { offset }, (local * 4.0).min(1.0))
}

impl Render for ThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line = self.line.read(cx);
        let t = line.tokens.theme(line.dark);
        let ink = color(&t.text_muted);
        let accent = color(&t.accent);
        let width = line.width();
        let (tags, _, _) = line.layout();
        let pinned: Vec<String> = line.sheets.iter().filter(|sheet| sheet.pinned).map(|sheet| sheet.tag.clone()).collect();
        let mut root = div().relative().size_full().child(cord(ink.opacity(0.7), width, THREAD_Y, THREAD_SAG));
        for (slot, tag) in tags {
            let center = tag.left + tag.width / 2.0;
            let on = matches!(&line.slots[slot], Slot::Tag(found) if pinned.contains(&found.id));
            root = root.child(div().absolute().left(px(center - 2.0)).top(px(cord_y(center, width, THREAD_Y, THREAD_SAG) - 3.0))
                .w(px(4.0)).h(px(8.0)).rounded(px(1.5)).bg(if on { accent } else { ink.opacity(0.8) }));
        }
        root
    }
}

impl Render for LineView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = self.line.clone();
        let line = self.line.read(cx);
        let tokens = line.tokens.clone();
        let t = tokens.theme(line.dark).clone();
        let width = line.width();
        let down = line.trigger.phase() == Phase::Down;
        let (tags, knots, add_left) = line.layout();
        let reduced = motion::reduced_motion();
        let mut root = div().relative().size_full().font_family(tokens.typography.font_family.clone())
            .text_size(px(TAG_FONT)).text_color(color(&t.text));
        if down {
            let mut band = div().absolute().left_0().top_0().w(px(width)).h(px(BAND_HEIGHT))
                .bg(color(&t.background).opacity(0.82))
                .child(cord(color(&t.text).opacity(0.36), width, CORD_Y, SAG));
            for x in knots {
                band = band.child(div().absolute().left(px(x - 3.0)).top(px(cord_y(x, width, CORD_Y, SAG) - 3.0))
                    .w(px(6.0)).h(px(6.0)).rounded_full().bg(color(&t.text_secondary).opacity(0.7)));
            }
            for (order, (slot, boxed)) in tags.into_iter().enumerate() {
                let Slot::Tag(tag) = &line.slots[slot] else { continue };
                band = band.child(render_tag(&entity, line, tag, boxed, order, reduced, &tokens, &t));
            }
            let add_center = add_left + ADD / 2.0;
            let add_top = cord_y(add_center, width, CORD_Y, SAG) - 7.0 + PEG_DROP;
            if let Some(add) = line.add.clone() {
                let entity = entity.clone();
                band = band
                    .child(div().absolute().left(px(add_center - 4.5)).top(px(add_top - PEG_DROP)).child(peg(color(&t.text_secondary), color(&t.surface), 9.0, 15.0, false)))
                    .child(div().id("file-line-add").absolute().left(px(add_left)).top(px(add_top)).w(px(ADD)).h(px(ADD))
                        .flex().items_center().justify_center().rounded_full().border_1().border_dashed().border_color(color(&t.border))
                        .bg(color(&t.surface)).text_color(color(&t.text_secondary)).cursor_pointer()
                        .hover(|style| style.text_color(color(&t.accent)).border_color(color(&t.accent)))
                        .child(icon(&tokens, "plus", 13.0).text_color(color(&t.text_secondary)))
                        .on_click(move |event, _, cx| { let _ = event; entity.update(cx, |line, cx| line.invoke(add.clone(), true, cx)); }));
            }
            if let Some(status) = line.status.clone() {
                band = band.child(div().absolute().right(px(LINE_PAD + ADD + 12.0)).top(px(BAND_HEIGHT - 22.0)).max_w(px(320.0))
                    .text_size(px(11.0)).text_color(color(&t.error)).whitespace_nowrap().text_ellipsis().child(status));
            }
            if line.root_task.is_some() && line.slots.is_empty() {
                band = band.child(div().absolute().left(px(LINE_PAD)).top(px(LINE_TOP + PEG_DROP + 7.0)).text_size(px(11.0))
                    .text_color(color(&t.text_muted)).child(tr("正在读取文件菜单…", "Loading the file menu…")));
            }
            root = root.child(band);
        }
        for (index, sheet_box) in line.sheet_boxes() {
            root = root.child(render_sheet(&entity, line, index, sheet_box, &tokens, &t));
        }
        root = root.children(interaction::render(&entity, line, &tokens, &t));
        root
    }
}

fn render_tag(entity: &Entity<FileLine>, line: &FileLine, tag: &Tag, boxed: TagBox, order: usize, reduced: bool,
    tokens: &DesignTokens, t: &crate::preview::ThemeTokens) -> AnyElement {
    let open = line.sheets.iter().any(|sheet| sheet.tag == tag.id);
    let pinned = line.sheets.iter().any(|sheet| sheet.tag == tag.id && sheet.pinned);
    let icon_name = match tag.kind { TagKind::Folder => "folder-closed", TagKind::Drive => "desktop", TagKind::Recent => "list", _ => "folder-open" };
    let (hover_entity, click_entity, middle_entity) = (entity.clone(), entity.clone(), entity.clone());
    let (hover_id, click_id, middle_id) = (tag.id.clone(), tag.id.clone(), tag.id.clone());
    let accent = color(&t.accent);
    let (drag_entity, drag_node, drag_id) = (entity.clone(), tag.node.clone(), tag.id.clone());
    let body = div().id(SharedString::from(format!("file-line-tag-{}", tag.id)))
        .absolute().left_0().top(px(PEG_DROP)).w(px(boxed.width)).h(px(TAG_HEIGHT))
        .px(px(10.0)).flex().items_center().gap(px(6.0)).rounded(px(8.0)).border_1()
        .border_color(if open { accent.opacity(0.6) } else { color(&t.border) })
        .bg(color(&t.surface)).shadow_sm().whitespace_nowrap().cursor_pointer()
        .hover(move |style| style.border_color(accent.opacity(0.6)))
        .when(pinned, |tag| tag.border_b_2().border_color(accent))
        .when(line.focus_is_tag(&tag.id), |tag| tag.border_color(accent).bg(color(&t.hover)))
        .child(icon(tokens, icon_name, 14.0).text_color(if open { accent } else { color(&t.text_muted) }))
        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().child(tag.label.clone()))
        .when(tag.kind == TagKind::Opened, |tag| tag.child(div().flex_none().w(px(6.0)).h(px(6.0)).rounded_full().bg(accent)))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| drag_entity.update(cx, |line, cx| {
            line.interaction.focus = Some(interaction::Focus::Tag(drag_id.clone()));
            line.begin_gesture(drag_node.clone(), true, cx);
        }))
        .on_hover(move |hovered, _, cx| { let id = hover_id.clone(); hover_entity.update(cx, |line, cx| line.hover_tag(id, *hovered, cx)); })
        .on_click(move |event, _, cx| { let ctrl = event.modifiers().control; click_entity.update(cx, |line, cx| line.activate_tag(click_id.clone(), ctrl, cx)); })
        .on_mouse_down(MouseButton::Middle, move |_, _, cx| middle_entity.update(cx, |line, cx| line.toggle_sheet(middle_id.clone(), cx)));
    let pin = div().absolute().left(px(boxed.width / 2.0 - 4.5)).top_0()
        .child(peg(if pinned { accent } else { color(&t.text_secondary) }, color(&t.surface), 9.0, 15.0, false));
    // The hanger (peg and tag) is placed by its box; the drop moves it as one.
    let hanger = div().absolute().left(px(boxed.left)).top(px(boxed.top - PEG_DROP)).w(px(boxed.width)).h(px(TAG_HEIGHT + PEG_DROP))
        .child(body).child(pin);
    if reduced {
        return hanger.into_any_element();
    }
    let delay = order as f32 * 24.0;
    let total = 700.0 + delay;
    let top = boxed.top - PEG_DROP;
    hanger.with_animation(("file-line-drop", line.epoch * 1_000 + order as u64), gpui::Animation::new(Duration::from_millis(total as u64)),
        move |hanger, progress| {
            let elapsed = progress * total;
            let (offset, alpha) = settle((elapsed - delay) / 700.0);
            hanger.top(px(top + offset)).opacity(if elapsed < delay { 0.0 } else { alpha })
        }).into_any_element()
}

fn render_sheet(entity: &Entity<FileLine>, line: &FileLine, index: usize, boxed: SheetBox,
    tokens: &DesignTokens, t: &crate::preview::ThemeTokens) -> AnyElement {
    let sheet = &line.sheets[index];
    let id = sheet.tag.clone();
    let Some(tag) = line.tag(&id) else { return div().into_any_element() };
    let accent = color(&t.accent);
    let level = sheet.levels.last();
    let depth_count = sheet.levels.len();
    let mut crumbs = div().flex_1().min_w_0().flex().items_center().overflow_hidden().whitespace_nowrap();
    for (depth, level) in sheet.levels.iter().enumerate() {
        let label: SharedString = if depth == 0 { tag.label.clone().into() } else { text(&level.node, "title").into() };
        let last = depth + 1 == depth_count;
        if depth > 0 { crumbs = crumbs.child(div().flex_none().px(px(1.0)).text_color(color(&t.text_muted)).child("›")); }
        let (entity, id) = (entity.clone(), id.clone());
        crumbs = crumbs.child(div().id(SharedString::from(format!("file-line-crumb-{id}-{depth}"))).min_w_0().max_w(px(110.0))
            .px(px(4.0)).py(px(3.0)).rounded(px(4.0)).overflow_hidden().text_ellipsis()
            .text_color(if last { color(&t.text) } else { color(&t.text_secondary) }).when(last, |crumb| crumb.font_weight(gpui::FontWeight::SEMIBOLD))
            .when(!last, |crumb| crumb.cursor_pointer().hover(|style| style.bg(color(&t.hover)))
                .on_click(move |_, _, cx| entity.update(cx, |line, cx| line.back(&id, depth + 1, cx))))
            .child(label));
    }
    let mut header = div().flex_none().h(px(SHEET_HEADER)).pt(px(6.0)).pl(px(8.0)).pr(px(6.0)).flex().items_center().gap(px(5.0))
        .border_b_1().border_color(color(&t.border));
    if depth_count > 1 {
        let (entity, id) = (entity.clone(), id.clone());
        header = header.child(div().id(SharedString::from(format!("file-line-back-{id}"))).flex_none().w(px(24.0)).h(px(24.0))
            .flex().items_center().justify_center().rounded(px(5.0)).text_color(color(&t.text_secondary)).cursor_pointer()
            .hover(|style| style.bg(color(&t.hover))).child(icon(tokens, "chevron-left", 12.0).text_color(color(&t.text_secondary)))
            .on_click(move |_, _, cx| entity.update(cx, |line, cx| line.back(&id, depth_count - 1, cx))));
    }
    header = header.child(crumbs);
    if let Some(level) = level {
        header = header.child(div().flex_none().text_size(px(10.5)).text_color(color(&t.text_muted))
            .child(level.items.iter().filter(|item| enabled(item)).count().to_string()));
        if primary_invokes(&level.node) && !node_path(&level.node).is_empty() {
            let (entity, node) = (entity.clone(), level.node.clone());
            header = header.child(div().id(SharedString::from(format!("file-line-go-{id}"))).flex_none().h(px(24.0)).px(px(9.0))
                .flex().items_center().rounded(px(4.0)).text_size(px(11.0)).text_color(accent).bg(accent.opacity(0.12)).cursor_pointer()
                .hover(move |style| style.bg(accent.opacity(0.22))).child(tr("去那里", "Go"))
                .on_click(move |event, _, cx| { let ctrl = event.modifiers().control; entity.update(cx, |line, cx| line.invoke(node.clone(), ctrl, cx)); }));
        }
    }
    let mut rows = div().id(SharedString::from(format!("file-line-rows-{id}-{depth_count}"))).flex_1().min_h_0().p(px(4.0)).overflow_y_scroll();
    if let Some(level) = level { rows = rows.track_scroll(&level.scroll); }
    {
        let (entity, id) = (entity.clone(), id.clone());
        rows = rows.on_scroll_wheel(move |event, _, cx| {
            if event.delta.pixel_delta(px(20.0)).y < px(0.0) { entity.update(cx, |line, cx| line.load_level(id.clone(), true, cx)); }
        });
    }
    let note = |label: SharedString, ink: gpui::Hsla| div().px(px(8.0)).h(px(ROW)).flex().items_center().text_color(ink).child(label);
    match level {
        Some(level) if level.loading && level.items.is_empty() => rows = rows.child(note(tr("正在读取…", "Loading…").into(), color(&t.text_muted))),
        Some(level) if level.items.is_empty() => {
            let message = level.error.clone().unwrap_or_else(|| tr("空文件夹", "Empty folder").to_string());
            rows = rows.child(note(message.into(), if level.error.is_some() { color(&t.error) } else { color(&t.text_muted) }));
        }
        Some(level) => {
            for (row, item) in level.items.iter().enumerate() {
                if item["kind"].as_str() == Some("Separator") {
                    rows = rows.child(div().h(px(1.0)).mx(px(6.0)).my(px(4.0)).bg(color(&t.border)));
                    continue;
                }
                let usable = enabled(item);
                let (click, click_id, click_node) = (entity.clone(), id.clone(), item.clone());
                let (drag, drag_id, drag_node) = (entity.clone(), id.clone(), item.clone());
                let mut element = div().id(SharedString::from(format!("file-line-row-{id}-{depth_count}-{row}")))
                    .h(px(ROW)).pl(px(8.0)).pr(px(2.0)).flex().items_center().gap(px(9.0)).rounded(px(5.0)).whitespace_nowrap()
                    .when(line.focus_is_row(&id, row), |row| row.bg(color(&t.hover)).border_1().border_color(accent))
                    .when(!usable, |row| row.opacity(0.55))
                    .when(usable, |row| row.cursor_pointer().hover(|style| style.bg(color(&t.hover))))
                    .child(icon(tokens, row_icon(item), 15.0).text_color(color(&t.text_muted)))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().child(text(item, "title")))
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| drag.update(cx, |line, cx| {
                        line.interaction.focus = Some(interaction::Focus::Sheet(drag_id.clone(), Some(row)));
                        line.begin_gesture(drag_node.clone(), false, cx);
                    }))
                    .on_click(move |event, _, cx| { let ctrl = event.modifiers().control; click.update(cx, |line, cx| line.activate_row(&click_id, click_node.clone(), ctrl, cx)); });
                if expandable(item) && usable {
                    let (drill, drill_id, drill_node) = (entity.clone(), id.clone(), item.clone());
                    element = element.child(div().id(SharedString::from(format!("file-line-in-{id}-{depth_count}-{row}"))).flex_none()
                        .w(px(24.0)).h(px(ROW - 6.0)).flex().items_center().justify_center().rounded(px(4.0)).text_color(color(&t.text_muted))
                        .hover(|style| style.bg(color(&t.hover))).child(icon(tokens, "chevron-right", 11.0).text_color(color(&t.text_muted)))
                        .on_click(move |_, _, cx| { cx.stop_propagation(); drill.update(cx, |line, cx| line.drill(&drill_id, drill_node.clone(), cx)); }));
                }
                rows = rows.child(element);
            }
            if level.has_more {
                let label = if level.loading { tr("正在读取…", "Loading…") } else { tr("向下滚动加载更多", "Scroll down for more") };
                rows = rows.child(note(label.into(), color(&t.text_muted)).text_size(px(11.0)));
            }
        }
        None => {}
    }
    let pinned = sheet.pinned;
    let (hover_entity, pin_entity, pin_id) = (entity.clone(), entity.clone(), id.clone());
    let clip = div().id(SharedString::from(format!("file-line-clip-{id}"))).absolute().left(px(SHEET_WIDTH / 2.0 - 12.0)).top(px(-13.0))
        .w(px(24.0)).h(px(24.0)).flex().justify_center().cursor_pointer()
        .child(peg(if pinned { accent } else { color(&t.text_muted) }, color(&t.surface), 12.0, 21.0, !pinned))
        .on_click(move |_, _, cx| { cx.stop_propagation(); pin_entity.update(cx, |line, cx| line.toggle_pin(&pin_id, cx)); });
    div().id(SharedString::from(format!("file-line-sheet-{id}"))).absolute().left(px(boxed.left)).top(px(boxed.top))
        .w(px(SHEET_WIDTH)).h(px(boxed.height)).flex().flex_col().rounded(px(10.0)).border_1()
        .border_color(if pinned { accent.opacity(0.4) } else { color(&t.border) })
        .bg(color(&t.background)).shadow_lg().text_size(px(12.0))
        .on_hover(move |hovered, _, cx| hover_entity.update(cx, |line, cx| line.hover_sheet(pinned, *hovered, cx)))
        .child(header).child(rows).child(clip)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, title: &str, path: &str) -> Value {
        json!({"id": title, "kind": "Item", "title": title, "role": role, "isEnabled": true, "payload": {"path": path}})
    }

    #[test]
    fn a_cancelled_root_reply_closes_its_session_but_an_accepted_one_stays_live() {
        cliphost_core::clipboard::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new().prefix("file-line-reply-").tempdir_in(root).unwrap();
        let host = Arc::new(HostState::open(
            cliphost_core::paths::AppPaths::from_data_root(dir.path().join("data")), 29065,
        ).unwrap());
        let before = host.super_menu_state_detail();
        let id = host.file_line_open_session(0)["sessionId"].as_str().unwrap().to_owned();
        drop(PendingMenuSession { host: host.clone(), id: Some(id.clone()) });
        assert!(!host.menu_close_session(&id), "a discarded reply must already be closed");
        let id = host.file_line_open_session(0)["sessionId"].as_str().unwrap().to_owned();
        let accepted = PendingMenuSession { host: host.clone(), id: Some(id.clone()) }.accept();
        assert_eq!(accepted, id);
        assert!(host.menu_close_session(&id), "an accepted reply transfers its live session");
        assert_eq!(host.super_menu_state_detail(), before);
    }

    #[test]
    fn root_rows_become_tags_with_knots_and_a_trailing_add() {
        let separator = json!({"id": "s", "kind": "Separator"});
        let items = vec![
            node("file-add-dir", "添加目录…", ""), node("file-add-opened-root", "添加已打开的文件夹…", ""), separator.clone(),
            node("opened-folder", "下载", r"C:\Users\demo\Downloads"), separator.clone(),
            node("file", "项目", r"D:\项目"), separator.clone(),
            node("file", "C:", r"C:\"), separator.clone(),
            node("recent-files", "最近使用的文件", r"C:\Recent"), separator.clone(), node("setting", "设置", ""),
        ];
        let (slots, add) = line_slots(&items);
        assert_eq!(add.unwrap()["role"], "file-add-dir");
        let kinds: Vec<Option<TagKind>> = slots.iter().map(|slot| match slot { Slot::Tag(tag) => Some(tag.kind), Slot::Knot => None }).collect();
        assert!(kinds == vec![Some(TagKind::Opened), None, Some(TagKind::Folder), None, Some(TagKind::Drive), None, Some(TagKind::Recent)],
            "leading maintenance rows and trailing settings leave no stray knots");
        let Slot::Tag(drive) = &slots[4] else { unreachable!() };
        assert_eq!(drive.label, "C:");
    }

    #[test]
    fn the_drop_settles_at_rest_and_fades_in_quickly() {
        assert_eq!(settle(1.0), (0.0, 1.0));
        let (start, alpha) = settle(0.0);
        assert!(start < -13.0 && alpha == 0.0);
        assert!(settle(0.5).0.abs() < 2.0, "most of the swing has died out by halfway");
    }

    #[test]
    fn the_cord_sags_most_in_the_middle() {
        assert_eq!(cord_y(0.0, 1000.0, CORD_Y, SAG), CORD_Y);
        assert_eq!(cord_y(500.0, 1000.0, CORD_Y, SAG), CORD_Y + SAG);
        assert!(cord_y(250.0, 1000.0, CORD_Y, SAG) < CORD_Y + SAG);
    }
}

#[cfg(test)]
#[path = "file_line_interaction_tests.rs"]
mod interaction_tests;
