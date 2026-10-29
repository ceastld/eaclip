//! GPUI hover card (2026-10-24). Mixed QQ text/images show as one card; file
//! items show a file list with a cascade preview (images and bounded text or
//! code). It sits beside the owner window like the native image preview,
//! never takes focus, and follows the hover ledger's tickets and leave grace.
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use cliphost_core::{i18n::tr, HostState};
use gpui::{
    canvas, div, img, prelude::*, px, relative, AnimationExt, AnyElement, App, Context,
    HighlightStyle, MouseButton, ScrollHandle, SharedString, StyledText, Task, Timer, Window,
    WindowHandle, WindowOptions,
};
use serde_json::json;

use crate::{
    crisp_svg::svg,
    hover::{CardAdmission, CardCommand, Content, FileCard, RichCard, RichPart},
    motion,
    native_content::ContentImage,
    platform,
    preview::DesignTokens,
    text_preview::{self, TextPreview, Token},
    view::color,
};

const MARGIN: f32 = 10.0;
const GAP: f32 = 8.0;
const CASCADE_GAP: f32 = 6.0;
const RICH_WIDTH: f32 = 380.0;
const FILES_WIDTH: f32 = 300.0;
const TEXT_CARD_WIDTH: f32 = 480.0;
const PREVIEW_WIDTH: f32 = 340.0;
const TEXT_PREVIEW_WIDTH: f32 = 460.0;
const MAX_HEIGHT: f32 = 460.0;
const FILES_HEAD: f32 = 36.0;
const FILE_ROW: f32 = 40.0;
const VISIBLE_ROWS: usize = 10;
const LEAVE_GRACE: Duration = Duration::from_millis(180);
/// Same session side rule as the native image preview (2026-10-24).
const COMFORT: f32 = 480.0;
/// Moving off an interactive file list may cross other rows; hold briefly.
const AIM_HOLD: Duration = Duration::from_millis(140);
const FIRST_FILE_HOVER: Duration = Duration::from_millis(120);
const GLIDE_STEPS: u32 = 8;
const CACHE_LIMIT: usize = 48;

type Rect = (f32, f32, f32, f32);

#[cfg(all(windows, debug_assertions))]
#[path = "hover_card_fixture.rs"]
pub mod fixture;

#[derive(Clone)]
enum Slot<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CardLayout {
    /// The card is right of the owner.
    pub right: bool,
    /// Physical screen rectangle of the popup window.
    pub window: (i32, i32, i32, i32),
    /// Logical offsets inside the window.
    pub card_x: f32,
    pub card_width: f32,
    pub preview_x: f32,
    pub preview_width: f32,
    pub height: f32,
    /// Logical top of the hovered row inside the window. The card starts
    /// there and moves up only as far as it would overflow the window.
    pub card_top: f32,
    /// Physical pixels per logical pixel on the card's monitor.
    pub scale: f32,
}

/// Beside the owner, never over it; the cascade preview sits on the outer
/// side. A new session takes the right side when it is comfortable or not the
/// smaller one; `locked` keeps a warm session's side unless the card cannot
/// fit there at all. `None` when not even the card fits.
pub(crate) fn place(
    owner: (i32, i32, i32, i32),
    anchor_top: i32,
    work: (i32, i32, i32, i32),
    scale: f64,
    card_width: f32,
    preview_width: f32,
    locked: Option<bool>,
) -> Option<CardLayout> {
    let s = scale as f32;
    let (owner_left, _, owner_right, _) = owner;
    let (work_left, work_top, work_right, work_bottom) = work;
    let gap = (GAP * s).round() as i32;
    let right_room = (work_right - owner_right - gap) as f32 / s;
    let left_room = (owner_left - gap - work_left) as f32 / s;
    // Room from the gap to the screen edge; the inner margin overlaps the gap.
    let need = |preview: f32| {
        card_width + MARGIN + if preview > 0.0 { CASCADE_GAP + preview } else { 0.0 }
    };
    let mut right = locked.unwrap_or(right_room >= COMFORT || right_room >= left_room);
    let fits = |room: f32| room >= need(0.0);
    if !fits(if right { right_room } else { left_room }) && fits(if right { left_room } else { right_room }) {
        right = !right;
    }
    let room = if right { right_room } else { left_room };
    if room < need(0.0) {
        return None;
    }
    let preview_width = if room >= need(preview_width) {
        preview_width
    } else {
        (room - need(0.0) - CASCADE_GAP).max(0.0).min(preview_width)
    };
    let preview_width = if preview_width < 160.0 { 0.0 } else { preview_width };
    let width = MARGIN + need(preview_width);
    let height = MAX_HEIGHT + 2.0 * MARGIN;
    let (card_x, preview_x) = if right {
        (MARGIN, MARGIN + card_width + CASCADE_GAP)
    } else if preview_width > 0.0 {
        (MARGIN + preview_width + CASCADE_GAP, MARGIN)
    } else {
        (MARGIN, MARGIN)
    };
    let physical_width = (width * s).round() as i32;
    let physical_height = (height * s).round() as i32;
    let margin = (MARGIN * s).round() as i32;
    let x = if right {
        owner_right + gap - margin
    } else {
        owner_left - gap + margin - physical_width
    };
    let y = (anchor_top - margin).clamp(work_top, (work_bottom - physical_height).max(work_top));
    Some(CardLayout {
        right,
        window: (x, y, physical_width, physical_height),
        card_x,
        card_width,
        preview_x,
        preview_width,
        height: MAX_HEIGHT,
        // The window is clamped to the work area; the card still follows the row.
        card_top: ((anchor_top - y) as f32 / s).clamp(MARGIN, MARGIN + MAX_HEIGHT),
        scale: s,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pointer {
    /// The ledger owns visibility; a row leave starts the grace.
    Anchored,
    Leaving(Instant),
    /// The pointer is inside the card or its preview.
    Inside,
}

struct PendingShow {
    command: CardCommand,
    at: Instant,
}

pub struct HoverCard {
    admission: Option<CardAdmission>,
    host: HostState,
    tokens: DesignTokens,
    dark: bool,
    owner: isize,
    ticket: u64,
    epoch: u64,
    content: Option<Content>,
    layout: Option<CardLayout>,
    visible: bool,
    pointer: Pointer,
    pending: Option<PendingShow>,
    focus: Option<usize>,
    focus_task: Option<Task<()>>,
    images: HashMap<String, Slot<ContentImage>>,
    texts: HashMap<String, Slot<TextPreview>>,
    sizes: HashMap<String, Option<u64>>,
    rich_scroll: ScrollHandle,
    list_scroll: ScrollHandle,
    text_scroll: ScrollHandle,
    row_tops: Rc<RefCell<HashMap<usize, f32>>>,
    regions: Rc<RefCell<HashMap<usize, Rect>>>,
    last_regions: Rc<RefCell<Vec<Rect>>>,
    watch: Option<Task<()>>,
    glide: Option<Task<()>>,
    /// Fixture captures paint a desktop-like backdrop behind the cards.
    backdrop: bool,
}

/// Wire the hover service to one lazily created card window.
#[cfg(windows)]
pub fn install(service: &crate::service::NativeHost, tokens: DesignTokens, cx: &mut App) {
    let Ok(hover) = service.hover_service() else {
        return;
    };
    let host = service.state();
    install_service(hover, host, tokens, cx);
}

#[cfg(windows)]
pub(crate) fn install_service(
    hover: Arc<crate::hover::HoverService>,
    host: Arc<HostState>,
    tokens: DesignTokens,
    cx: &mut App,
) {
    let admission = hover.card_admission();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<CardCommand>();
    hover.set_card_sink(Some(Arc::new(move |command| {
        let _ = sender.send(command);
    })));
    cx.spawn(async move |cx| {
        let mut card: Option<WindowHandle<HoverCard>> = None;
        while let Some(command) = receiver.recv().await {
            let result = cx.update(|cx| HoverCard::dispatch(&mut card, &host, &tokens, &admission, command, cx));
            if result.is_err() {
                break;
            }
        }
    })
    .detach();
}

impl HoverCard {
    fn dispatch(
        card: &mut Option<WindowHandle<Self>>,
        host: &HostState,
        tokens: &DesignTokens,
        admission: &CardAdmission,
        command: CardCommand,
        cx: &mut App,
    ) {
        if let CardCommand::Show { owner, ticket, .. } = &command {
            if !admission(*owner, *ticket) {
                return;
            }
        }
        if card.is_none() {
            if matches!(command, CardCommand::Hide { .. }) {
                return;
            }
            let (host, tokens) = (host.clone(), tokens.clone());
            match cx.open_window(
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
                    window.set_window_title(tr("ClipHost GPUI · 悬浮预览", "ClipHost GPUI · Hover preview"));
                    cx.new(|_| Self::new(host, tokens))
                },
            ) {
                Ok(handle) => *card = Some(handle),
                Err(error) => {
                    tracing::debug!(%error, "Hover card window unavailable");
                    return;
                }
            }
        }
        let Some(handle) = *card else { return };
        if handle
            .update(cx, |this, window, cx| {
                this.admission = Some(admission.clone());
                this.apply(command, window, cx);
            })
            .is_err()
        {
            *card = None;
        }
    }

    fn new(host: HostState, tokens: DesignTokens) -> Self {
        Self {
            admission: None,
            host,
            tokens,
            dark: false,
            owner: 0,
            ticket: 0,
            epoch: 0,
            content: None,
            layout: None,
            visible: false,
            pointer: Pointer::Anchored,
            pending: None,
            focus: None,
            focus_task: None,
            images: HashMap::new(),
            texts: HashMap::new(),
            sizes: HashMap::new(),
            rich_scroll: ScrollHandle::new(),
            list_scroll: ScrollHandle::new(),
            text_scroll: ScrollHandle::new(),
            row_tops: Rc::default(),
            regions: Rc::default(),
            last_regions: Rc::default(),
            watch: None,
            glide: None,
            backdrop: false,
        }
    }

    fn apply(&mut self, command: CardCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            CardCommand::Hide { owner, ticket, force } => {
                if ticket < self.ticket || (owner != self.owner && self.owner != 0) {
                    return;
                }
                self.ticket = ticket;
                self.pending = None;
                if !self.visible {
                    return;
                }
                if force {
                    self.hide(window, cx);
                } else if self.pointer == Pointer::Anchored {
                    self.pointer = Pointer::Leaving(Instant::now());
                }
            }
            CardCommand::Show { ticket, .. } if ticket < self.ticket => {}
            CardCommand::Show { ref content, warm, ticket, owner, .. } => {
                let interactive = matches!(&self.content, Some(Content::Files(card)) if card.entries().count() > 1);
                let same_item = self.same_item(content);
                let hold = self.visible && warm && interactive && !same_item && owner == self.owner;
                if hold {
                    self.ticket = ticket;
                    self.pending = Some(PendingShow { command, at: Instant::now() + AIM_HOLD });
                    return;
                }
                self.show(command, window, cx);
            }
        }
    }

    /// The same file card with another focused file updates in place.
    fn same_item(&self, content: &Content) -> bool {
        match (&self.content, content) {
            (Some(Content::Files(old)), Content::Files(new)) => old.paths == new.paths,
            (Some(old), new) => old == new,
            _ => false,
        }
    }

    fn show(&mut self, command: CardCommand, window: &mut Window, cx: &mut Context<Self>) {
        let CardCommand::Show { owner, ticket, anchor, beside, scale, dark, warm, content } = command else {
            return;
        };
        // Also runs when an interactive card's AIM_HOLD completes. The worker
        // admission check predates both the UI queue and this optional delay.
        if self.admission.as_ref().is_some_and(|admit| !admit(owner, ticket)) {
            return;
        }
        self.ticket = ticket;
        self.pending = None;
        self.pointer = Pointer::Anchored;
        if beside.is_none() && self.visible && owner == self.owner && self.same_item(&content) {
            if let Content::Files(card) = &content {
                if card.focus.is_some() {
                    self.set_focus(card.focus, Duration::ZERO, cx);
                }
            }
            cx.notify();
            return;
        }
        let Some(owner_rect) = beside.map(|r| (r[0], r[1], r[2], r[3])).or_else(|| window_rect(owner)) else {
            self.hide(window, cx);
            return;
        };
        let Some(area) = cliphost_core::window_place::overlay_work_area_from_point(anchor[0], anchor[1]) else {
            self.hide(window, cx);
            return;
        };
        let work = (
            area.work.left.round() as i32,
            area.work.top.round() as i32,
            (area.work.left + area.work.width).round() as i32,
            (area.work.top + area.work.height).round() as i32,
        );
        let (mut card_width, preview_width) = match &content {
            Content::Rich(_) => (RICH_WIDTH, 0.0),
            Content::Files(card) if card.entries().count() == 1 && card.focus.is_some() => (TEXT_CARD_WIDTH, 0.0),
            Content::Files(card) if card.entries().count() == 1 => (FILES_WIDTH, 0.0),
            Content::Files(card) => (
                FILES_WIDTH,
                if card.entries().any(|(_, path)| crate::hover::content::local_text_path(path)) {
                    TEXT_PREVIEW_WIDTH
                } else {
                    PREVIEW_WIDTH
                },
            ),
            _ => return,
        };
        if beside.is_some() {
            let room = ((work.2 - owner_rect.2).max(owner_rect.0 - work.0) as f32
                / area.dpi_scale_y as f32 - GAP - MARGIN).max(240.0);
            card_width = card_width.min(room);
        }
        let _ = scale;
        let locked = self.layout.filter(|_| warm && owner == self.owner).map(|layout| layout.right);
        let Some(layout) = place(owner_rect, anchor[1], work, area.dpi_scale_y, card_width, preview_width, locked) else {
            self.hide(window, cx);
            return;
        };
        let previous = self.layout.filter(|old| self.visible && warm && owner == self.owner
            && (beside.is_none() || (old.right == layout.right
                && (old.right || layout.window.2 <= old.window.2))));
        self.owner = owner;
        self.dark = dark;
        self.epoch += 1;
        self.layout = Some(layout);
        self.focus = None;
        self.focus_task = None;
        self.rich_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.list_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.text_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.row_tops.borrow_mut().clear();
        self.regions.borrow_mut().clear();
        self.last_regions.borrow_mut().clear();
        if let Content::Files(card) = &content {
            self.focus = card.focus;
            if beside.is_some() {
                for (_, path) in card.entries() {
                    self.texts.remove(path);
                    self.sizes.remove(path);
                }
            }
        }
        self.content = Some(content);
        self.load(cx);
        self.visible = true;
        cx.notify();
        if let Some(from) = previous.map(|previous| previous.window) {
            if from != layout.window && !motion::reduced_motion() {
                self.glide_to(from, layout.window, owner, window, cx);
                self.watch(window, cx);
                return;
            }
        }
        self.glide = None;
        let _ = platform::overlay_show(window, owner, layout.window);
        self.watch(window, cx);
    }

    fn glide_to(
        &mut self,
        from: (i32, i32, i32, i32),
        to: (i32, i32, i32, i32),
        owner: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.epoch;
        let _ = platform::overlay_show(window, owner, (from.0, from.1, to.2, to.3));
        self.glide = Some(cx.spawn_in(window, async move |this, cx| {
            for step in 1..=GLIDE_STEPS {
                Timer::after(Duration::from_millis(16)).await;
                let t = step as f32 / GLIDE_STEPS as f32;
                let eased = 1.0 - (1.0 - t).powi(3);
                let x = from.0 + ((to.0 - from.0) as f32 * eased).round() as i32;
                let y = from.1 + ((to.1 - from.1) as f32 * eased).round() as i32;
                let alive = this.update_in(cx, |this, window, _| {
                    if this.epoch != epoch || !this.visible {
                        return false;
                    }
                    let _ = platform::set_geometry(window, Some((x, y)), None);
                    true
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        }));
    }

    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = false;
        self.pending = None;
        self.content = None;
        self.focus = None;
        self.focus_task = None;
        self.watch = None;
        self.glide = None;
        self.pointer = Pointer::Anchored;
        let _ = platform::hide(window);
        cx.notify();
    }

    /// Leave grace, pointer-inside tracking, owner visibility and the
    /// delayed aim switch. Runs only while the card is visible.
    fn watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.watch.is_some() {
            return;
        }
        self.watch = Some(cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(40)).await;
            let keep = this.update_in(cx, |this, window, cx| {
                if !this.visible {
                    this.watch = None;
                    return false;
                }
                if !window_visible(this.owner) {
                    this.hide(window, cx);
                    return false;
                }
                let inside = this.cursor_inside();
                this.pointer = match (this.pointer, inside) {
                    (_, true) => Pointer::Inside,
                    (Pointer::Inside, false) => Pointer::Leaving(Instant::now()),
                    (pointer, false) => pointer,
                };
                if inside {
                    this.pending = None;
                }
                if let Some(pending) = this.pending.take() {
                    if Instant::now() >= pending.at {
                        this.show(pending.command, window, cx);
                    } else {
                        this.pending = Some(pending);
                    }
                }
                if matches!(this.pointer, Pointer::Leaving(since) if since.elapsed() >= LEAVE_GRACE) {
                    this.hide(window, cx);
                    return false;
                }
                true
            });
            if !matches!(keep, Ok(true)) {
                return;
            }
        }));
    }

    fn cursor_inside(&self) -> bool {
        let (Some(layout), Some((x, y))) = (self.layout, platform::cursor_position()) else {
            return false;
        };
        let scale = layout.scale;
        self.last_regions.borrow().iter().any(|(left, top, width, height)| {
            let left = layout.window.0 as f32 + left * scale;
            let top = layout.window.1 as f32 + top * scale;
            (x as f32) >= left && (x as f32) < left + width * scale && (y as f32) >= top && (y as f32) < top + height * scale
        })
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let mut images = Vec::new();
        let mut sizes = Vec::new();
        match &self.content {
            Some(Content::Rich(card)) => {
                images.extend(card.parts.iter().filter_map(|part| match part {
                    RichPart::Image(path) => Some(path.clone()),
                    RichPart::Text(_) => None,
                }).take(12));
            }
            Some(Content::Files(card)) => {
                for (index, path) in card.entries().take(200) {
                    sizes.push(path.to_owned());
                    if crate::hover::content::local_raster_path(path) && (index < 24 || card.focus == Some(index)) {
                        images.push(path.to_owned());
                    }
                }
            }
            _ => {}
        }
        if self.images.len() > CACHE_LIMIT {
            self.images.retain(|_, slot| matches!(slot, Slot::Loading));
        }
        if self.texts.len() > CACHE_LIMIT {
            self.texts.clear();
        }
        for path in images {
            self.load_image(path, cx);
        }
        let missing: Vec<_> = sizes.into_iter().filter(|path| !self.sizes.contains_key(path)).collect();
        if !missing.is_empty() {
            cx.spawn(async move |this, cx| {
                let measured = cx
                    .background_executor()
                    .spawn(async move {
                        missing
                            .into_iter()
                            .map(|path| {
                                let size = std::fs::metadata(&path).ok().filter(|meta| meta.is_file()).map(|meta| meta.len());
                                (path, size)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.sizes.extend(measured);
                    cx.notify();
                });
            })
            .detach();
        }
        if let Some(path) = self.focused_path() {
            self.load_preview(path, cx);
        }
    }

    fn load_image(&mut self, path: String, cx: &mut Context<Self>) {
        if self.images.contains_key(&path) {
            return;
        }
        self.images.insert(path.clone(), Slot::Loading);
        cx.spawn(async move |this, cx| {
            let source = path.clone();
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let metadata = std::fs::metadata(&source).map_err(|error| error.to_string())?;
                    if metadata.len() > 64 * 1024 * 1024 {
                        return Err("Image too large".to_owned());
                    }
                    let bytes = std::fs::read(&source).map_err(|error| error.to_string())?;
                    crate::native_content::list_image(bytes).ok_or_else(|| "Unsupported image".to_owned())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.images.insert(path, match loaded {
                    Ok(image) => Slot::Ready(image),
                    Err(error) => Slot::Failed(error),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn load_preview(&mut self, path: String, cx: &mut Context<Self>) {
        if crate::hover::content::local_raster_path(&path) {
            self.load_image(path, cx);
            return;
        }
        if !crate::hover::content::local_text_path(&path) || self.texts.contains_key(&path) {
            return;
        }
        self.texts.insert(path.clone(), Slot::Loading);
        cx.spawn(async move |this, cx| {
            let source = PathBuf::from(&path);
            let preview = cx
                .background_executor()
                .spawn(async move { text_preview::read_preview(&source) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.texts.insert(path, match preview {
                    Ok(preview) => Slot::Ready(preview),
                    Err(error) => Slot::Failed(error),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn focused_path(&self) -> Option<String> {
        let Some(Content::Files(card)) = &self.content else { return None };
        let focus = self.focus.or_else(|| (card.entries().count() == 1).then(|| card.entries().next().unwrap().0))?;
        card.paths.get(focus).map(|path| path.trim().to_owned())
    }

    fn set_focus(&mut self, focus: Option<usize>, delay: Duration, cx: &mut Context<Self>) {
        if delay.is_zero() || self.focus.is_some() {
            self.focus_task = None;
            self.focus = focus;
            self.text_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            if let Some(path) = self.focused_path() {
                self.load_preview(path, cx);
            }
            cx.notify();
            return;
        }
        let epoch = self.epoch;
        self.focus_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.epoch == epoch {
                    this.set_focus(focus, Duration::ZERO, cx);
                }
            });
        }));
    }

    fn open(&mut self, path: String, reveal: bool, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.clone();
        cx.background_executor()
            .spawn(async move {
                let params = json!({ "path": path });
                let result = if reveal { host.shell_reveal(&params) } else { host.shell_open_path(&params) };
                if let Err(error) = result {
                    tracing::warn!(%error, "Hover card file action failed");
                }
            })
            .detach();
        self.hide(window, cx);
    }

    fn icon(&self, key: &str, size: f32, tint: gpui::Hsla) -> Option<crate::crisp_svg::CrispSvg> {
        self.tokens
            .icons
            .get(key)
            .map(|path| svg().path(path.clone()).size(px(size)).flex_none().text_color(tint))
    }

    fn region_probe(&self, index: usize) -> AnyElement {
        let regions = self.regions.clone();
        canvas(
            move |bounds, _, _| {
                regions.borrow_mut().insert(
                    index,
                    (
                        f32::from(bounds.left()) - 6.0,
                        f32::from(bounds.top()) - 6.0,
                        f32::from(bounds.size.width) + 12.0,
                        f32::from(bounds.size.height) + 12.0,
                    ),
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// The main card in a column below a spacer as tall as the row offset.
    /// The spacer shrinks when the card would overflow, so the card's top is
    /// the row's top clamped to the window like the native preview.
    fn anchored(&self, layout: CardLayout, card: gpui::Div) -> AnyElement {
        div()
            .absolute()
            .left(px(layout.card_x))
            .top(px(MARGIN))
            .w(px(layout.card_width))
            .h(px(layout.height))
            .flex()
            .flex_col()
            .child(div().flex_shrink().min_h_0().h(px(layout.card_top - MARGIN)))
            .child(card.flex_none())
            .into_any_element()
    }

    fn surface(&self, width: f32) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        div()
            .relative()
            .w(px(width))
            .max_h(px(self.layout.map_or(MAX_HEIGHT, |layout| layout.height)))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(8.0))
            .border_1()
            .border_color(color(&t.border))
            .bg(color(&t.surface))
            .text_color(color(&t.text))
            .font_family(self.tokens.typography.font_family.clone())
            .text_size(px(12.5))
            .shadow_lg()
    }

    fn rich_card(&self, card: &RichCard, layout: CardLayout) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let inner = layout.card_width - 24.0;
        let mut body = div()
            .id("hover-card-rich")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.rich_scroll)
            .flex()
            .flex_col()
            .items_start()
            .gap(px(8.0))
            .px(px(12.0))
            .pb(px(10.0));
        for part in &card.parts {
            body = body.child(match part {
                RichPart::Text(text) => div()
                    .flex_none()
                    .line_height(relative(1.6))
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
                RichPart::Image(path) => self.image_block(path, inner, 200.0),
            });
        }
        let card = self.surface(layout.card_width)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .pt(px(10.0))
                    .pb(px(8.0))
                    .text_size(px(11.0))
                    .text_color(color(&t.text_muted))
                    .children(self.icon("image", 14.0, color(&t.text_muted)))
                    .child(format!("{} · {} {}", tr("图文", "Text and images"), card.images, tr("张图片", "images"))),
            )
            .child(body)
            .child(self.region_probe(0));
        self.anchored(layout, card)
    }

    fn image_block(&self, path: &str, max_width: f32, max_height: f32) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        match self.images.get(path) {
            Some(Slot::Ready(image)) => {
                let ratio = (max_width / image.width.max(1.0)).min(max_height / image.height.max(1.0)).min(1.0);
                // Scroll bodies would otherwise shrink the box and letterbox it.
                img(gpui::ImageSource::Image(image.image.clone()))
                    .flex_none()
                    .w(px((image.width * ratio).max(1.0)))
                    .h(px((image.height * ratio).max(1.0)))
                    .object_fit(gpui::ObjectFit::Contain)
                    .rounded(px(4.0))
                    .into_any_element()
            }
            Some(Slot::Failed(_)) => div()
                .text_color(color(&t.text_muted))
                .child(tr("[图片不可用]", "[Image unavailable]"))
                .into_any_element(),
            _ => div()
                .flex_none()
                .w(px(max_width))
                .h(px(max_height.min(96.0)))
                .rounded(px(4.0))
                .bg(color(&t.secondary))
                .into_any_element(),
        }
    }

    fn files_card(&self, card: &FileCard, layout: CardLayout, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let entries: Vec<(usize, String)> = card.entries().map(|(index, path)| (index, path.to_owned())).collect();
        let images = entries.iter().filter(|(_, path)| crate::hover::content::local_raster_path(path)).count();
        let mut list = div()
            .id("hover-card-files")
            .max_h(px(FILE_ROW * VISIBLE_ROWS as f32))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .flex()
            .flex_col()
            .px(px(4.0));
        let epoch = self.epoch;
        for (index, path) in entries.iter().take(200) {
            let (index, path) = (*index, path.clone());
            let name = file_name(&path);
            let active = self.focus == Some(index);
            let mut meta = self.sizes.get(&path).copied().flatten().map(format_size).unwrap_or_default();
            if entries.len() == 1 {
                // A single file row also shows where it lives.
                let folder = folder_of(&path);
                meta = if meta.is_empty() { folder } else { format!("{meta} · {folder}") };
            }
            let thumb: AnyElement = match self.images.get(&path) {
                Some(Slot::Ready(image)) => img(gpui::ImageSource::Image(image.image.clone()))
                    .w(px(36.0))
                    .h(px(26.0))
                    .object_fit(gpui::ObjectFit::Cover)
                    .rounded(px(3.0))
                    .into_any_element(),
                _ => div()
                    .w(px(36.0))
                    .flex()
                    .justify_center()
                    .children(self.icon(file_icon(&path), 16.0, color(&t.text_muted)))
                    .into_any_element(),
            };
            let tops = self.row_tops.clone();
            let (open_path, reveal_path) = (path.clone(), path.clone());
            list = list.child(
                div()
                    .id(SharedString::from(format!("hover-card-file-{index}")))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .h(px(FILE_ROW))
                    .flex_none()
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .when(active, |row| row.bg(color(&t.hover)))
                    .hover(|row| row.bg(color(&t.hover)))
                    .child(
                        canvas(move |bounds, _, _| {
                            tops.borrow_mut().insert(index, f32::from(bounds.top()));
                        }, |_, _, _, _| {})
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .child(thumb)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .flex_1()
                            .child(div().truncate().text_size(px(12.0)).child(name))
                            .child(div().truncate().text_size(px(10.5)).text_color(color(&t.text_muted)).child(meta)),
                    )
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.epoch == epoch {
                            this.set_focus(Some(index), FIRST_FILE_HOVER, cx);
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open(open_path.clone(), false, window, cx);
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open(reveal_path.clone(), true, window, cx);
                    })),
            );
        }
        let card = self.surface(layout.card_width)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .h(px(FILES_HEAD))
                    .px(px(12.0))
                    .pt(px(10.0))
                    .child(div().text_size(px(12.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(format!(
                        "{} {}",
                        entries.len(),
                        tr("个文件", "files")
                    )))
                    .child(div().text_size(px(11.0)).text_color(color(&t.text_muted)).child(if images > 0 {
                        format!("{images} {}", tr("张图片", "images"))
                    } else {
                        String::new()
                    })),
            )
            .child(list)
            .child(
                div()
                    .mt(px(4.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_t_1()
                    .border_color(color(&t.border))
                    .text_size(px(10.5))
                    .text_color(color(&t.text_muted))
                    .child(tr("单击打开 · 右键在资源管理器中定位", "Click to open · Right-click to reveal")),
            )
            .child(self.region_probe(0));
        self.anchored(layout, card)
    }

    /// One file's preview: image, bounded text/code, or file details.
    fn file_preview(&self, path: &str, width: f32) -> gpui::Div {
        let t = self.tokens.theme(self.dark).clone();
        let name = file_name(path);
        let size = self.sizes.get(path).copied().flatten().map(format_size);
        let mut panel = self.surface(width);
        if crate::hover::content::local_raster_path(path) {
            let caption = match self.images.get(path) {
                Some(Slot::Ready(image)) => format!("{} × {}{}", image.width as u32, image.height as u32,
                    size.map(|size| format!(" · {size}")).unwrap_or_default()),
                _ => size.unwrap_or_default(),
            };
            panel = panel
                .child(div().p(px(8.0)).flex().justify_center().child(self.image_block(path, width - 16.0, 300.0)))
                .child(caption_block(&t, name, caption));
        } else if crate::hover::content::local_text_path(path) {
            panel = panel.child(self.text_block(path, name, size, &t));
        } else {
            let detail = if crate::hover::content::local_video_path(path) {
                tr("视频文件 · 单击打开播放", "Video file · Click to play")
            } else {
                tr("此类型不提供内容预览", "No content preview for this type")
            };
            panel = panel
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.0))
                        .py(px(18.0))
                        .px(px(12.0))
                        .text_size(px(11.0))
                        .text_color(color(&t.text_muted))
                        .children(self.icon(file_icon(path), 28.0, color(&t.text_muted)))
                        .child(detail),
                )
                .child(caption_block(&t, name, folder_of(path)));
        }
        panel.child(self.region_probe(1))
    }

    fn text_block(&self, path: &str, name: String, size: Option<String>, t: &crate::preview::ThemeTokens) -> AnyElement {
        let header = |label: String| {
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .pt(px(9.0))
                .pb(px(7.0))
                .border_b_1()
                .border_color(color(&t.border))
                .child(div().flex_1().min_w_0().truncate().font_weight(gpui::FontWeight::MEDIUM).child(name.clone()))
                .child(div().flex_none().text_size(px(10.5)).text_color(color(&t.text_muted)).child(label))
        };
        match self.texts.get(path) {
            Some(Slot::Ready(preview)) => {
                let palette = syntax_palette(self.dark);
                let highlights = text_preview::highlight(preview.language, &preview.lines);
                let mut body = div()
                    .id("hover-card-text")
                    .max_h(px(360.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.text_scroll)
                    .px(px(12.0))
                    .py(px(8.0))
                    .font_family(monospace_family())
                    .text_size(px(12.0))
                    .line_height(relative(1.5));
                for (line, spans) in preview.lines.iter().zip(highlights) {
                    let styled = StyledText::new(if line.is_empty() { " ".to_owned() } else { line.clone() })
                        .with_highlights(spans.into_iter().map(|(range, token)| {
                            (range, HighlightStyle { color: Some(palette(token)), ..Default::default() })
                        }));
                    body = body.child(div().whitespace_nowrap().child(styled));
                }
                let mut label = preview.language.label().to_owned();
                if let Some(size) = size {
                    label.push_str(&format!(" · {size}"));
                }
                if preview.encoding != "UTF-8" {
                    label.push_str(&format!(" · {}", preview.encoding));
                }
                let mut block = div().flex().flex_col().child(header(label)).child(body);
                if preview.truncated {
                    block = block.child(
                        div()
                            .px(px(12.0))
                            .py(px(5.0))
                            .border_t_1()
                            .border_color(color(&t.border))
                            .text_size(px(10.5))
                            .text_color(color(&t.text_muted))
                            .child(format!("{} {} {}", tr("仅预览前", "Showing the first"), preview.lines.len(), tr("行", "lines"))),
                    );
                }
                block.into_any_element()
            }
            Some(Slot::Failed(error)) => div()
                .flex()
                .flex_col()
                .child(header(size.unwrap_or_default()))
                .child(div().p(px(12.0)).text_color(color(&t.text_muted)).child(if error == "Binary file" {
                    tr("不是文本文件，无法预览", "Not a text file").to_owned()
                } else {
                    format!("{} {error}", tr("无法读取：", "Cannot read:"))
                }))
                .into_any_element(),
            _ => div()
                .flex()
                .flex_col()
                .child(header(size.unwrap_or_default()))
                .child(div().h(px(120.0)))
                .into_any_element(),
        }
    }
}

impl Render for HoverCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .size_full()
            .relative()
            .when(self.backdrop, |root| root.bg(color(&self.tokens.theme(self.dark).secondary)));
        let (Some(layout), Some(content), true) = (self.layout, self.content.clone(), self.visible) else {
            return root.into_any_element();
        };
        let mut root = root;
        let epoch = self.epoch;
        match &content {
            Content::Rich(card) => {
                root = root.child(self.rich_card(card, layout));
            }
            Content::Files(card) if card.entries().count() == 1 && card.focus.is_some() => {
                let path = card.entries().next().unwrap().1.to_owned();
                root = root.child(self.anchored(layout, self.file_preview(&path, layout.card_width)));
            }
            Content::Files(card) => {
                root = root.child(self.files_card(card, layout, cx));
                if let (Some(focus), true) = (self.focus, layout.preview_width > 0.0) {
                    if let Some(path) = card.paths.get(focus).map(|path| path.trim().to_owned()) {
                        let row_top = self.row_tops.borrow().get(&focus).copied()
                            .unwrap_or(layout.card_top + FILES_HEAD + focus as f32 * FILE_ROW);
                        let estimate = if crate::hover::content::local_text_path(&path) { 420.0 } else { 340.0 };
                        let top = (row_top - 8.0).clamp(MARGIN, (MARGIN + layout.height - estimate).max(MARGIN));
                        root = root.child(
                            div()
                                .child(self.file_preview(&path, layout.preview_width).absolute()
                                    .left(px(layout.preview_x)).top(px(top)))
                                .with_animation(
                                    SharedString::from(format!("hover-card-preview-{epoch}-{focus}")),
                                    motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                                    |element, value| element.opacity(value),
                                ),
                        );
                    }
                }
            }
            _ => {}
        }
        if !matches!(&content, Content::Files(card) if card.entries().count() > 1) || self.focus.is_none() {
            self.regions.borrow_mut().remove(&1);
        }
        let regions = self.regions.clone();
        let previous = self.last_regions.clone();
        let handle = window.window_handle();
        div()
            .size_full()
            .child(root.with_animation(
                SharedString::from(format!("hover-card-{epoch}")),
                motion::enter(Duration::from_millis(110), motion::EASE_OUT),
                |element, value| element.opacity(0.4 + 0.6 * value),
            ))
            .child(
                canvas(
                    move |_, _, cx| {
                        let mut indexed: Vec<_> = regions.borrow().iter().map(|(index, rect)| (*index, *rect)).collect();
                        indexed.sort_by(|a, b| a.0.cmp(&b.0));
                        let rectangles: Vec<_> = indexed.into_iter().map(|(_, rect)| rect).collect();
                        if rectangles.is_empty() || *previous.borrow() == rectangles {
                            return;
                        }
                        *previous.borrow_mut() = rectangles.clone();
                        cx.defer(move |cx| {
                            let _ = handle.update(cx, |_, window, _| platform::overlay_set_regions(window, rectangles));
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .into_any_element()
    }
}

fn caption_block(t: &crate::preview::ThemeTokens, name: String, detail: String) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(12.0))
        .pb(px(10.0))
        .child(div().truncate().font_weight(gpui::FontWeight::MEDIUM).child(name))
        .child(div().truncate().text_size(px(11.0)).text_color(color(&t.text_muted)).child(detail))
        .into_any_element()
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}
fn folder_of(path: &str) -> String {
    Path::new(path).parent().map(|parent| parent.display().to_string()).unwrap_or_default()
}
fn file_icon(path: &str) -> &'static str {
    if crate::hover::content::local_raster_path(path) {
        "image"
    } else if crate::hover::content::local_text_path(path) {
        "text"
    } else {
        "file"
    }
}

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn monospace_family() -> SharedString {
    SharedString::from("Cascadia Mono")
}

/// One Light / One Dark token colors.
fn syntax_palette(dark: bool) -> impl Fn(Token) -> gpui::Hsla {
    move |token| {
        let hex = match (dark, token) {
            (false, Token::Keyword) => 0xa626a4,
            (false, Token::String) => 0x50a14f,
            (false, Token::Comment) => 0xa0a1a7,
            (false, Token::Number) => 0x986801,
            (false, Token::Name) => 0x4078f2,
            (false, Token::Meta) => 0x0184bc,
            (true, Token::Keyword) => 0xc678dd,
            (true, Token::String) => 0x98c379,
            (true, Token::Comment) => 0x7f848e,
            (true, Token::Number) => 0xd19a66,
            (true, Token::Name) => 0x61afef,
            (true, Token::Meta) => 0x56b6c2,
        };
        gpui::rgb(hex).into()
    }
}

#[cfg(windows)]
fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::{Foundation::{HWND, RECT}, UI::WindowsAndMessaging::GetWindowRect};
    let mut rect = RECT::default();
    unsafe { GetWindowRect(HWND(hwnd as *mut _), &mut rect) }.ok()?;
    Some((rect.left, rect.top, rect.right, rect.bottom))
}
#[cfg(not(windows))]
fn window_rect(_: isize) -> Option<(i32, i32, i32, i32)> {
    None
}
#[cfg(windows)]
fn window_visible(hwnd: isize) -> bool {
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::IsWindowVisible};
    hwnd != 0 && unsafe { IsWindowVisible(HWND(hwnd as *mut _)) }.as_bool()
}
#[cfg(not(windows))]
fn window_visible(_: isize) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_prefers_the_right_side_and_puts_the_preview_outside() {
        let work = (0, 0, 1920, 1040);
        let layout = place((100, 100, 500, 800), 300, work, 1.0, FILES_WIDTH, PREVIEW_WIDTH, None).unwrap();
        assert_eq!(layout.window.0, 500 + 8 - 10);
        assert_eq!((layout.card_x, layout.preview_x), (MARGIN, MARGIN + FILES_WIDTH + CASCADE_GAP));
        assert_eq!(layout.window.1, 290);
        let left = place((1400, 100, 1900, 800), 300, work, 1.0, FILES_WIDTH, PREVIEW_WIDTH, None).unwrap();
        assert!(left.window.0 + left.window.2 <= 1400 - 8 + 10);
        assert!(left.preview_x < left.card_x, "the cascade stays on the outer side");
    }

    #[test]
    fn card_shrinks_or_drops_the_preview_and_never_covers_the_owner() {
        let work = (0, 0, 1000, 800);
        let tight = place((0, 0, 500, 700), 20, work, 1.0, FILES_WIDTH, PREVIEW_WIDTH, None).unwrap();
        assert!(tight.preview_width < PREVIEW_WIDTH);
        assert!(tight.window.0 >= 500 - 10 + 8);
        assert!(place((0, 0, 950, 700), 20, work, 1.0, RICH_WIDTH, 0.0, None).is_none());
        let scaled = place((100, 100, 500, 800), 300, (0, 0, 3840, 2080), 2.0, RICH_WIDTH, 0.0, None).unwrap();
        assert_eq!(scaled.window.2, ((2.0 * MARGIN + RICH_WIDTH) * 2.0) as i32);
        assert!(scaled.window.1 + scaled.window.3 <= 2080);
    }

    #[test]
    fn card_side_comes_from_room_and_stays_for_a_warm_session() {
        let work = (0, 0, 1120, 640);
        // Right 212, left 552: start on the left whatever the card needs.
        let owner = (560, 40, 900, 600);
        assert!(!place(owner, 100, work, 1.0, RICH_WIDTH, 0.0, None).unwrap().right);
        // A locked right side holds while the card still fits there...
        // Right 432, left 332: both fit a 300 card.
        let owner = (340, 40, 680, 600);
        assert!(place(owner, 100, work, 1.0, FILES_WIDTH, 0.0, Some(true)).unwrap().right);
        assert!(!place(owner, 100, work, 1.0, FILES_WIDTH, 0.0, Some(false)).unwrap().right);
        // ...and only moves when the card cannot fit on it at all.
        let owner = (560, 40, 900, 600);
        assert!(!place(owner, 100, work, 1.0, FILES_WIDTH, 0.0, Some(true)).unwrap().right);
    }

    #[test]
    fn card_follows_rows_after_the_window_is_clamped_to_the_work_area() {
        let work = (0, 0, 1920, 1000);
        let owner = (100, 100, 500, 1000);
        let top = place(owner, 200, work, 1.0, FILES_WIDTH, 0.0, None).unwrap();
        assert_eq!((top.window.1, top.card_top), (190, MARGIN));
        // Lower rows share one clamped window; the card top still differs.
        let low = place(owner, 800, work, 1.0, FILES_WIDTH, 0.0, None).unwrap();
        let lower = place(owner, 900, work, 1.0, FILES_WIDTH, 0.0, None).unwrap();
        assert_eq!(low.window, lower.window);
        assert_eq!(low.window.1 + low.window.3, 1000);
        assert_eq!(low.window.1 as f32 + low.card_top, 800.0);
        assert_eq!(lower.window.1 as f32 + lower.card_top, 900.0);
        let scaled = place(owner, 1600, (0, 0, 3840, 2000), 2.0, FILES_WIDTH, 0.0, None).unwrap();
        assert_eq!(scaled.window.1 as f32 + scaled.card_top * 2.0, 1600.0);
    }

    #[test]
    fn sizes_are_short_labels() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(4096), "4.0 KB");
        assert_eq!(format_size(3 * 1024 * 1024 + 1024 * 100), "3.1 MB");
    }
}
