//! Native CeaBox. Search and execution stay in HostState; this view owns only
//! presentation, committed input, selection and generation-scoped workers.

use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use cliphost_core::{
    ceabox::{CeaBoxItem, CeaBoxPage, CeaBoxSearchUpdate, CeaBoxSource},
    host::HostState,
    i18n::{tr, Text},
};
use gpui::{
    div, list, prelude::*, px, relative, size, AnyElement, App, Bounds, Context, Entity,
    FocusHandle, FontWeight, HighlightStyle, Hsla, KeyDownEvent, ListAlignment, ListState,
    MouseButton, SharedString, StyledText, Subscription, Task, Timer, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use gpui_rsx::rsx;
use serde_json::{json, Value};

use crate::{
    crisp_svg::svg,
    input::{InputColors, InputWindowDrag, SearchEvent, SearchInput},
    motion::{self, Tweens},
    platform,
    preview::DesignTokens,
    scrollbar::{ScrollbarState, ScrollbarStyle, ScrollbarTarget},
    view::color,
};

const PAGE_SIZE: usize = 100;
const FIXTURE_JSON: &str = include_str!("../../../../ui-design/ceabox.fixture.json");
const SEARCH_PLACEHOLDER: Text = Text("搜索收藏、剪贴板、动作…", "Search favorites, clipboard and actions…");

/// Dropping the UI task alone cannot interrupt a synchronous worker. This lease
/// retires scans and the in-flight action RPC on query changes and view teardown.
#[derive(Default)]
struct SearchLease(Arc<AtomicBool>);
impl Drop for SearchLease {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// watch holds a single latest snapshot, so a busy UI never accumulates batches.
#[derive(Clone, Default)]
struct SearchDelivery {
    update: Option<Arc<CeaBoxSearchUpdate>>,
    finished: bool,
    error: Option<String>,
}

impl SearchDelivery {
    fn publish_update(
        &mut self,
        sender: &tokio::sync::watch::Sender<Self>,
        update: CeaBoxSearchUpdate,
        preserving: bool,
        retain_count: usize,
    ) {
        let publish = !preserving || (update.complete && retain_count <= PAGE_SIZE);
        self.update = Some(Arc::new(update));
        // A first-page refresh cannot replace a longer list without losing its
        // selection and scroll anchor. The merged final range is sent below.
        if publish {
            sender.send_replace(self.clone());
        }
    }
}

fn source_name(source: CeaBoxSource) -> &'static str {
    match source {
        CeaBoxSource::All => "全部",
        CeaBoxSource::Favorite => "收藏",
        CeaBoxSource::Clipboard => "剪贴板",
        CeaBoxSource::Action => "动作",
    }
}

// Keep source_name/row_key stable across language changes; only labels translate.
fn source_label(source: CeaBoxSource) -> Text {
    match source {
        CeaBoxSource::All => Text("全部", "All"),
        CeaBoxSource::Favorite => Text("收藏", "Favorites"),
        CeaBoxSource::Clipboard => Text("剪贴板", "Clipboard"),
        CeaBoxSource::Action => Text("动作", "Actions"),
    }
}

fn row_key(item: &CeaBoxItem) -> String {
    format!("{}:{}", source_name(item.source), item.id)
}

fn retain_selection(
    previous: Option<String>,
    previous_index: usize,
    keys: &[String],
) -> Option<String> {
    previous.filter(|key| keys.contains(key)).or_else(|| {
        keys.get(previous_index.min(keys.len().saturating_sub(1)))
            .cloned()
    })
}

fn display_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_string())
}

fn icon_name(item: &CeaBoxItem) -> &'static str {
    if item.is_folder {
        return "folder";
    }
    if item.source == CeaBoxSource::Action {
        return "play";
    }
    let kind = item.kind.to_ascii_lowercase();
    if kind == "actionclipitem" {
        "play"
    } else if kind.contains("image") {
        "image"
    } else if kind.contains("file") {
        "file"
    } else if kind.contains("link") || kind.contains("url") {
        "link"
    } else if kind.contains("code") || kind.contains("command") {
        "code"
    } else {
        "text"
    }
}

fn action_label(item: Option<&CeaBoxItem>) -> Text {
    match item {
        Some(item) if item.is_folder => Text("打开", "Open"),
        Some(item) if item.source == CeaBoxSource::Action => Text("运行", "Run"),
        Some(item)
            if item.source == CeaBoxSource::Favorite
                && item.kind.eq_ignore_ascii_case("ActionClipItem") =>
        {
            Text("运行", "Run")
        }
        Some(item)
            if item.source == CeaBoxSource::Favorite
                && matches!(icon_name(item), "link" | "file") =>
        {
            Text("打开", "Open")
        }
        _ => Text("粘贴", "Paste"),
    }
}

fn action_name(item: Option<&CeaBoxItem>) -> &'static str {
    action_label(item).get()
}

fn title_text(title: &str, query: &str) -> StyledText {
    let lowered = title.to_ascii_lowercase();
    let mut ranges = query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .flat_map(|term| {
            let term = term.to_ascii_lowercase();
            lowered
                .match_indices(&term)
                .map(|(start, found)| start..start + found.len())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    StyledText::new(title.to_string()).with_highlights(merged.into_iter().map(|range| {
        (
            range,
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        )
    }))
}

struct Tip {
    text: SharedString,
    tokens: DesignTokens,
    dark: bool,
}
impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark);
        rsx! { <div px_2 py_1 max_w={px(400.0)} rounded={px(4.0)} border_1
        bg={color(&t.surface)} border_color={color(&t.border)}
        text_size={px(12.0)} text_color={color(&t.text)}>{self.text.clone()}</div> }
    }
}

pub struct CeaBoxWindow {
    host: Option<Arc<HostState>>,
    tokens: DesignTokens,
    dark: bool,
    search: Entity<SearchInput>,
    focus: FocusHandle,
    _subscription: Subscription,
    _window_drag_subscription: Subscription,
    window_drag_requests: usize,
    source: CeaBoxSource,
    query: String,
    items: Vec<CeaBoxItem>,
    total: usize,
    has_more: bool,
    selected: Option<String>,
    pressed: Option<(String, u64)>,
    list: ListState,
    scrollbar: ScrollbarState,
    active: bool,
    pinned: bool,
    had_activation: bool,
    native_initial_size: Option<(f64, f64)>,
    hwnd: isize,
    display: u64,
    query_generation: u64,
    loading: bool,
    refresh_pending: bool,
    preserving: bool,
    busy: bool,
    error: Option<String>,
    source_notice: Option<String>,
    feedback: Option<(String, Instant)>,
    load: Option<Task<()>>,
    search_lease: Option<SearchLease>,
    results_generation: Option<u64>,
    search_started: Instant,
    first_results_ms: Option<u128>,
    complete_ms: Option<u128>,
    search_updates: usize,
    operation: u64,
    hovered: HashSet<SharedString>,
    motion: RefCell<Tweens<SharedString>>,
    fixture_items: Vec<CeaBoxItem>,
    fixture_defaults: Vec<String>,
    fixture_search: std::collections::HashMap<String, Vec<String>>,
}

impl CeaBoxWindow {
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut App,
    ) -> Result<(), String> {
        if !host.ceabox_native_snapshot().visible {
            return Ok(());
        }
        let incoming = tokens.clone();
        let handle = if let Some(handle) = existing {
            *handle
        } else {
            let build_host = host.clone();
            let p = tokens.ceabox_proposal;
            let bounds = Bounds::centered(None, size(px(p.width), px(p.height)), cx);
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        show: false,
                        focus: false,
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(p.min_width), px(p.min_height))),
                        ..Default::default()
                    },
                    move |window, cx| {
                        window.set_window_title("CeaBox");
                        let view =
                            cx.new(|cx| Self::new(Some(build_host), tokens, dark, window, cx));
                        let weak = view.downgrade();
                        window.on_window_should_close(cx, move |window, cx| {
                            let _ = weak.update(cx, |this, cx| this.dismiss(window, cx));
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
                this.set_design_tokens(incoming, dark, cx);
                this.initialize_native_window(window, None, false)?;
                #[cfg(windows)]
                {
                    this.hwnd = platform::hwnd(window)?.0 as isize;
                }
                host.ceabox_set_hwnd(this.hwnd);
                let snapshot = host.ceabox_native_snapshot();
                this.display = snapshot.generation;
                this.source = snapshot.source;
                this.pinned = snapshot.pinned;
                this.active = true;
                this.had_activation = false;
                this.busy = false;
                this.operation = this.operation.wrapping_add(1);
                this.feedback = None;
                let owner = host.clone();
                let generation = this.display;
                let _guard = platform::guard_condition(move || {
                    let snapshot = owner.ceabox_native_snapshot();
                    snapshot.visible && snapshot.generation == generation
                });
                platform::set_no_activate(window, false)?;
                platform::show(window, true, true)?;
                this.search.read(cx).focus_handle().focus(window);
                this.reload(cx);
                Ok::<_, String>(())
            })
            .map_err(|error| error.to_string())??;
        Ok(())
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| this.suspend(window, cx));
    }

    /// Shared by production Show and the isolated renderer. Native tool-window
    /// style changes can replace GPUI's initial placement; assert the logical
    /// client size once, then preserve every user resize/move on later Shows.
    /// Only hidden-window configuration bypasses a canceled Show's guards.
    /// Showing/activation remains under the caller's presentation permissions.
    pub fn initialize_native_window(
        &mut self,
        window: &Window,
        requested_size: Option<(f64, f64)>,
        no_activate: bool,
    ) -> Result<(), String> {
        if self.native_initial_size.is_none() {
            let p = self.tokens.ceabox_proposal;
            let client_size = requested_size.unwrap_or((f64::from(p.width), f64::from(p.height)));
            if !client_size.0.is_finite()
                || !client_size.1.is_finite()
                || client_size.0 <= 0.0
                || client_size.1 <= 0.0
            {
                return Err("Invalid CeaBox initial client size.".into());
            }
            {
                // The root also has a ChromeTask guard. Replace it only while
                // queuing self-owned style/geometry jobs, never Show/activate.
                // Thus Hide before dispatch cannot skip size initialization
                // while leaving a permanently initialized view behind.
                let _configuration = platform::guard_condition(|| true);
                platform::configure_host(window)?;
                platform::initialize_ceabox_geometry(window, client_size)?;
            }
            self.native_initial_size = Some(client_size);
        }
        platform::set_no_activate(window, no_activate)
    }

    pub fn maintain(handle: WindowHandle<Self>, host: &HostState, dark: bool, cx: &mut App) {
        let snapshot = host.ceabox_native_snapshot();
        let _ = handle.update(cx, |this, window, cx| {
            if !snapshot.visible || snapshot.generation != this.display {
                if this.active {
                    this.suspend(window, cx);
                }
                return;
            }
            this.pinned = snapshot.pinned;
            if window.is_window_active() {
                if !this.had_activation {
                    this.search.read(cx).focus_handle().focus(window);
                }
                this.had_activation = true;
            } else {
                this.search
                    .update(cx, |input, _| input.cancel_window_drag());
                if this.had_activation && !this.pinned && !snapshot.executing {
                    host.ceabox_hide();
                    this.suspend(window, cx);
                    return;
                }
            }
            if this.dark != dark {
                this.set_design_tokens(this.tokens.clone(), dark, cx);
            }
            if this.refresh_pending
                && !this.busy
                && !this.loading
                && !this.search.read(cx).is_composing()
            {
                this.refresh_preserving(cx);
            }
            if let Some(error) = platform::take_error(window).ok().flatten() {
                this.error = Some(error);
                cx.notify();
            }
            if this
                .feedback
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_millis(2200))
            {
                this.feedback = None;
                cx.notify();
            }
        });
    }

    pub fn refresh(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, _, cx| {
            if this.active {
                if this.busy || this.loading || this.search.read(cx).is_composing() {
                    this.refresh_pending = true;
                } else {
                    this.refresh_preserving(cx);
                }
            }
        });
    }

    fn new(
        host: Option<Arc<HostState>>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| SearchInput::new(window, cx));
        search.update(cx, |input, _| input.set_window_draggable(true));
        let subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(query) => {
                    this.query = query.clone();
                    this.reload(cx);
                }
                SearchEvent::Submit => this.execute(window, cx),
                SearchEvent::Escape => this.dismiss(window, cx),
            });
        let window_drag_subscription = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputWindowDrag, window, cx| {
                this.begin_window_drag(event.0, window, cx);
            },
        );
        let mut this = Self {
            host,
            tokens,
            dark,
            search,
            focus: cx.focus_handle(),
            _subscription: subscription,
            _window_drag_subscription: window_drag_subscription,
            window_drag_requests: 0,
            source: CeaBoxSource::All,
            query: String::new(),
            items: Vec::new(),
            total: 0,
            has_more: false,
            selected: None,
            pressed: None,
            list: ListState::new(0, ListAlignment::Top, px(100.0)),
            scrollbar: ScrollbarState::default(),
            active: false,
            pinned: false,
            had_activation: false,
            native_initial_size: None,
            hwnd: 0,
            display: 0,
            query_generation: 0,
            loading: false,
            refresh_pending: false,
            preserving: false,
            busy: false,
            error: None,
            source_notice: None,
            feedback: None,
            load: None,
            search_lease: None,
            results_generation: None,
            search_started: Instant::now(),
            first_results_ms: None,
            complete_ms: None,
            search_updates: 0,
            operation: 0,
            hovered: HashSet::new(),
            motion: RefCell::new(Tweens::default()),
            fixture_items: Vec::new(),
            fixture_defaults: Vec::new(),
            fixture_search: Default::default(),
        };
        this.configure_input(cx);
        this
    }

    /// The same renderer with immutable, fictional records and no host services.
    #[cfg(debug_assertions)]
    pub fn new_fixture(
        tokens: DesignTokens,
        dark: bool,
        query: &str,
        source: CeaBoxSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(None, tokens, dark, window, cx);
        let fixture: Value = serde_json::from_str(FIXTURE_JSON).expect("CeaBox fixture JSON");
        this.fixture_defaults = fixture["defaultItemIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        for value in fixture["items"].as_array().into_iter().flatten() {
            let fields = ["subtitle", "content", "context", "title"]
                .into_iter()
                .map(|key| value[key].as_str().unwrap_or_default().to_string())
                .chain(
                    value["aliases"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                )
                .collect();
            this.fixture_search
                .insert(value["id"].as_str().unwrap_or_default().into(), fields);
        }
        this.fixture_items = fixture["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|value| {
                let mut value = value.clone();
                value["target"] = json!({"source":value["source"], "id":value["id"]});
                value["isFolder"] = Value::Bool(false);
                serde_json::from_value(value).expect("CeaBox fixture items match the typed DTO")
            })
            .collect();
        this.source = source;
        this.query = query.into();
        this.active = true;
        this.search
            .update(cx, |input, cx| input.set_text(query.into(), window, cx));
        this.reload(cx);
        this
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        self.configure_input(cx);
        cx.notify();
    }

    fn configure_input(&mut self, cx: &mut Context<Self>) {
        let t = self.tokens.theme(self.dark);
        let p = self.tokens.ceabox_proposal;
        let colors = InputColors {
            background: color(&t.surface),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: gpui::transparent_black(),
            selection: color(&t.selected),
            caret: color(&t.accent),
        };
        self.search.update(cx, |input, cx| {
            input.set_colors(colors, cx);
            input.set_metrics(30.0, p.search_font_size, 0.0, cx);
            input.set_placeholder(SEARCH_PLACEHOLDER.get(), cx);
        });
    }

    fn suspend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |input, _| input.cancel_window_drag());
        self.active = false;
        self.search_lease = None;
        self.load = None;
        self.loading = false;
        self.refresh_pending = false;
        self.preserving = false;
        self.query_generation = self.query_generation.wrapping_add(1);
        self.operation = self.operation.wrapping_add(1);
        self.busy = false;
        self.scrollbar.cancel();
        self.hovered.clear();
        self.motion.borrow_mut().clear();
        let _ = platform::hide(window);
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            host.ceabox_hide();
        }
        self.suspend(window, cx);
    }

    fn begin_window_drag(
        &mut self,
        origin: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.current() || self.search.read(cx).is_composing() {
            return;
        }
        self.window_drag_requests += 1;
        let result = if let Some(host) = &self.host {
            let owner = host.clone();
            let generation = self.display;
            let _permission = platform::guard_condition(move || {
                let snapshot = owner.ceabox_native_snapshot();
                snapshot.visible && snapshot.generation == generation
            });
            platform::begin_host_move_at_client(window, host, "ceabox", origin)
        } else {
            platform::begin_owned_move_at_client(window, origin)
        };
        if let Err(error) = result {
            self.error = Some(error);
        }
        cx.notify();
    }

    fn current(&self) -> bool {
        self.active
            && self.host.as_ref().is_none_or(|host| {
                let snapshot = host.ceabox_native_snapshot();
                snapshot.visible
                    && snapshot.generation == self.display
                    && snapshot.hwnd == self.hwnd
            })
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.current() {
            return;
        }
        self.retire_execution();
        self.refresh_pending = false;
        self.preserving = false;
        self.query_generation = self.query_generation.wrapping_add(1);
        self.search_lease = None;
        self.load = None;
        self.loading = false;
        self.pressed = None;
        self.selected = None;
        self.feedback = None;
        self.error = None;
        self.source_notice = None;
        // Keep the previous presentation until the first current snapshot. It
        // is dimmed and cannot execute; no blank flash between fast keystrokes.
        self.results_generation = None;
        self.search_started = Instant::now();
        self.first_results_ms = None;
        self.complete_ms = None;
        self.search_updates = 0;
        self.total = 0;
        self.has_more = false;
        self.scrollbar.cancel();
        self.hovered.clear();
        self.motion.borrow_mut().clear();
        if self.host.is_none() {
            self.items = self.fixture_results();
            self.results_generation = Some(self.query_generation);
            self.total = self.items.len();
            self.selected = self.items.first().map(row_key);
            self.list.reset(self.items.len());
            cx.notify();
            return;
        }
        self.load_page(0, cx);
    }

    /// Store/event refreshes do not retire an accepted execution. They wait for
    /// its completion and keep the current source/query, selection and viewport.
    fn refresh_preserving(&mut self, cx: &mut Context<Self>) {
        if !self.current() {
            return;
        }
        self.refresh_pending = false;
        if self.host.is_none() {
            return;
        }
        self.preserving = true;
        self.query_generation = self.query_generation.wrapping_add(1);
        // Same-query data refresh keeps already accepted targets usable.
        self.results_generation = Some(self.query_generation);
        self.search_lease = None;
        self.load = None;
        self.loading = false;
        self.pressed = None;
        self.has_more = false;
        self.load_page(0, cx);
    }

    fn fixture_results(&self) -> Vec<CeaBoxItem> {
        let query = self.query.trim().to_lowercase();
        let mut rows: Vec<_> = self
            .fixture_items
            .iter()
            .filter(|item| {
                (self.source == CeaBoxSource::All || item.source == self.source)
                    && (query.is_empty()
                        || query.split_whitespace().all(|term| {
                            self.fixture_search.get(&item.id).is_some_and(|fields| {
                                fields
                                    .iter()
                                    .any(|field| field.to_lowercase().contains(term))
                            })
                        }))
            })
            .cloned()
            .collect();
        if query.is_empty() && self.source == CeaBoxSource::All {
            rows.retain(|item| self.fixture_defaults.contains(&item.id));
            rows.sort_by_key(|item| self.fixture_defaults.iter().position(|id| id == &item.id));
        } else {
            rows.sort_by_key(|item| item.source != CeaBoxSource::Favorite);
        }
        rows
    }

    fn load_page(&mut self, skip: usize, cx: &mut Context<Self>) {
        let Some(host) = self.host.clone() else {
            return;
        };
        if !self.current() || self.loading {
            return;
        }
        self.loading = true;
        let preserving = self.preserving;
        let lease = SearchLease::default();
        let cancelled = lease.0.clone();
        self.search_lease = Some(lease);
        let retain_count = if skip == 0 && self.preserving {
            self.items.len().max(PAGE_SIZE)
        } else {
            PAGE_SIZE
        };
        let (display, generation, source, query) = (
            self.display,
            self.query_generation,
            self.source,
            self.query.clone(),
        );
        let (sender, mut receiver) = tokio::sync::watch::channel(SearchDelivery::default());
        cx.background_executor()
            .spawn(async move {
                let check = || cancelled.load(Ordering::Acquire) || sender.is_closed();
                let mut latest = SearchDelivery::default();
                let result =
                    host.ceabox_search_stream(&query, source, skip, PAGE_SIZE, check, |update| {
                        latest.publish_update(&sender, update, preserving, retain_count);
                    });
                // Preserve an already loaded range on a data refresh. These pages
                // use Core's completed query snapshot, rather than rescanning it.
                let result = result.and_then(|page| {
                    let Some(mut page) = page else {
                        return Ok(None);
                    };
                    while skip == 0 && page.has_more && page.items.len() < retain_count && !check()
                    {
                        let next = host.ceabox_search_stream(
                            &query,
                            source,
                            page.items.len(),
                            PAGE_SIZE,
                            check,
                            |_| {},
                        )?;
                        let Some(next) = next else {
                            return Ok(None);
                        };
                        if next.items.is_empty() {
                            page.has_more = false;
                            break;
                        }
                        page.has_more = next.has_more;
                        page.total = next.total;
                        page.items.extend(next.items);
                    }
                    Ok(Some(page))
                });
                if check() {
                    return;
                }
                if let Ok(Some(page)) = &result {
                    // The final delivery also contains any retained extra pages.
                    if retain_count > PAGE_SIZE || latest.update.is_none() {
                        latest.update = Some(Arc::new(CeaBoxSearchUpdate {
                            page: page.clone(),
                            complete: true,
                            elapsed_ms: latest
                                .update
                                .as_ref()
                                .map_or(0, |update| update.elapsed_ms),
                        }));
                    }
                }
                latest.finished = true;
                if preserving && result.is_err() {
                    latest.update = None;
                }
                latest.error = result.err();
                sender.send_replace(latest);
            })
            .detach();
        self.load = Some(cx.spawn(async move |this, cx| {
            let mut previous_update: Option<Arc<CeaBoxSearchUpdate>> = None;
            while receiver.changed().await.is_ok() {
                // Coalesce rapid batches into an 8ms UI tick. This bounds UI
                // work without treating the timer as a measured painted frame.
                Timer::after(Duration::from_millis(8)).await;
                let delivery = receiver.borrow_and_update().clone();
                let accepted = this
                    .update(cx, |this, cx| {
                        if !this.current()
                            || this.display != display
                            || this.query_generation != generation
                        {
                            return false;
                        }
                        if let Some(update) = &delivery.update.as_ref().filter(|update| {
                            previous_update
                                .as_ref()
                                .is_none_or(|previous| !Arc::ptr_eq(previous, update))
                        }) {
                            this.apply_search_page(&update.page, skip, cx);
                        }
                        this.loading = !delivery.finished;
                        if delivery.finished {
                            this.complete_ms = Some(this.search_started.elapsed().as_millis());
                            this.search_lease = None;
                        }
                        if let Some(error) = delivery.error.clone() {
                            this.has_more = false;
                            this.error = Some(error);
                        }
                        // A worker can close without a snapshot only when retired.
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                previous_update = delivery.update;
                if !accepted || delivery.finished {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.current()
                    && this.display == display
                    && this.query_generation == generation
                    && this.loading
                {
                    this.loading = false;
                    this.search_lease = None;
                    this.error = Some(tr("搜索任务提前结束，请重新输入。", "Search ended early. Please enter your query again.").into());
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn results_current(&self) -> bool {
        self.results_generation == Some(self.query_generation)
    }

    fn apply_search_page(&mut self, page: &CeaBoxPage, skip: usize, cx: &mut Context<Self>) {
        self.total = page.total;
        self.has_more = page.has_more && !page.items.is_empty();
        self.source_notice = page
            .sources
            .iter()
            .filter(|status| !status.available)
            .find_map(|status| status.message.clone());
        self.error = None;
        self.search_updates += 1;
        if self.first_results_ms.is_none() && !page.items.is_empty() {
            self.first_results_ms = Some(self.search_started.elapsed().as_millis());
        }
        let first_current = !self.results_current();
        let old_keys: Vec<_> = self.items.iter().map(row_key).collect();
        let previous_index = self
            .selected
            .as_ref()
            .and_then(|key| old_keys.iter().position(|old| old == key))
            .unwrap_or(0);
        let scroll = self.list.logical_scroll_top();
        let anchor = old_keys.get(scroll.item_ix).cloned();
        let mut items = if skip == 0 {
            Vec::new()
        } else {
            self.items.iter().take(skip).cloned().collect()
        };
        let mut keys: HashSet<_> = items.iter().map(row_key).collect();
        items.extend(
            page.items
                .iter()
                .filter(|item| keys.insert(row_key(item)))
                .cloned(),
        );
        let new_keys: Vec<_> = items.iter().map(row_key).collect();
        self.selected = retain_selection(self.selected.take(), previous_index, &new_keys);
        let changed = self.items != items;
        self.items = items;
        self.results_generation = Some(self.query_generation);
        if first_current && !self.preserving {
            self.list.reset(self.items.len());
            self.list.scroll_to(gpui::ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            });
        } else {
            crate::list_reconcile::reconcile(&self.list, &old_keys, &new_keys);
            if !self.items.is_empty() {
                let index = anchor
                    .and_then(|key| new_keys.iter().position(|new| *new == key))
                    .unwrap_or(scroll.item_ix.min(self.items.len() - 1));
                self.list.scroll_to(gpui::ListOffset {
                    item_ix: index,
                    offset_in_item: scroll.offset_in_item,
                });
            }
        }
        self.preserving = false;
        if changed {
            self.scrollbar.invalidate_measurements();
        }
        cx.notify();
    }

    fn selected_item(&self) -> Option<&CeaBoxItem> {
        if !self.results_current() {
            return None;
        }
        self.selected
            .as_ref()
            .and_then(|key| self.items.iter().find(|item| row_key(item) == *key))
    }

    fn navigate(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.results_current() || self.items.is_empty() {
            return;
        }
        let index = self
            .selected
            .as_ref()
            .and_then(|key| self.items.iter().position(|item| row_key(item) == *key))
            .unwrap_or(0);
        let next = if delta.unsigned_abs() > 1 {
            (index as isize + delta).clamp(0, self.items.len() as isize - 1) as usize
        } else {
            (index as isize + delta).rem_euclid(self.items.len() as isize) as usize
        };
        self.retire_execution();
        self.selected = Some(row_key(&self.items[next]));
        self.list.scroll_to_reveal_item(next);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).is_composing()
            || event.keystroke.modifiers.number_of_modifiers() != 0
        {
            return;
        }
        let focused = self.search.read(cx).focus_handle().is_focused(window);
        match event.keystroke.key.as_str() {
            "down" => self.navigate(1, cx),
            "up" => self.navigate(-1, cx),
            "pagedown" => self.navigate(8, cx),
            "pageup" => self.navigate(-8, cx),
            "tab" => {
                if !event.is_held {
                    self.execute(window, cx);
                }
            }
            "enter" if !focused => {
                if !event.is_held {
                    self.execute(window, cx);
                }
            }
            "escape" if !focused => {
                if !event.is_held {
                    self.dismiss(window, cx);
                }
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    fn execute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current()
            || self.busy
            || !self.results_current()
            || self.search.read(cx).is_composing()
        {
            return;
        }
        let Some(item) = self.selected_item().cloned() else {
            return;
        };
        let Some(host) = self.host.clone() else {
            self.feedback = Some((
                format!("{}: {} “{}”", tr("预览", "Preview"), action_name(Some(&item)), item.title),
                Instant::now(),
            ));
            cx.notify();
            return;
        };
        let request = match host.capture_native_ceabox_execute() {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.operation = self.operation.wrapping_add(1);
        let operation = self.operation;
        let display = self.display;
        let target = item.target.clone();
        self.search.read(cx).focus_handle().focus(window);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.execute_native_ceabox(&request, &target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.current() || this.display != display || this.operation != operation {
                    return;
                }
                this.busy = false;
                match result {
                    Ok(reply) if reply["ok"].as_bool() != Some(false) => {
                        this.feedback = Some((
                            format!("{}: {} “{}”", tr("已完成", "Completed"), action_name(Some(&item)), item.title),
                            Instant::now(),
                        ));
                    }
                    Ok(reply) => {
                        this.error = Some(reply["error"].as_str().unwrap_or(tr("操作未完成", "Operation did not complete")).into())
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).is_composing() {
            return;
        }
        self.query.clear();
        self.search
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.search.read(cx).focus_handle().focus(window);
        self.reload(cx);
    }

    fn choose_source(&mut self, source: CeaBoxSource, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).is_composing() {
            return;
        }
        self.source = source;
        if let Some(host) = &self.host {
            host.ceabox_set_source(source);
        }
        self.search.read(cx).focus_handle().focus(window);
        self.reload(cx);
    }

    fn retire_execution(&mut self) {
        self.operation = self.operation.wrapping_add(1);
        self.busy = false;
        if let Some(host) = &self.host {
            host.ceabox_retire_execution();
        }
    }

    fn background(
        &self,
        key: SharedString,
        normal: Hsla,
        hover: Hsla,
        window: &mut Window,
    ) -> Hsla {
        let now = Instant::now();
        let mut tweens = self.motion.borrow_mut();
        let amount = tweens.value(
            key.clone(),
            if self.hovered.contains(&key) {
                1.0
            } else {
                0.0
            },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if tweens.any_active(now) {
            window.request_animation_frame();
        }
        motion::mix_hsla(normal, hover, amount)
    }

    fn icon_button(
        &self,
        id: &'static str,
        icon: &str,
        tip: &'static str,
        active: bool,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let key: SharedString = id.into();
        let background = self.background(
            key.clone(),
            if active {
                color(&t.tab_active)
            } else {
                gpui::transparent_black()
            },
            color(&t.secondary),
            window,
        );
        let (tokens, dark) = (self.tokens.clone(), self.dark);
        rsx! { <div id={id} size={px(24.0)} flex_none flex items_center justify_center
        rounded={px(5.0)} cursor_pointer bg={background} /> }
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| cx.stop_propagation()),
        )
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered {
                this.hovered.insert(key.clone());
            } else {
                this.hovered.remove(&key);
            }
            cx.notify();
        }))
        .child(
            div()
                .size(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .when(id == "ceabox-clear", |element| {
                    element.rounded_full().bg(color(&t.secondary))
                })
                .child(
                    svg()
                        .path(self.tokens.icons.get(icon).cloned().unwrap_or_default())
                        .size(px(if id == "ceabox-clear" { 10.0 } else { 14.0 }))
                        .text_color(color(if active { &t.accent } else { &t.text_muted })),
                ),
        )
        .tooltip(move |_, cx| {
            cx.new(|_| Tip {
                text: tip.into(),
                tokens: tokens.clone(),
                dark,
            })
            .into()
        })
    }

    fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.results_current() && self.has_more && !self.loading && index + 8 >= self.items.len()
        {
            self.load_page(self.items.len(), cx);
        }
        let Some(item) = self.items.get(index).cloned() else {
            return div().into_any_element();
        };
        let key = row_key(&item);
        let selected = self.selected.as_ref() == Some(&key);
        let t = self.tokens.theme(self.dark).clone();
        let p = self.tokens.ceabox_proposal;
        let narrow = f32::from(window.viewport_size().width) <= 440.0;
        let row_id: SharedString = format!("ceabox-row-{key}").into();
        let normal = if selected {
            motion::mix_hsla(color(&t.surface), color(&t.neutral_active), 0.78)
        } else {
            gpui::transparent_black()
        };
        let background = self.background(
            row_id.clone(),
            normal,
            if selected { normal } else { color(&t.hover) },
            window,
        );
        let context = item
            .context
            .strip_prefix("Quicker 动作 / ")
            .unwrap_or(&item.context);
        let context = if context == source_name(item.source) || context == "Quicker 动作" {
            ""
        } else {
            context
        };
        let meta = rsx! { <div flex items_center gap={px(6.0)} min_w_0 text_size={px(p.meta_font_size)} text_color={color(&t.text_muted)} /> }
            .child(rsx! { <div flex flex_none items_center gap={px(3.0)} /> }
                .when(item.source == CeaBoxSource::Favorite, |element| element.child(svg()
                    .path(self.tokens.icons.get("star").cloned().unwrap_or_default()).size(px(10.0)).text_color(color(&t.pin))))
            .child(source_label(item.source).get()))
            .when(!context.is_empty(), |element| element.child("·").child(div().min_w_0().truncate().child(context.to_string())));
        let tail = if selected {
            "↵".to_string()
        } else if item.source == CeaBoxSource::Clipboard {
            display_time(&item.time)
        } else if icon_name(&item) == "link" {
            tr("链接", "Link").into()
        } else if icon_name(&item) == "file" {
            tr("文件", "File").into()
        } else {
            String::new()
        };
        let click_key = key.clone();
        let hover_key = row_id.clone();
        let icon = icon_name(&item);
        let icon_color = if icon == "link" || item.source == CeaBoxSource::Action {
            color(&t.accent)
        } else {
            color(&t.text_secondary)
        };
        let down_key = key.clone();
        let generation = self.query_generation;
        let tail_width = if item.source == CeaBoxSource::Clipboard {
            if narrow {
                72.0
            } else {
                76.0
            }
        } else if narrow {
            32.0
        } else {
            42.0
        };
        let row = rsx! {
            <div id={row_id} h={px(p.row_height)} flex items_center gap={px(if narrow { 7.0 } else { 10.0 })}
                px={px(if narrow { 7.0 } else { p.row_padding })} rounded={px(p.row_radius)} border_1
                border_color={if selected { color(&t.border).opacity(0.44) } else { gpui::transparent_black() }}
                bg={background} cursor_pointer>
                <div size={px(p.icon_box_size)} flex_none flex items_center justify_center>
                    {svg().path(self.tokens.icons.get(icon).cloned().unwrap_or_default()).size(px(p.icon_size)).text_color(icon_color)}
                </div>
                <div flex_1 min_w_0 flex flex_col justify_center gap={px(4.0)}>
                    <div w_full min_w_0 line_clamp={1} h={px(p.title_font_size * 1.35)} line_height={relative(1.35)}
                        text_size={px(p.title_font_size)} font_weight={FontWeight::MEDIUM} text_color={color(&t.text)}>{title_text(&item.title, &self.query)}</div>
                    {meta}
                </div>
                <div w={px(tail_width)} flex_none text_right whitespace_nowrap overflow_hidden
                    text_size={px(if selected { 15.0 } else { p.meta_font_size })} text_color={color(&t.text_muted)}>{tail}</div>
            </div>
        }
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered { this.hovered.insert(hover_key.clone()); } else { this.hovered.remove(&hover_key); }
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if !this.results_current() { return; }
                this.retire_execution();
                this.selected = Some(down_key.clone());
                this.pressed = Some((down_key.clone(), generation));
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                let pressed = this.pressed.take();
                if this.results_current() && window.modifiers().number_of_modifiers() == 0
                    && pressed == Some((click_key.clone(), this.query_generation))
                    && this.selected.as_ref() == Some(&click_key)
                { this.execute(window, cx); }
            }));
        rsx! { <div h={px(p.row_height + 2.0)} px={px(p.row_inset)} pb={px(2.0)} opacity={if self.results_current() { 1.0 } else { 0.45 }}>{row}</div> }
            .into_any_element()
    }

    pub fn report(&self, window: &Window, cx: &App) -> Value {
        let native_geometry = platform::geometry(window).ok().map(|geometry| {
            json!({
                "left":geometry.left, "top":geometry.top,
                "clientWidthPhysical":geometry.width, "clientHeightPhysical":geometry.height,
                "scale":geometry.scale,
                "clientWidthLogical":f64::from(geometry.width) / geometry.scale,
                "clientHeightLogical":f64::from(geometry.height) / geometry.scale,
            })
        });
        json!({"mode":if self.host.is_some() {"live"} else {"fixture"}, "productionHostStarted":self.host.is_some(),
            "theme":if self.dark {"dark"} else {"light"}, "width":f32::from(window.viewport_size().width),
            "height":f32::from(window.viewport_size().height), "query":self.query, "source":source_name(self.source),
            "total":self.total, "renderedItems":self.items.len(), "selected":self.selected,
            "loading":self.loading, "busy":self.busy, "error":self.error,
            "resultsCurrent":self.results_current(),
            "searchUpdates":self.search_updates, "firstResultsMs":self.first_results_ms,
            "completeMs":self.complete_ms,
            "topHeight":self.tokens.ceabox_proposal.search_height+self.tokens.ceabox_proposal.filter_height,
            "nativeInitialClientSize":self.native_initial_size,
            "nativeGeometry":native_geometry,
            "windowDragRequests":self.window_drag_requests,
            "input":self.search.read(cx).diagnostics()})
    }
}

impl Render for CeaBoxWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        let p = self.tokens.ceabox_proposal;
        let narrow = f32::from(window.viewport_size().width) <= 440.0;
        let mut header = rsx! { <div id={"ceabox-caption"} h={px(p.search_height)} flex_none flex items_center gap={px(8.0)} pl={px(12.0)} pr={px(8.0)} /> }
            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                this.search.read(cx).focus_handle().focus(window);
                this.search.update(cx, |input, _| input.arm_window_drag(event));
                cx.stop_propagation();
            }))
            .child(rsx! { <div id={"ceabox-drag"} w={px(20.0)} h_full flex_none flex items_center justify_center cursor_pointer /> }
                .child(svg().path(self.tokens.icons.get("search").cloned().unwrap_or_default()).size(px(16.0)).text_color(color(&t.text_muted))))
            .child(div().flex_1().min_w_0().child(self.search.clone()));
        if !self.query.is_empty() {
            header = header.child(
                self.icon_button("ceabox-clear", "close", tr("清空搜索", "Clear search"), false, window, cx)
                    .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
            );
        }
        header = header.child(rsx! { <div flex flex_none items_center h={px(20.0)} pl={px(6.0)} border_l_1 border_color={color(&t.border)} /> }
            .child(self.icon_button("ceabox-pin", "pin", if self.pinned { tr("取消固定", "Unpin window") } else { tr("固定窗口", "Pin window") }, self.pinned, window, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.pinned = !this.pinned;
                    if let Some(host) = &this.host { host.ceabox_set_pinned(this.pinned); }
                    cx.notify();
                })))
            .child(self.icon_button("ceabox-close", "close", tr("关闭 CeaBox（Esc）", "Close CeaBox (Esc)"), false, window, cx)
                .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx)))));
        let mut tabs = rsx! { <div h={px(p.filter_height)} flex_none px={px(12.0)} flex items_center gap={px(3.0)} border_b_1 border_color={color(&t.border).opacity(0.65)} /> };
        for source in [
            CeaBoxSource::All,
            CeaBoxSource::Favorite,
            CeaBoxSource::Clipboard,
            CeaBoxSource::Action,
        ] {
            let active = self.source == source;
            let key: SharedString = format!("ceabox-source-{}", source_name(source)).into();
            let background = self.background(
                key.clone(),
                if active {
                    color(&t.secondary)
                } else {
                    gpui::transparent_black()
                },
                if active {
                    color(&t.secondary)
                } else {
                    color(&t.hover)
                },
                window,
            );
            tabs = tabs.child(rsx! { <div id={key.clone()}
                h={px(24.0)} px={px(8.0)} flex_none flex items_center rounded={px(4.0)}
                bg={background}
                text_size={px(12.0)} text_color={color(if active { &t.text } else { &t.text_muted })} cursor_pointer>{source_label(source).get()}</div> }
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered { this.hovered.insert(key.clone()); } else { this.hovered.remove(&key); }
                    cx.notify();
                }))
                .on_click(cx.listener(move |this, _, window, cx| this.choose_source(source, window, cx))));
        }
        if matches!(self.source, CeaBoxSource::All | CeaBoxSource::Favorite) {
            tabs = tabs
                .child(div().flex_1())
                .child(
                    svg()
                        .path(self.tokens.icons.get("star").cloned().unwrap_or_default())
                        .size(px(11.0))
                        .text_color(color(&t.pin)),
                )
                .when(!narrow, |element| {
                    element.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(color(&t.text_muted))
                            .child(tr("收藏优先", "Favorites first")),
                    )
                });
        }
        let weak = cx.weak_entity();
        let virtual_list = list(self.list.clone(), move |index, window, cx| {
            weak.update(cx, |this, cx| this.render_row(index, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h_0()
        .w_full();
        let mut results = rsx! { <div relative flex_1 min_h_0 flex flex_col /> };
        if self.query.trim().is_empty() && !self.items.is_empty() {
            results = results.child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .px(px(20.0))
                    .flex()
                    .items_end()
                    .pb(px(3.0))
                    .text_size(px(11.0))
                    .text_color(color(&t.text_muted))
                    .child(tr("常用与最近", "Common and recent")),
            );
        }
        if self.items.is_empty() {
            results = results.child(rsx! { <div flex_1 min_h_0 px={px(24.0)} flex flex_col items_center justify_center gap={px(9.0)} text_color={color(&t.text_muted)}>
                <div text_size={px(13.0)}>{if self.loading { tr("正在搜索…", "Searching…") } else if self.error.is_some() { tr("搜索暂不可用", "Search unavailable") } else if self.query.trim().is_empty() { tr("还没有常用内容", "No common items yet") } else { tr("没有找到匹配内容", "No matching items") }}</div>
                <div text_size={px(12.0)}>{if self.loading { String::new() } else { self.error.clone().unwrap_or_else(|| if self.query.trim().is_empty() { tr("输入关键词搜索收藏、剪贴板和动作", "Search favorites, clipboard and actions by keyword").into() } else { tr("换个关键词，或试试其他来源", "Try another keyword or source").into() }) }}</div>
            </div> });
        } else {
            let weak = cx.weak_entity();
            let changed = Rc::new(move |_: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |_, cx| cx.notify());
            });
            results = results.child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .pt(px(5.0))
                    .pb(px(8.0))
                    .child(virtual_list)
                    .child(self.scrollbar.render(
                        "ceabox-scrollbar",
                        ScrollbarTarget::List(self.list.clone()),
                        self.query_generation,
                        ScrollbarStyle {
                            ink: color(&t.text_muted),
                            metrics: self.tokens.scrollbar,
                            always_visible: false,
                        },
                        changed,
                    )),
            );
        }
        if let Some((message, _)) = &self.feedback {
            results = results.child(
                div()
                    .absolute()
                    .right(px(12.0))
                    .bottom(px(10.0))
                    .max_w_full()
                    .mx(px(12.0))
                    .px(px(12.0))
                    .py(px(9.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(color(&t.border))
                    .bg(color(&t.surface))
                    .text_size(px(12.0))
                    .text_color(color(&t.text_secondary))
                    .truncate()
                    .child(message.clone()),
            );
        }
        let footer_label = if let Some(error) = &self.error {
            error.clone()
        } else if self.busy {
            tr("正在执行…", "Executing…").into()
        } else if self.loading {
            if self.results_current() && !self.items.is_empty() {
                cliphost_core::trf!("已找到 {} 个 · 继续搜索…", "{} found · searching…", self.total)
            } else {
                tr("搜索中…", "Searching…").into()
            }
        } else if let Some(notice) = &self.source_notice {
            notice.clone()
        } else {
            if self.query.trim().is_empty() {
                cliphost_core::trf!("{} 个条目", "{} items", self.total)
            } else {
                cliphost_core::trf!("{} 个结果", "{} results", self.total)
            }
        };
        let execute_label = action_name(self.selected_item());
        let can_execute = self.selected_item().is_some() && !self.busy;
        let execute_button = rsx! { <div id={"ceabox-execute"} flex_none px={px(5.0)} py={px(4.0)} rounded={px(4.0)}
            text_color={color(if can_execute { &t.text_secondary } else { &t.text_muted })} cursor_pointer>{format!("{execute_label}  ↵")}</div> }
            .on_click(cx.listener(|this, _, window, cx| this.execute(window, cx)));
        let footer = rsx! { <div h={px(p.footer_height)} flex_none flex items_center gap={px(8.0)} pl={px(19.0)} pr={px(13.0)} border_t_1 border_color={color(&t.border).opacity(0.65)} text_size={px(11.0)} text_color={color(&t.text_muted)}>
            <div flex_1 min_w_0 truncate>{footer_label}</div>
            <div flex_none>{tr("↑ ↓ 选择", "↑ ↓ Select")}</div>
            {execute_button}
        </div> };
        div()
            .id("ceabox-root")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .font_family(self.tokens.typography.font_family.clone())
            .bg(color(&t.surface))
            .text_color(color(&t.text))
            .capture_key_down(
                cx.listener(|this, event, window, cx| this.key_down(event, window, cx)),
            )
            .child(header)
            .child(tabs)
            .child(results)
            .child(footer)
    }
}

#[cfg(test)]
mod localization_tests {
    use super::*;
    use cliphost_core::i18n::{tr_for, LanguageMode};

    #[test]
    fn ceabox_source_and_search_labels_resolve_both_languages() {
        for (source, chinese, english) in [
            (CeaBoxSource::All, "全部", "All"),
            (CeaBoxSource::Favorite, "收藏", "Favorites"),
            (CeaBoxSource::Clipboard, "剪贴板", "Clipboard"),
            (CeaBoxSource::Action, "动作", "Actions"),
        ] {
            let Text(zh, en) = source_label(source);
            assert_eq!(tr_for(LanguageMode::Chinese, zh, en), chinese);
            assert_eq!(tr_for(LanguageMode::English, zh, en), english);
        }
        let Text(zh, en) = SEARCH_PLACEHOLDER;
        assert_eq!(tr_for(LanguageMode::Chinese, zh, en), "搜索收藏、剪贴板、动作…");
        assert_eq!(tr_for(LanguageMode::English, zh, en), "Search favorites, clipboard and actions…");
    }

    #[test]
    fn ceabox_action_labels_translate_without_changing_targets_or_selection_keys() {
        for (source, kind, is_folder, chinese, english) in [
            (CeaBoxSource::Favorite, "FolderClipItem", true, "打开", "Open"),
            (CeaBoxSource::Action, "ActionClipItem", false, "运行", "Run"),
            (CeaBoxSource::Favorite, "ActionClipItem", false, "运行", "Run"),
            (CeaBoxSource::Favorite, "LinkClipItem", false, "打开", "Open"),
            (CeaBoxSource::Favorite, "FileClipItem", false, "打开", "Open"),
            (CeaBoxSource::Clipboard, "FileClipItem", false, "粘贴", "Paste"),
        ] {
            let item = CeaBoxItem {
                id: "用户-id".into(), source, kind: kind.into(), title: "用户原文".into(),
                context: "D:\\文档".into(), time: String::new(), is_folder,
                target: cliphost_core::ceabox::CeaBoxExecutionTarget { source, id: "用户-id".into() },
            };
            let key = row_key(&item);
            let Text(zh, en) = action_label(Some(&item));
            assert_eq!(tr_for(LanguageMode::Chinese, zh, en), chinese);
            assert_eq!(tr_for(LanguageMode::English, zh, en), english);
            assert_eq!(row_key(&item), key);
            assert_eq!(key, format!("{}:用户-id", source_name(source)));
            assert_eq!(item.target.id, "用户-id");
            assert_eq!(item.title, "用户原文");
            assert_eq!(item.context, "D:\\文档");
        }
        let Text(zh, en) = action_label(None);
        assert_eq!(tr_for(LanguageMode::Chinese, zh, en), "粘贴");
        assert_eq!(tr_for(LanguageMode::English, zh, en), "Paste");
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    #[test]
    fn streaming_is_immediate_but_refresh_does_not_publish_a_truncated_range() {
        let update = |complete| CeaBoxSearchUpdate {
            page: CeaBoxPage {
                items: vec![],
                total: 400,
                skip: 0,
                take: 100,
                has_more: true,
                sources: vec![],
            },
            complete,
            elapsed_ms: 1,
        };
        let (sender, mut receiver) = tokio::sync::watch::channel(SearchDelivery::default());
        let mut delivery = SearchDelivery::default();
        delivery.publish_update(&sender, update(false), false, 100);
        assert!(
            receiver.has_changed().unwrap(),
            "a new query must publish its first batch"
        );
        receiver.borrow_and_update();
        delivery.publish_update(&sender, update(false), true, 300);
        delivery.publish_update(&sender, update(true), true, 300);
        assert!(
            !receiver.has_changed().unwrap(),
            "first-page refresh must keep the loaded viewport"
        );
        delivery.publish_update(&sender, update(true), true, 100);
        assert!(
            receiver.has_changed().unwrap(),
            "a complete one-page refresh can publish"
        );
    }

    #[test]
    fn query_replacement_retires_detached_work_without_cancelling_the_next_query() {
        let first = SearchLease::default();
        let worker = first.0.clone();
        let next = SearchLease::default();
        assert!(!worker.load(Ordering::Acquire));
        drop(first);
        assert!(worker.load(Ordering::Acquire));
        assert!(!next.0.load(Ordering::Acquire));
    }

    #[test]
    fn streaming_reorder_keeps_the_selected_source_and_id() {
        let keys = ["收藏:a", "剪贴板:a", "动作:b"].map(str::to_owned);
        assert_eq!(
            retain_selection(Some("剪贴板:a".into()), 0, &keys),
            Some("剪贴板:a".into())
        );
        let reordered = ["动作:b", "收藏:a", "剪贴板:a"].map(str::to_owned);
        assert_eq!(
            retain_selection(Some("剪贴板:a".into()), 1, &reordered),
            Some("剪贴板:a".into())
        );
    }

    #[test]
    fn removed_selection_uses_its_nearest_row_and_empty_results_clear_it() {
        let keys = ["收藏:a", "动作:b"].map(str::to_owned);
        assert_eq!(
            retain_selection(Some("剪贴板:removed".into()), 90, &keys),
            Some("动作:b".into())
        );
        assert_eq!(
            retain_selection(Some("剪贴板:removed".into()), 1, &[]),
            None
        );
        assert_eq!(retain_selection(None, 0, &keys), Some("收藏:a".into()));
    }
}
