//! Small persistent file window. The UI never enumerates a drive or invokes
//! Shell on the render thread. Every delivery is scoped to both the native
//! display generation and the current navigation epoch.

use std::{
    collections::{HashSet, VecDeque},
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use chrono::{DateTime, Local};
use cliphost_core::{
    file_browser::{self, BrowserDrive, BrowserEntry, BrowserHome, BrowserPlace},
    host::{HostState, NativeFileBrowserPanelLease, NativeSuperMenuPasteRequest},
    i18n::tr,
};
use gpui::{
    div, list, prelude::*, px, relative, size, AnyElement, App, Bounds, Context, Entity,
    EventEmitter, FocusHandle, KeyDownEvent, ListAlignment, ListState, MouseButton, SharedString,
    Stateful, Subscription, Task, Window, WindowBounds, WindowHandle, WindowOptions,
};
use gpui_rsx::rsx;
use serde_json::{json, Value};

use crate::{
    crisp_svg::svg,
    input::{InputColors, SearchEvent, SearchInput},
    platform,
    preview::DesignTokens,
    scrollbar::{ScrollbarState, ScrollbarStyle, ScrollbarTarget},
    view::color,
};

const DIRECTORY_WORKERS: usize = 2;
const DRIVE_WORKERS: usize = 4;
const GLOBAL_DIRECTORY_WORKERS: usize = 8;
const GLOBAL_DRIVE_WORKERS: usize = 8;
static DIRECTORY_IO: AtomicUsize = AtomicUsize::new(0);
static DRIVE_IO: AtomicUsize = AtomicUsize::new(0);

/// Lives inside the actual blocking operation, independently of any GPUI
/// entity/task handle. Closing and recreating panels cannot retire its slot.
struct BrowserIoPermit(&'static AtomicUsize);

impl BrowserIoPermit {
    fn try_acquire(counter: &'static AtomicUsize, limit: usize) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < limit).then_some(active + 1)
            })
            .ok()
            .map(|_| Self(counter))
    }
}

impl Drop for BrowserIoPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The parent menu owns its HWND, panel chrome and dismissal. Content never
/// hides the shared overlay or borrows the independent window's lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileBrowserEvent {
    Close,
    OpenPanel(String),
    Dismiss,
    /// Send this folder to the menu's accepted target (footer button).
    Go(String),
    CopyPath(String),
    /// Add or remove the folder from the file menu's custom list.
    ToggleFileMenu(String),
}

/// Docked inside the super menu (2026-10-26): the menu's root column is the
/// sidebar, so a panel shows one header row, its list and an action footer.
struct EmbeddedBrowser {
    lease: Option<NativeFileBrowserPanelLease>,
    width: f32,
    reload_on_resume: bool,
    can_split: bool,
    go_label: String,
    in_file_menu: bool,
}

impl EmbeddedBrowser {
    fn new(width: f32) -> Self {
        Self { lease: None, width, reload_on_resume: false, can_split: false, go_label: String::new(), in_file_menu: false }
    }
}

impl Drop for EmbeddedBrowser {
    fn drop(&mut self) {
        if let Some(lease) = &self.lease {
            lease.retire();
        }
    }
}

#[derive(Clone)]
enum Row {
    Label(&'static str, usize),
    Places(Vec<BrowserPlace>),
    Drives(Vec<BrowserDrive>),
    Entry(usize),
    Hint,
}

fn path_title(path: &str) -> String {
    if path.is_empty() {
        return tr("主页", "Home").into();
    }
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.len() == 2 && trimmed.as_bytes().get(1) == Some(&b':') {
        return String::new();
    }
    let Some(index) = trimmed.rfind(['\\', '/']) else {
        return String::new();
    };
    if index == 2 && trimmed.as_bytes().get(1) == Some(&b':') {
        return format!("{}\\", &trimmed[..2]);
    }
    // The server/share boundary is the root of a UNC location.
    if trimmed.starts_with("\\\\") && trimmed[2..].matches('\\').count() <= 1 {
        return String::new();
    }
    trimmed[..index].to_string()
}

fn display_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if size >= 100.0 {
        format!("{size:.0} {}", UNITS[unit])
    } else if size >= 10.0 {
        format!("{size:.1} {}", UNITS[unit])
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}

fn display_time(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(DateTime::from_timestamp_millis)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y/%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

fn place_icon(kind: &str) -> &'static str {
    match kind {
        "desktop" => "desktop",
        "downloads" | "download" => "arrow-down",
        "pictures" | "picture" => "image",
        "documents" | "document" => "text",
        _ => "folder-closed",
    }
}

fn entry_icon(entry: &BrowserEntry) -> &'static str {
    if entry.is_dir {
        return "folder-closed";
    }
    match entry.kind.as_str() {
        "image" | "video" => "image",
        "code" => "code",
        "pdf" => "text",
        "link" => "link",
        _ => "file",
    }
}

fn kind_label(entry: &BrowserEntry) -> &'static str {
    if entry.is_dir {
        return tr("文件夹", "Folder");
    }
    match entry.kind.as_str() {
        "image" => tr("图像", "Image"),
        "video" => tr("视频", "Video"),
        "audio" => tr("音频", "Audio"),
        "archive" => tr("压缩文件", "Archive"),
        "code" => tr("代码文件", "Code"),
        "pdf" => "PDF",
        "link" => tr("快捷方式", "Shortcut"),
        _ => tr("文件", "File"),
    }
}

fn drive_kind(kind: &str) -> &'static str {
    match kind {
        "fixed" => tr("本地磁盘", "Local drive"),
        "removable" => tr("可移动磁盘", "Removable drive"),
        "network" => tr("网络磁盘", "Network drive"),
        "cdrom" => tr("光盘驱动器", "Disc drive"),
        "ramdisk" => tr("内存磁盘", "RAM drive"),
        _ => tr("磁盘", "Drive"),
    }
}

pub struct FileBrowserWindow {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    focus: FocusHandle,
    address: Entity<SearchInput>,
    filter: Entity<SearchInput>,
    _subscriptions: Vec<Subscription>,
    initialized: bool,
    active: bool,
    display: u64,
    epoch: u64,
    history: Vec<String>,
    history_index: usize,
    path: String,
    address_editing: bool,
    query: String,
    home: BrowserHome,
    entries: Vec<BrowserEntry>,
    rows: Vec<Row>,
    cursor: Option<usize>,
    columns: usize,
    list: ListState,
    scrollbar: ScrollbarState,
    loading: bool,
    error: Option<String>,
    feedback: Option<String>,
    opening: bool,
    next_work: u64,
    directory_tasks: Vec<(u64, Task<()>)>,
    pending_directory: Option<(u64, String)>,
    drive_tasks: Vec<(u64, String, Task<()>)>,
    drive_queue: VecDeque<(u64, BrowserDrive)>,
    action_task: Option<Task<()>>,
    home_task: Option<Task<()>>,
    home_loaded: bool,
    fixture: bool,
    embedded: Option<EmbeddedBrowser>,
    filter_open: bool,
    detail: bool,
    more_open: bool,
}

impl EventEmitter<FileBrowserEvent> for FileBrowserWindow {}

impl Drop for FileBrowserWindow {
    fn drop(&mut self) {
        // Retire execution rights before dropping task handles; OS calls can
        // outlive the entity but cannot borrow a replacement panel's lease.
        if let Some(lease) = self
            .embedded
            .as_ref()
            .and_then(|embedded| embedded.lease.as_ref())
        {
            lease.retire();
        }
    }
}

impl FileBrowserWindow {
    fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address = cx.new(|cx| SearchInput::new(window, cx));
        let filter = cx.new(|cx| SearchInput::new(window, cx));
        let subscriptions =
            vec![
                cx.subscribe_in(
                    &filter,
                    window,
                    |this: &mut Self, _, event, window, cx| match event {
                        SearchEvent::Changed(query) => {
                            this.query.clone_from(query);
                            this.cursor = None;
                            this.reproject(cx);
                        }
                        SearchEvent::Submit => this.open_cursor(window, cx),
                        SearchEvent::Escape => {
                            this.filter
                                .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                            this.filter_open = false;
                            this.query.clear();
                            this.reproject(cx);
                            this.focus.focus(window);
                        }
                    },
                ),
                cx.subscribe_in(&address, window, |this: &mut Self, _, event, window, cx| {
                    match event {
                        SearchEvent::Submit => {
                            let path = this.address.read(cx).text().trim().to_string();
                            this.navigate(path, true, window, cx);
                        }
                        SearchEvent::Escape => {
                            this.address_editing = false;
                            this.focus.focus(window);
                            cx.notify();
                        }
                        SearchEvent::Changed(_) => {}
                    }
                }),
            ];
        let mut this = Self {
            host,
            tokens,
            dark,
            focus: cx.focus_handle(),
            address,
            filter,
            _subscriptions: subscriptions,
            initialized: false,
            active: false,
            display: 0,
            epoch: 0,
            history: vec![String::new()],
            history_index: 0,
            path: String::new(),
            address_editing: false,
            query: String::new(),
            home: BrowserHome {
                places: Vec::new(),
                drives: Vec::new(),
            },
            entries: Vec::new(),
            rows: Vec::new(),
            cursor: None,
            columns: 2,
            list: ListState::new(0, ListAlignment::Top, px(200.0)),
            scrollbar: ScrollbarState::default(),
            loading: false,
            error: None,
            feedback: None,
            opening: false,
            next_work: 0,
            directory_tasks: Vec::new(),
            pending_directory: None,
            drive_tasks: Vec::new(),
            drive_queue: VecDeque::new(),
            action_task: None,
            home_task: None,
            home_loaded: false,
            fixture: false,
            embedded: None,
            filter_open: false,
            detail: false,
            more_open: false,
        };
        this.configure_inputs(cx);
        this
    }

    pub fn new_embedded(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        parent_request: NativeSuperMenuPasteRequest,
        initial_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let width = tokens.file_browser_proposal.width;
        let mut this = Self::new(host, tokens, dark, window, cx);
        this.embedded = Some(EmbeddedBrowser::new(width));
        if this.bind_embedded_lease(parent_request, cx) {
            this.navigate(initial_path.to_string(), true, window, cx);
            this.ensure_sidebar_home(false, cx);
        }
        this
    }

    /// Rebind a retained panel to the current menu presentation. Navigation and
    /// input drafts remain local to the panel; old OS reads keep their slots.
    pub fn resume_embedded(
        &mut self,
        parent_request: NativeSuperMenuPasteRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bind_embedded_lease(parent_request, cx) {
            let reload = self.embedded.as_mut().is_some_and(|embedded| {
                let reload = embedded.reload_on_resume;
                embedded.reload_on_resume = false;
                reload
            });
            if reload { self.reload(cx); }
            self.ensure_sidebar_home(false, cx);
            if self.path.is_empty() && self.home_loaded && !self.fixture {
                self.drive_queue = self.home.drives.iter().filter(|d| d.kind == "fixed" && d.capacity.is_none())
                    .cloned().map(|d| (self.epoch, d)).collect();
                self.pump_drives(cx);
            }
            self.focus_embedded(window, cx);
            cx.notify();
        }
    }

    fn bind_embedded_lease(
        &mut self,
        parent_request: NativeSuperMenuPasteRequest,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(embedded) = self.embedded.as_mut() else {
            return false;
        };
        if let Some(old) = embedded.lease.take() {
            old.retire();
        }
        self.display = self.display.wrapping_add(1);
        self.epoch = self.epoch.wrapping_add(1);
        match self.host.file_browser_create_panel_lease(&parent_request) {
            Ok(lease) => {
                embedded.lease = Some(lease);
                self.active = true;
                true
            }
            Err(error) => {
                self.active = false;
                self.feedback = Some(error);
                cx.notify();
                false
            }
        }
    }

    pub fn suspend_embedded(&mut self, cx: &mut Context<Self>) {
        if self.embedded.is_none() {
            return;
        }
        self.suspend_content(cx);
    }

    pub fn set_embedded_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if !width.is_finite() || width <= 0.0 {
            return;
        }
        if let Some(embedded) = &mut self.embedded {
            if (embedded.width - width).abs() > 0.5 {
                embedded.width = width;
                cx.notify();
            }
        }
    }

    pub fn current_path(&self) -> &str {
        &self.path
    }

    /// Parent-owned chrome: whether a second panel fits, the footer verb for
    /// the accepted target, and the current folder's file-menu membership.
    pub fn set_embedded_chrome(&mut self, can_split: bool, go_label: &str, in_file_menu: bool, cx: &mut Context<Self>) {
        if let Some(embedded) = &mut self.embedded {
            if embedded.can_split != can_split || embedded.go_label != go_label || embedded.in_file_menu != in_file_menu {
                embedded.can_split = can_split;
                embedded.go_label = go_label.to_string();
                embedded.in_file_menu = in_file_menu;
                cx.notify();
            }
        }
    }

    /// The selected folder, else the current folder: what the footer sends.
    fn go_target(&self) -> Option<String> {
        if self.loading || self.error.is_some() {
            return None;
        }
        if let Some(Row::Entry(index)) = self.cursor.and_then(|index| self.rows.get(index)) {
            if let Some(entry) = self.entries.get(*index).filter(|entry| entry.is_dir) {
                return Some(entry.path.clone());
            }
        }
        (!self.path.is_empty()).then(|| self.path.clone())
    }

    fn open_filter(&mut self, text: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_open = true;
        self.more_open = false;
        if let Some(text) = text {
            let next = format!("{}{text}", self.query);
            self.filter.update(cx, |input, cx| input.set_text(next.clone(), window, cx));
            self.query = next;
            self.cursor = None;
            self.reproject(cx);
        }
        self.filter.read(cx).focus_handle().focus(window);
        cx.notify();
    }

    pub fn navigate_embedded(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.embedded.is_some() && self.current() {
            self.navigate(path.to_string(), true, window, cx);
        }
    }

    pub fn focus_embedded(&self, window: &mut Window, cx: &App) {
        if !self.current() {
            return;
        }
        if self.address_editing || self.address.read(cx).is_composing() {
            self.address.read(cx).focus_handle().focus(window);
        } else if self.filter.read(cx).is_composing() {
            self.filter.read(cx).focus_handle().focus(window);
        } else {
            self.focus.focus(window);
        }
    }

    fn content_width(&self, window: &Window) -> f32 {
        self.embedded.as_ref().map_or_else(
            || f32::from(window.viewport_size().width),
            |embedded| embedded.width,
        )
    }

    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut App,
    ) -> Result<(), String> {
        let state = host.file_browser_snapshot();
        if !state.visible {
            return Ok(());
        }
        let incoming = tokens.clone();
        let handle = if let Some(handle) = existing {
            *handle
        } else {
            let build_host = host.clone();
            let p = tokens.file_browser_proposal;
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
                        window.set_window_title(tr("文件", "Files"));
                        let entity = cx.new(|cx| Self::new(build_host, tokens, dark, window, cx));
                        let weak = entity.downgrade();
                        window.on_window_should_close(cx, move |window, cx| {
                            let _ = weak.update(cx, |this, cx| this.dismiss(window, cx));
                            false
                        });
                        entity
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
                host.file_browser_set_hwnd(platform::hwnd(window)?.0 as isize);
                let current = host.file_browser_snapshot();
                this.display = current.generation;
                this.active = true;
                this.navigate(current.path, true, window, cx);
                this.ensure_sidebar_home(false, cx);
                let owner = host.clone();
                let generation = this.display;
                let _guard = platform::guard_condition(move || {
                    let state = owner.file_browser_snapshot();
                    state.visible && state.generation == generation
                });
                platform::set_no_activate(window, false)?;
                platform::show(window, true, true)?;
                this.focus.focus(window);
                Ok::<_, String>(())
            })
            .map_err(|error| error.to_string())??;
        Ok(())
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| this.suspend(window, cx));
    }

    pub fn maintain(handle: WindowHandle<Self>, host: &HostState, dark: bool, cx: &mut App) {
        let state = host.file_browser_snapshot();
        let _ = handle.update(cx, |this, window, cx| {
            if !state.visible || state.generation != this.display {
                if this.active {
                    this.suspend(window, cx);
                }
                return;
            }
            // File browsing intentionally survives focus changes.
            if this.dark != dark {
                this.set_design_tokens(this.tokens.clone(), dark, cx);
            }
            if let Some(error) = platform::take_error(window).ok().flatten() {
                this.feedback = Some(error);
                cx.notify();
            }
        });
    }

    pub fn initialize_native_window(
        &mut self,
        window: &Window,
        dimensions: Option<(f64, f64)>,
        no_activate: bool,
    ) -> Result<(), String> {
        if !self.initialized {
            let p = self.tokens.file_browser_proposal;
            let dimensions = dimensions.unwrap_or((f64::from(p.width), f64::from(p.height)));
            if !dimensions.0.is_finite()
                || !dimensions.1.is_finite()
                || dimensions.0 <= 0.0
                || dimensions.1 <= 0.0
            {
                return Err("Invalid file browser initial dimensions".into());
            }
            let _configuration = platform::guard_condition(|| true);
            platform::configure_host(window)?;
            platform::initialize_ceabox_geometry(window, dimensions)?;
            self.initialized = true;
        }
        platform::set_no_activate(window, no_activate)
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        self.configure_inputs(cx);
        cx.notify();
    }

    fn configure_inputs(&mut self, cx: &mut Context<Self>) {
        let t = self.tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&t.search_background),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: gpui::transparent_black(),
            selection: color(&t.selected),
            caret: color(&t.accent),
        };
        for input in [&self.address, &self.filter] {
            input.update(cx, |input, cx| {
                input.set_colors(colors, cx);
                input.set_metrics(30.0, 11.0, 0.0, cx);
                input.set_double_click_clears(false);
            });
        }
        self.filter.update(cx, |input, cx| {
            input.set_placeholder(tr("筛选当前目录名称", "Filter names in this folder"), cx)
        });
        self.address.update(cx, |input, cx| {
            input.set_placeholder(tr("输入文件夹路径", "Enter a folder path"), cx)
        });
    }

    fn current(&self) -> bool {
        if !self.active {
            return false;
        }
        if let Some(embedded) = &self.embedded {
            return embedded
                .lease
                .as_ref()
                .is_some_and(|lease| self.host.file_browser_panel_lease_is_current(lease));
        }
        if self.fixture {
            return true;
        }
        let state = self.host.file_browser_snapshot();
        state.visible && state.generation == self.display
    }

    fn suspend_content(&mut self, cx: &mut Context<Self>) {
        if let Some(embedded) = &mut self.embedded {
            embedded.reload_on_resume |= self.loading;
        }
        self.active = false;
        self.epoch = self.epoch.wrapping_add(1);
        self.pending_directory = None;
        self.drive_queue.clear();
        self.loading = false;
        if self.embedded.is_none() {
            self.address_editing = false;
        }
        self.scrollbar.cancel();
        if let Some(embedded) = &mut self.embedded {
            if let Some(lease) = embedded.lease.take() {
                lease.retire();
            }
        }
        // Keep bounded worker handles until completion; dropping a future cannot
        // interrupt a synchronous OS read and must not enable another spawn.
        cx.notify();
    }

    fn suspend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.suspend_content(cx);
        let _ = platform::hide(window);
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.embedded.is_some() {
            cx.emit(FileBrowserEvent::Dismiss);
            return;
        }
        self.host.file_browser_hide();
        self.suspend(window, cx);
    }

    fn navigate(&mut self, path: String, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) {
            return;
        }
        let path = if path.trim().is_empty() || matches!(path.trim(), "主页" | "Home") {
            String::new()
        } else {
            match file_browser::browser_normalize_path(&path) {
                Ok(path) => path,
                Err(error) => {
                    self.feedback = Some(error);
                    cx.notify();
                    return;
                }
            }
        };
        if push && self.path != path {
            self.history.truncate(self.history_index + 1);
            self.history.push(path.clone());
            self.history_index = self.history.len() - 1;
        }
        self.path = path;
        self.address_editing = false;
        self.filter_open = false;
        self.more_open = false;
        self.query.clear();
        self.cursor = None;
        self.error = None;
        self.feedback = None;
        self.address.update(cx, |input, cx| {
            input.set_text(self.path.clone(), window, cx)
        });
        self.filter.update(cx, |input, cx| {
            input.set_text(String::new(), window, cx);
            input.set_placeholder(
                if self.path.is_empty() {
                    tr("筛选主页位置", "Filter Home locations")
                } else {
                    tr("筛选当前目录名称", "Filter names in this folder")
                },
                cx,
            );
        });
        self.focus.focus(window);
        self.reload(cx);
    }

    fn history_move(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) {
            return;
        }
        let next = if backwards {
            self.history_index.saturating_sub(1)
        } else {
            (self.history_index + 1).min(self.history.len().saturating_sub(1))
        };
        if next != self.history_index {
            self.history_index = next;
            self.navigate(self.history[next].clone(), false, window, cx);
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.loading = true;
        self.entries.clear();
        self.rows.clear();
        self.list.reset(0);
        self.error = None;
        self.drive_queue.clear();
        self.pending_directory = Some((self.epoch, self.path.clone()));
        if self.fixture {
            self.load_fixture(cx);
            return;
        }
        if self.path.is_empty() {
            self.pending_directory = None;
            self.loading = !self.home_loaded;
            self.reproject(cx);
            self.ensure_sidebar_home(true, cx);
            return;
        }
        self.pump_directory(cx);
        cx.notify();
    }

    fn ensure_sidebar_home(&mut self, refresh: bool, cx: &mut Context<Self>) {
        if self.fixture || (!refresh && self.home_loaded) || self.home_task.is_some() {
            return;
        }
        self.home_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { file_browser::browser_home() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.home_task = None;
                // Home is shared location metadata, independent of a selected
                // directory. Cache it while hidden, but never revive a window
                // or replace directory rows. It owns a dedicated bounded slot
                // so two unavailable network folders cannot block Home.
                match result {
                    Ok(home) => {
                        if !this.home_loaded || (this.current() && this.path.is_empty()) {
                            this.home = home;
                            this.home_loaded = true;
                            if this.current() && this.path.is_empty() {
                                this.loading = false;
                                this.error = None;
                                this.drive_queue = this
                                    .home
                                    .drives
                                    .iter()
                                    .filter(|drive| drive.kind == "fixed")
                                    .cloned()
                                    .map(|drive| (this.epoch, drive))
                                    .collect();
                                this.pump_drives(cx);
                                this.reproject_preserving(cx);
                            }
                            if this.current() {
                                cx.notify();
                            }
                        }
                    }
                    Err(error) => {
                        if this.current() && this.path.is_empty() && !this.home_loaded {
                            this.loading = false;
                            this.error = Some(error);
                            this.reproject(cx);
                        }
                    }
                }
            });
        }));
    }

    fn pump_directory(&mut self, cx: &mut Context<Self>) {
        if !self.current() || self.directory_tasks.len() >= DIRECTORY_WORKERS {
            return;
        }
        let Some((epoch, path)) = self.pending_directory.take() else {
            return;
        };
        self.next_work = self.next_work.wrapping_add(1);
        let id = self.next_work;
        let display = self.display;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _permit =
                        BrowserIoPermit::try_acquire(&DIRECTORY_IO, GLOBAL_DIRECTORY_WORKERS)
                            .ok_or_else(|| {
                                tr(
                                    "其他目录仍在读取，请稍后重试。",
                                    "Other folders are still being read. Retry shortly.",
                                )
                                .to_string()
                            })?;
                    file_browser::browser_read_directory(&path)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.directory_tasks.retain(|(work, _)| *work != id);
                if this.current() && this.epoch == epoch && this.display == display {
                    this.loading = false;
                    match result {
                        Ok(directory) => {
                            this.path = directory.path;
                            this.entries = directory.entries;
                        }
                        Err(error) => this.error = Some(error),
                    }
                    this.reproject(cx);
                }
                this.pump_directory(cx);
                cx.notify();
            });
        });
        self.directory_tasks.push((id, task));
    }

    fn pump_drives(&mut self, cx: &mut Context<Self>) {
        if !self.current() || !self.path.is_empty() {
            return;
        }
        let in_flight = self
            .drive_tasks
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect::<HashSet<_>>();
        let mut available = DRIVE_WORKERS.saturating_sub(self.drive_tasks.len());
        let mut delayed = VecDeque::new();
        while let Some((epoch, drive)) = self.drive_queue.pop_front() {
            if epoch != self.epoch {
                continue;
            }
            if available == 0 || in_flight.contains(&drive.path) {
                delayed.push_back((epoch, drive));
                continue;
            }
            available -= 1;
            self.next_work = self.next_work.wrapping_add(1);
            let id = self.next_work;
            let path = drive.path.clone();
            let display = self.display;
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let _permit = BrowserIoPermit::try_acquire(&DRIVE_IO, GLOBAL_DRIVE_WORKERS)
                            .ok_or_else(|| {
                                tr(
                                    "其他磁盘仍在读取，请稍后重试。",
                                    "Other drives are still being read. Retry shortly.",
                                )
                                .to_string()
                            })?;
                        file_browser::browser_drive_details(&drive)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.drive_tasks.retain(|(work, _, _)| *work != id);
                    if this.current()
                        && this.display == display
                        && this.epoch == epoch
                        && this.path.is_empty()
                    {
                        if let Ok(drive) = result {
                            if let Some(current) = this
                                .home
                                .drives
                                .iter_mut()
                                .find(|item| item.path == drive.path)
                            {
                                *current = drive;
                            }
                        }
                        this.reproject_preserving(cx);
                    }
                    this.pump_drives(cx);
                });
            });
            self.drive_tasks.push((id, path, task));
        }
        self.drive_queue = delayed;
    }

    fn reproject(&mut self, cx: &mut Context<Self>) {
        let query = self.query.trim().to_lowercase();
        self.rows.clear();
        if !self.loading && self.error.is_none() {
            if self.path.is_empty() {
                let places = self
                    .home
                    .places
                    .iter()
                    .filter(|item| item.name.to_lowercase().contains(&query))
                    .cloned()
                    .collect::<Vec<_>>();
                let drives = self
                    .home
                    .drives
                    .iter()
                    .filter(|item| item.name.to_lowercase().contains(&query))
                    .cloned()
                    .collect::<Vec<_>>();
                if !places.is_empty() {
                    self.rows
                        .push(Row::Label(tr("常用位置", "Common locations"), places.len()));
                    let per_row = if self.embedded.is_some() { 1 } else { 2 };
                    self.rows
                        .extend(places.chunks(per_row).map(|items| Row::Places(items.to_vec())));
                }
                if !drives.is_empty() {
                    self.rows.push(Row::Label(
                        tr("设备和驱动器", "Devices and drives"),
                        drives.len(),
                    ));
                    let per_row = if self.embedded.is_some() { 1 } else { self.columns };
                    self.rows.extend(
                        drives
                            .chunks(per_row)
                            .map(|items| Row::Drives(items.to_vec())),
                    );
                }
                if !self.rows.is_empty() && self.embedded.is_none() {
                    self.rows.push(Row::Hint);
                }
            } else {
                self.rows.extend(
                    self.entries
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| item.name.to_lowercase().contains(&query))
                        .map(|(index, _)| Row::Entry(index)),
                );
            }
        }
        self.list.reset(self.rows.len());
        self.cursor = self.cursor.filter(|index| *index < self.rows.len());
        cx.notify();
    }

    fn reproject_preserving(&mut self, cx: &mut Context<Self>) {
        let mut top = self.list.logical_scroll_top();
        self.reproject(cx);
        top.item_ix = top.item_ix.min(self.rows.len().saturating_sub(1));
        self.list.scroll_to(top);
    }

    fn edit_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) {
            return;
        }
        self.address_editing = true;
        self.address.update(cx, |input, cx| {
            input.set_text(self.path.clone(), window, cx);
            input.select_all_text(cx);
        });
        self.address.read(cx).focus_handle().focus(window);
        cx.notify();
    }

    pub fn is_composing(&self, cx: &App) -> bool {
        self.address.read(cx).is_composing() || self.filter.read(cx).is_composing()
    }

    fn open_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Row::Entry(index)) = self.cursor.and_then(|index| self.rows.get(index)) {
            if let Some(entry) = self.entries.get(*index).cloned() {
                self.open_entry(entry, window, cx);
            }
        }
    }

    fn open_entry(&mut self, entry: BrowserEntry, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() || self.loading || self.is_composing(cx) {
            return;
        }
        if entry.is_dir {
            self.navigate(entry.path, true, window, cx);
            return;
        }
        if self.opening {
            return;
        }
        if self.fixture {
            self.feedback = Some(format!(
                "{} {}",
                tr(
                    "演示：使用默认应用打开",
                    "Demo: open with default application"
                ),
                entry.name
            ));
            cx.notify();
            return;
        }
        let captured = if let Some(embedded) = &self.embedded {
            embedded
                .lease
                .as_ref()
                .ok_or_else(|| tr("文件面板已关闭。", "The file panel is closed.").to_string())
                .and_then(|lease| self.host.file_browser_capture_panel_open(lease))
        } else {
            self.host.file_browser_capture_open()
        };
        let ticket = match captured {
            Ok(ticket) => ticket,
            Err(error) => {
                self.feedback = Some(error);
                cx.notify();
                return;
            }
        };
        let host = self.host.clone();
        let epoch = self.epoch;
        let display = self.display;
        self.opening = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.file_browser_open_file(&entry.path, &ticket) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.opening = false;
                if this.current() && this.epoch == epoch && this.display == display {
                    if let Err(error) = result {
                        this.feedback = Some(error);
                    }
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() || self.is_composing(cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let text_focused = self.address.read(cx).focus_handle().is_focused(window)
            || self.filter.read(cx).focus_handle().is_focused(window);
        let typed = event.keystroke.key_char.as_deref().filter(|text| {
            let mut chars = text.chars();
            chars.next().is_some_and(|ch| !ch.is_whitespace() && !ch.is_control()) && chars.next().is_none()
        });
        if modifiers.control && !modifiers.alt && !modifiers.shift && key == "l" {
            self.edit_address(window, cx);
        } else if self.embedded.is_some() && modifiers.control && !modifiers.alt && !modifiers.shift && key == "f" {
            self.open_filter(None, window, cx);
        } else if self.embedded.is_some() && !text_focused && !modifiers.control && !modifiers.alt
            && !modifiers.platform && typed.is_some()
        {
            // Type-to-filter, like the docked design; IME text still starts
            // from the filter button so composition stays inside an input.
            self.open_filter(typed.map(str::to_string), window, cx);
        } else if modifiers.alt
            && !modifiers.control
            && !modifiers.shift
            && matches!(key, "left" | "right" | "up")
        {
            if key == "left" {
                self.history_move(true, window, cx);
            } else if key == "right" {
                self.history_move(false, window, cx);
            } else if !self.path.is_empty() {
                self.navigate(parent_path(&self.path), true, window, cx);
            }
        } else if modifiers.number_of_modifiers() == 0 {
            if key == "escape" && self.more_open {
                self.more_open = false;
                cx.notify();
            } else if key == "escape" && !text_focused {
                self.dismiss(window, cx);
            } else if key == "backspace" && !text_focused && !self.path.is_empty() {
                self.navigate(parent_path(&self.path), true, window, cx);
            } else if key == "f5" {
                self.reload(cx);
            } else if !text_focused
                && matches!(key, "up" | "down")
                && !self.path.is_empty()
                && !self.rows.is_empty()
            {
                self.cursor = Some(if key == "down" {
                    self.cursor
                        .map_or(0, |index| (index + 1).min(self.rows.len() - 1))
                } else {
                    self.cursor.unwrap_or(0).saturating_sub(1)
                });
                self.list.scroll_to_reveal_item(self.cursor.unwrap());
                cx.notify();
            } else if key == "enter" && !text_focused {
                self.open_cursor(window, cx);
            } else {
                return;
            }
        } else {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    fn icon(&self, name: &str, size: f32) -> crate::crisp_svg::CrispSvg {
        svg()
            .path(self.tokens.icons.get(name).cloned().unwrap_or_default())
            .size(px(size))
            .flex_none()
            .text_color(color(&self.tokens.theme(self.dark).text_secondary))
    }

    fn icon_button(
        &self,
        id: impl Into<SharedString>,
        icon: &str,
        enabled: bool,
    ) -> Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        div()
            .id(id.into())
            .w(px(28.0))
            .h(px(30.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .text_color(color(&t.text_secondary).opacity(if enabled { 1.0 } else { 0.28 }))
            .when(enabled, |element| {
                element
                    .cursor_pointer()
                    .hover(|style| style.bg(color(&t.hover)))
            })
            .child(
                self.icon(icon, 16.0)
                    .text_color(color(&t.text_secondary).opacity(if enabled { 1.0 } else { 0.28 })),
            )
    }

    fn button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        div()
            .id(id.into())
            .min_h(px(29.0))
            .px(px(11.0))
            .py(px(4.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(color(&t.border))
            .text_size(px(11.0))
            .text_color(color(&t.text_secondary))
            .cursor_pointer()
            .hover(|style| style.bg(color(&t.hover)))
            .child(label.into())
    }

    fn drive_icon(&self, drive: &BrowserDrive, small: bool) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        let size = if small { 18.0 } else { 43.0 };
        if drive.kind == "network" {
            return div()
                .w(px(size))
                .h(px(size))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(self.icon("cloud", size).text_color(color(&t.accent)));
        }
        let body = div()
            .relative()
            .w(px(size))
            .h(px(size * 0.78))
            .flex_none()
            .border_1()
            .border_color(color(&t.text_muted).opacity(0.7))
            .rounded(px(if small { 2.0 } else { 4.0 }))
            .bg(color(&t.secondary))
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .h(px(size * 0.27))
                    .border_t_1()
                    .border_color(color(&t.text_muted).opacity(0.55))
                    .bg(color(&t.surface))
                    .rounded_b(px(3.0))
                    .child(
                        div()
                            .absolute()
                            .right(px(size * 0.12))
                            .top(px(size * 0.09))
                            .size(px(if small { 2.0 } else { 3.0 }))
                            .rounded_full()
                            .bg(color(&t.accent)),
                    ),
            );
        let body = body.when(drive.system && !small, |element| {
            element.child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .left(px(14.0))
                    .size(px(13.0))
                    .flex()
                    .flex_wrap()
                    .gap(px(1.0))
                    .children((0..4).map(|_| div().size(px(5.0)).bg(color(&t.accent)))),
            )
        });
        div()
            .w(px(size))
            .h(px(size))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(body)
    }

    fn sidebar(&self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let p = self.tokens.file_browser_proposal;
        let mut column = div()
            .id("file-browser-sidebar")
            .w(px(if narrow {
                p.narrow_sidebar_width
            } else {
                p.sidebar_width
            }))
            .h_full()
            .flex_none()
            .overflow_y_scroll()
            .p(px(8.0))
            .border_r_1()
            .border_color(color(&t.border))
            .bg(color(&t.surface));
        let row = |id: String, name: String, active: bool, icon: AnyElement| {
            div()
                .id(SharedString::from(id))
                .h(px(34.0))
                .w_full()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(10.0))
                .mb(px(2.0))
                .rounded(px(5.0))
                .text_size(px(12.0))
                .text_color(color(if active { &t.accent } else { &t.text_secondary }))
                .bg(if active {
                    color(&t.tab_active)
                } else {
                    gpui::transparent_black()
                })
                .cursor_pointer()
                .hover(|style| style.bg(color(&t.hover)))
                .child(icon)
                .child(div().flex_1().min_w_0().truncate().child(name))
        };
        column = column.child(
            row(
                "file-browser-home".into(),
                tr("主页", "Home").into(),
                self.path.is_empty(),
                self.icon("all", 17.0).into_any_element(),
            )
            .on_click(
                cx.listener(|this, _, window, cx| this.navigate(String::new(), true, window, cx)),
            ),
        );
        column = column.child(
            div()
                .mt(px(18.0))
                .mb(px(8.0))
                .px(px(10.0))
                .text_size(px(10.0))
                .text_color(color(&t.text_muted))
                .child(tr("常用位置", "Common locations")),
        );
        for (index, place) in self.home.places.iter().enumerate() {
            let path = place.path.clone();
            let active = self.path.eq_ignore_ascii_case(&path);
            column = column.child(
                row(
                    format!("file-browser-place-{index}"),
                    place.name.clone(),
                    active,
                    self.icon(place_icon(&place.kind), 17.0).into_any_element(),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.navigate(path.clone(), true, window, cx)
                })),
            );
        }
        column = column.child(
            div()
                .mt(px(18.0))
                .mb(px(8.0))
                .px(px(10.0))
                .text_size(px(10.0))
                .text_color(color(&t.text_muted))
                .child(tr("磁盘", "Drives")),
        );
        for (index, drive) in self.home.drives.iter().enumerate() {
            let path = drive.path.clone();
            let active =
                !self.path.is_empty() && self.path.to_lowercase().starts_with(&path.to_lowercase());
            column = column.child(
                row(
                    format!("file-browser-drive-{index}"),
                    drive.name.clone(),
                    active,
                    self.drive_icon(drive, true).into_any_element(),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.navigate(path.clone(), true, window, cx)
                })),
            );
        }
        column.into_any_element()
    }

    fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let t = self.tokens.theme(self.dark).clone();
        if self.embedded.is_some() {
            return self.render_docked_row(index, row, cx);
        }
        match row {
            Row::Label(label, count) => div()
                .w_full()
                .h(px(if label == tr("设备和驱动器", "Devices and drives") {
                    55.0
                } else {
                    43.0
                }))
                .pt(px(12.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(12.0))
                .child(label)
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(color(&t.text_muted))
                        .child(count.to_string()),
                )
                .into_any_element(),
            Row::Hint => div()
                .w_full()
                .h(px(55.0))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .text_size(px(10.0))
                .text_color(color(&t.text_muted))
                .child(self.icon("keyboard", 15.0))
                .child(tr(
                    "Ctrl+L 输入路径  ·  Alt+← 返回",
                    "Ctrl+L Enter path  ·  Alt+← Back",
                ))
                .into_any_element(),
            Row::Places(places) => {
                let mut row = div()
                    .w_full()
                    .h(px(69.0))
                    .px(px(10.0))
                    .pb(px(8.0))
                    .flex()
                    .gap(px(12.0));
                for (cell, place) in places.into_iter().enumerate() {
                    let path = place.path.clone();
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("file-place-{index}-{cell}")))
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(13.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(color(&t.border))
                            .cursor_pointer()
                            .hover(|style| style.bg(color(&t.hover)))
                            .child(
                                self.icon(place_icon(&place.kind), 23.0)
                                    .text_color(color(&t.accent)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(div().text_size(px(12.0)).truncate().child(place.name))
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(color(&t.text_muted))
                                            .truncate()
                                            .child(path_title(&path)),
                                    ),
                            )
                            .child(
                                self.icon("chevron-right", 13.0)
                                    .text_color(color(&t.text_muted)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(path.clone(), true, window, cx)
                            })),
                    );
                }
                if cell_count_from_row(&self.rows[index]) == 1 {
                    row = row.child(div().flex_1());
                }
                row.into_any_element()
            }
            Row::Drives(drives) => {
                let count = drives.len();
                let mut row = div()
                    .w_full()
                    .h(px(116.0))
                    .px(px(10.0))
                    .pb(px(11.0))
                    .flex()
                    .gap(px(12.0));
                for (cell, drive) in drives.into_iter().enumerate() {
                    let path = drive.path.clone();
                    let mut info = div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(7.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .truncate()
                                .child(drive.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(color(&t.text_muted))
                                .child(if drive.system {
                                    tr("本地磁盘 · 系统", "Local drive · System")
                                } else {
                                    drive_kind(&drive.kind)
                                }),
                        );
                    if let Some(capacity) = &drive.capacity {
                        let used = if capacity.total_bytes == 0 {
                            0.0
                        } else {
                            (1.0 - capacity.free_bytes.min(capacity.total_bytes) as f32
                                / capacity.total_bytes as f32)
                                .clamp(0.0, 1.0)
                        };
                        info = info
                            .child(
                                div()
                                    .h(px(5.0))
                                    .w_full()
                                    .rounded(px(3.0))
                                    .bg(color(&t.tertiary))
                                    .overflow_hidden()
                                    .child(
                                        div().h_full().w(relative(used)).rounded(px(3.0)).bg(
                                            color(if used >= 0.9 { &t.error } else { &t.accent }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(6.0))
                                    .text_size(px(10.0))
                                    .text_color(color(&t.text_muted))
                                    .child(format!(
                                        "{} {}",
                                        display_size(capacity.free_bytes),
                                        tr("可用", "free")
                                    ))
                                    .child(div().flex_1())
                                    .child(format!(
                                        "{} {}",
                                        tr("共", "of"),
                                        display_size(capacity.total_bytes)
                                    )),
                            );
                    } else {
                        info = info.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(color(&t.text_muted))
                                .child(tr("容量暂不可用", "Capacity unavailable")),
                        );
                    }
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("file-drive-{index}-{cell}")))
                            .h_full()
                            .flex_1()
                            .min_w_0()
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(color(&t.border))
                            .cursor_pointer()
                            .hover(|style| style.bg(color(&t.hover)))
                            .child(self.drive_icon(&drive, false))
                            .child(info)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(path.clone(), true, window, cx)
                            })),
                    );
                }
                if self.columns == 2 && count == 1 {
                    row = row.child(div().flex_1());
                }
                row.into_any_element()
            }
            Row::Entry(entry_index) => {
                let Some(entry) = self.entries.get(entry_index).cloned() else {
                    return div().into_any_element();
                };
                let narrow = self.content_width(window)
                    <= self.tokens.file_browser_proposal.narrow_breakpoint;
                let selected = self.cursor == Some(index);
                let open = entry.clone();
                div()
                    .id(SharedString::from(format!("file-entry-{entry_index}")))
                    .h(px(self.tokens.file_browser_proposal.row_height))
                    .px(px(8.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(4.0))
                    .bg(if selected {
                        color(&t.selected)
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|style| style.bg(color(&t.hover)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .text_size(px(11.0))
                            .child(
                                self.icon(entry_icon(&entry), 19.0)
                                    .text_color(color(&t.accent)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(entry.name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .w(px(if narrow { 112.0 } else { 135.0 }))
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(color(&t.text_muted))
                            .truncate()
                            .child(display_time(entry.modified_unix_ms)),
                    )
                    .when(!narrow, |row| {
                        row.child(
                            div()
                                .w(px(91.0))
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(color(&t.text_muted))
                                .truncate()
                                .child(kind_label(&entry)),
                        )
                    })
                    .child(
                        div()
                            .w(px(65.0))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .text_size(px(10.0))
                            .text_color(color(&t.text_muted))
                            .child(
                                entry
                                    .size_bytes
                                    .map(display_size)
                                    .unwrap_or_else(|| "—".into()),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            this.cursor = Some(index);
                            this.address_editing = false;
                            this.focus.focus(window);
                            cx.notify();
                            if event.click_count == 2 {
                                this.open_entry(open.clone(), window, cx);
                            }
                        }),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_address(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let mut row = div()
            .id("file-browser-address")
            .flex_1()
            .min_w(px(100.0))
            .h(px(33.0))
            .px(px(4.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(color(if self.address_editing {
                &t.accent
            } else {
                &t.border
            }))
            .bg(color(&t.search_background))
            .flex()
            .items_center()
            .overflow_hidden();
        if self.address_editing {
            return row.child(self.address.clone()).into_any_element();
        }
        row = row.child(
            self.icon_button("file-address-home", "desktop", true)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.navigate(String::new(), true, window, cx)
                })),
        );
        if self.path.is_empty() {
            row = row.child(
                self.button("file-address-home-name", tr("主页", "Home"))
                    .border_0()
                    .on_click(cx.listener(|this, _, window, cx| this.edit_address(window, cx))),
            );
        } else {
            let mut crumbs = Vec::new();
            let mut path = self.path.clone();
            while !path.is_empty() && crumbs.len() < 128 {
                crumbs.push((path_title(&path), path.clone()));
                let parent = parent_path(&path);
                if parent == path {
                    break;
                }
                path = parent;
            }
            crumbs.reverse();
            for (index, (title, path)) in crumbs.into_iter().enumerate() {
                row = row
                    .child(
                        self.icon("chevron-right", 11.0)
                            .text_color(color(&t.text_muted)),
                    )
                    .child(
                        self.button(SharedString::from(format!("file-crumb-{index}")), title)
                            .min_w_0()
                            .px(px(6.0))
                            .border_0()
                            .truncate()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(path.clone(), true, window, cx)
                            })),
                    );
            }
        }
        row.child(div().flex_1().min_w(px(0.0)))
            .child(
                self.icon_button("file-address-edit", "edit", true)
                    .on_click(cx.listener(|this, _, window, cx| this.edit_address(window, cx))),
            )
            .into_any_element()
    }

    fn item_count(&self) -> usize {
        self.rows
            .iter()
            .map(|row| match row {
                Row::Entry(_) => 1,
                Row::Places(items) => items.len(),
                Row::Drives(items) => items.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn diagnostics(&self, cx: &App) -> Value {
        json!({"active":self.active,"path":self.path,"loading":self.loading,"error":self.error,"feedback":self.feedback,"display":self.display,"epoch":self.epoch,"historyIndex":self.history_index,"history":self.history,"rows":self.rows.len(),"scrollItem":self.list.logical_scroll_top().item_ix,"scrollOffset":f32::from(self.list.logical_scroll_top().offset_in_item),"items":self.item_count(),"selected":self.cursor,"query":self.query,"addressEditing":self.address_editing,"directoryWorkers":self.directory_tasks.len(),"driveWorkers":self.drive_tasks.len(),"opening":self.opening,"fixture":self.fixture,"embedded":self.embedded.is_some(),"embeddedWidth":self.embedded.as_ref().map(|embedded|embedded.width),"panelLeaseCurrent":self.embedded.as_ref().and_then(|embedded|embedded.lease.as_ref()).is_some_and(|lease|self.host.file_browser_panel_lease_is_current(lease)),"address":self.address.read(cx).diagnostics(),"filter":self.filter.read(cx).diagnostics()})
    }

    #[cfg(debug_assertions)]
    pub fn new_fixture(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        assert!(
            cliphost_core::clipboard::is_headless(),
            "File browser visual fixtures require headless effects"
        );
        let mut this = Self::new(host, tokens, dark, window, cx);
        this.fixture = true;
        this.active = true;
        this.navigate(path.into(), true, window, cx);
        this
    }

    /// The shared synthetic content inside a real menu-owned panel. No home,
    /// drive or directory worker is created, and file opens remain feedback.
    #[cfg(debug_assertions)]
    pub fn new_embedded_fixture(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        parent_request: NativeSuperMenuPasteRequest,
        initial_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let width = tokens.file_browser_proposal.width;
        let mut this = Self::new_fixture(host, tokens, dark, initial_path, window, cx);
        this.embedded = Some(EmbeddedBrowser::new(width));
        this.bind_embedded_lease(parent_request, cx);
        this
    }

    fn load_fixture(&mut self, cx: &mut Context<Self>) {
        #[derive(serde::Deserialize)]
        struct Fixture {
            home: BrowserHome,
            directories: std::collections::HashMap<String, Vec<BrowserEntry>>,
            errors: std::collections::HashMap<String, String>,
        }
        let fixture: Fixture = serde_json::from_str(include_str!(
            "../../../../ui-design/file-browser.fixture.json"
        ))
        .expect("valid shared file browser fixture");
        self.loading = false;
        self.home = fixture.home;
        self.home_loaded = true;
        self.error = fixture.errors.get(&self.path).cloned();
        self.entries = fixture
            .directories
            .get(&self.path)
            .cloned()
            .unwrap_or_default();
        if !self.path.is_empty()
            && self.error.is_none()
            && !fixture.directories.contains_key(&self.path)
        {
            self.error = Some(
                tr(
                    "这个路径不在演示目录中。",
                    "This path is not part of the design fixture.",
                )
                .into(),
            );
        }
        self.pending_directory = None;
        self.reproject(cx);
    }
}
fn cell_count_from_row(row: &Row) -> usize {
    match row {
        Row::Places(items) => items.len(),
        _ => 0,
    }
}

impl FileBrowserWindow {
    fn docked_button(&self, id: impl Into<SharedString>, icon: &str, enabled: bool) -> Stateful<gpui::Div> {
        self.icon_button(id, icon, enabled).w(px(26.0)).h(px(26.0)).rounded(px(5.0))
    }

    fn capacity_bar(&self, used: f32, width: f32, height: f32) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        div()
            .w(px(width))
            .h(px(height))
            .flex_none()
            .rounded(px(height / 2.0))
            .bg(color(&t.tertiary))
            .overflow_hidden()
            .child(div().h_full().w(relative(used)).bg(color(if used >= 0.9 { &t.error } else { &t.accent })))
    }

    fn render_docked_row(&mut self, index: usize, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        match row {
            Row::Label(label, _) => div()
                .w_full()
                .h(px(28.0))
                .px(px(8.0))
                .pb(px(4.0))
                .flex()
                .items_end()
                .text_size(px(10.5))
                .text_color(color(&t.text_muted))
                .child(label)
                .into_any_element(),
            Row::Hint => div().into_any_element(),
            Row::Places(places) => {
                let mut row = div().w_full().flex().flex_col();
                for (cell, place) in places.into_iter().enumerate() {
                    let path = place.path.clone();
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("file-docked-place-{index}-{cell}")))
                            .h(px(30.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(9.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(color(&t.hover)))
                            .child(self.icon(place_icon(&place.kind), 16.0).text_color(color(&t.accent)))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(12.0)).child(place.name))
                            .child(div().flex_none().text_size(px(11.0)).text_color(color(&t.text_muted)).child(path_title(&path)))
                            .on_click(cx.listener(move |this, _, window, cx| this.navigate(path.clone(), true, window, cx))),
                    );
                }
                row.into_any_element()
            }
            Row::Drives(drives) => {
                let mut row = div().w_full().flex().flex_col();
                for (cell, drive) in drives.into_iter().enumerate() {
                    let path = drive.path.clone();
                    let capacity = match &drive.capacity {
                        Some(capacity) => {
                            let used = if capacity.total_bytes == 0 {
                                0.0
                            } else {
                                (1.0 - capacity.free_bytes.min(capacity.total_bytes) as f32 / capacity.total_bytes as f32).clamp(0.0, 1.0)
                            };
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(self.capacity_bar(used, 120.0, 5.0))
                                .child(format!("{} {}{} {}", display_size(capacity.free_bytes), tr("可用", "free"),
                                    tr("，共", ", of"), display_size(capacity.total_bytes)))
                        }
                        None => div().child(tr("容量暂不可用", "Capacity unavailable")),
                    };
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("file-docked-drive-{index}-{cell}")))
                            .h(px(44.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(11.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(color(&t.hover)))
                            .child(self.drive_icon(&drive, true))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(5.0))
                                    .child(div().truncate().text_size(px(12.0)).child(drive.name.clone()))
                                    .child(capacity.text_size(px(10.5)).text_color(color(&t.text_muted))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| this.navigate(path.clone(), true, window, cx))),
                    );
                }
                row.into_any_element()
            }
            Row::Entry(entry_index) => {
                let Some(entry) = self.entries.get(entry_index).cloned() else {
                    return div().into_any_element();
                };
                let selected = self.cursor == Some(index);
                let detail = self.detail;
                let open = entry.clone();
                let meta = |width: f32, text: String, end: bool| {
                    div()
                        .w(px(width))
                        .flex_none()
                        .flex()
                        .when(end, |cell| cell.justify_end())
                        .text_size(px(11.0))
                        .text_color(color(&t.text_muted))
                        .truncate()
                        .child(text)
                };
                div()
                    .id(SharedString::from(format!("file-docked-entry-{entry_index}")))
                    .h(px(28.0))
                    .px(px(8.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(5.0))
                    .bg(if selected { color(&t.selected) } else { gpui::transparent_black() })
                    .hover(|style| style.bg(color(&t.hover)))
                    .child(self.icon(entry_icon(&entry), 16.0).text_color(color(if entry.is_dir { &t.accent } else { &t.text_muted })))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.0)).child(entry.name.clone()))
                    .when(detail, |row| {
                        row.child(meta(106.0, display_time(entry.modified_unix_ms), false))
                            .child(meta(84.0, kind_label(&entry).to_string(), false))
                            .child(meta(60.0, entry.size_bytes.map(display_size).unwrap_or_else(|| "—".into()), true))
                    })
                    .when(!detail && !entry.is_dir, |row| {
                        row.child(meta(72.0, entry.size_bytes.map(display_size).unwrap_or_default(), true))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            this.more_open = false;
                            if event.modifiers.control && open.is_dir {
                                // Ctrl+click opens beside this panel, like the menu column.
                                if this.current() && !this.is_composing(cx) {
                                    cx.emit(FileBrowserEvent::OpenPanel(open.path.clone()));
                                }
                                return;
                            }
                            this.cursor = Some(index);
                            this.address_editing = false;
                            this.focus.focus(window);
                            cx.notify();
                            if event.click_count == 2 {
                                this.open_entry(open.clone(), window, cx);
                            }
                        }),
                    )
                    .into_any_element()
            }
        }
    }

    fn docked_crumbs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        if self.address_editing {
            return div()
                .flex_1()
                .min_w_0()
                .h(px(28.0))
                .px(px(4.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(color(&t.accent))
                .bg(color(&t.search_background))
                .flex()
                .items_center()
                .overflow_hidden()
                .child(self.address.clone())
                .into_any_element();
        }
        let mut crumbs = Vec::new();
        let mut path = self.path.clone();
        while !path.is_empty() && crumbs.len() < 128 {
            crumbs.push((path_title(&path), path.clone()));
            let parent = parent_path(&path);
            if parent == path {
                break;
            }
            path = parent;
        }
        crumbs.reverse();
        let shown: Vec<Option<(String, String)>> = if crumbs.len() > 3 {
            let tail = crumbs.split_off(crumbs.len() - 2);
            vec![Some(crumbs[0].clone()), None].into_iter().chain(tail.into_iter().map(Some)).collect()
        } else {
            crumbs.into_iter().map(Some).collect()
        };
        let count = shown.len();
        let mut row = div()
            .id("file-docked-crumbs")
            .flex_1()
            .min_w_0()
            .h(px(28.0))
            .px(px(2.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(5.0))
            .hover(|style| style.bg(color(&t.hover).opacity(0.5)))
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.edit_address(window, cx);
                }
            }))
            .child(
                div()
                    .id("file-docked-home")
                    .h(px(24.0))
                    .px(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(color(&t.hover)))
                    .child(self.icon("desktop", 14.0).text_color(color(&t.text_muted)))
                    .when(self.path.is_empty(), |home| {
                        home.child(div().text_size(px(12.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(tr("此电脑", "This PC")))
                    })
                    .tooltip(crate::settings::settings_help(tr("此电脑", "This PC").to_string(), t.clone()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.navigate(String::new(), true, window, cx);
                    })),
            );
        for (index, crumb) in shown.into_iter().enumerate() {
            let last = index + 1 == count;
            let keep = last || index == 0;
            let button = div()
                .id(SharedString::from(format!("file-docked-crumb-{index}")))
                .h(px(24.0))
                .px(px(6.0))
                .min_w(px(20.0))
                .rounded(px(4.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .truncate()
                .cursor_pointer()
                .hover(|style| style.bg(color(&t.hover)))
                .text_color(color(if last { &t.text } else { &t.text_secondary }))
                .when(last, |crumb| crumb.font_weight(gpui::FontWeight::SEMIBOLD));
            let button = match crumb {
                Some((title, path)) => button.child(title).on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.navigate(path.clone(), true, window, cx);
                })),
                None => button.child("…").on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.edit_address(window, cx);
                })),
            };
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .when(keep, |pair| pair.flex_shrink_0())
                    .when(last, |pair| pair.max_w(relative(0.62)))
                    .child(self.icon("chevron-right", 10.0).text_color(color(&t.text_muted)))
                    .child(button.min_w_0()),
            );
        }
        row.into_any_element()
    }

    fn more_item(&self, id: &'static str, icon: &str, label: &'static str, checked: bool, enabled: bool) -> Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        div()
            .id(id)
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(4.0))
            .text_size(px(12.0))
            .opacity(if enabled { 1.0 } else { 0.4 })
            .when(enabled, |item| item.cursor_pointer().hover(|style| style.bg(color(&t.hover))))
            .child(self.icon(icon, 14.0).text_color(color(&t.text_muted)))
            .child(div().flex_1().child(label))
            .when(checked, |item| item.child("✓"))
    }

    fn render_docked(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let (can_split, go_label, in_file_menu) = self
            .embedded
            .as_ref()
            .map(|embedded| (embedded.can_split, embedded.go_label.clone(), embedded.in_file_menu))
            .unwrap_or_default();
        let filter_visible = self.filter_open || !self.query.is_empty();
        let filter = if filter_visible {
            div()
                .w(px(150.0))
                .min_w(px(96.0))
                .flex_shrink()
                .h(px(28.0))
                .px(px(6.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(color(&t.border))
                .bg(color(&t.search_background))
                .overflow_hidden()
                .child(self.icon("search", 13.0).text_color(color(&t.text_muted)))
                .child(div().flex_1().min_w_0().child(self.filter.clone()))
                .when(!self.query.is_empty(), |filter| {
                    filter.child(
                        self.icon_button("file-docked-clear", "close", true)
                            .w(px(18.0))
                            .h(px(18.0))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.query.clear();
                                this.filter_open = false;
                                this.filter.update(cx, |input, cx| input.set_text(String::new(), window, cx));
                                this.reproject(cx);
                                this.focus.focus(window);
                            })),
                    )
                })
                .into_any_element()
        } else {
            self.docked_button("file-docked-filter", "search", true)
                .tooltip(crate::settings::settings_help(tr("筛选（直接输入或 Ctrl+F）", "Filter (type or Ctrl+F)").to_string(), t.clone()))
                .on_click(cx.listener(|this, _, window, cx| this.open_filter(None, window, cx)))
                .into_any_element()
        };
        let header = div()
            .h(px(40.0))
            .flex_none()
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .border_b_1()
            .border_color(color(&t.border))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .mr(px(2.0))
                    .child(self.docked_button("file-browser-back", "chevron-left", self.history_index > 0)
                        .on_click(cx.listener(|this, _, window, cx| this.history_move(true, window, cx))))
                    .child(self.docked_button("file-browser-forward", "chevron-right", self.history_index + 1 < self.history.len())
                        .on_click(cx.listener(|this, _, window, cx| this.history_move(false, window, cx))))
                    .child(self.docked_button("file-browser-up", "arrow-up", !self.path.is_empty())
                        .on_click(cx.listener(|this, _, window, cx| {
                            if !this.path.is_empty() {
                                this.navigate(parent_path(&this.path), true, window, cx);
                            }
                        }))),
            )
            .child(self.docked_crumbs(cx))
            .child(filter)
            .when(can_split, |header| {
                header.child(
                    self.docked_button("file-docked-split", "open-panel", true)
                        .tooltip(crate::settings::settings_help(tr("并排打开（Ctrl+单击文件夹）", "Open side by side (Ctrl+click a folder)").to_string(), t.clone()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.current() && !this.is_composing(cx) {
                                cx.emit(FileBrowserEvent::OpenPanel(this.path.clone()));
                            }
                        })),
                )
            })
            .child(
                self.docked_button("file-docked-more", "more", true)
                    .when(self.more_open, |button| button.bg(color(&t.hover)))
                    .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.more_open = !this.more_open;
                        cx.notify();
                    })),
            )
            .child(
                self.docked_button("file-docked-close", "close", true)
                    .tooltip(crate::settings::settings_help(tr("关闭浏览栏", "Close this panel").to_string(), t.clone()))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(FileBrowserEvent::Close))),
            );
        let mut body = div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .px(px(4.0))
            .pt(px(4.0));
        if self.loading || self.error.is_some() || self.rows.is_empty() {
            let text = if self.loading {
                tr("正在读取目录…", "Reading folder…")
            } else if self.error.is_some() {
                tr("无法访问此位置", "Unable to access this location")
            } else if self.query.is_empty() {
                tr("此文件夹为空", "This folder is empty")
            } else {
                tr("没有匹配的项目", "No matching items")
            };
            let mut state = div()
                .flex_1()
                .min_h_0()
                .px(px(20.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .text_color(color(&t.text_muted))
                .child(self.icon(if self.query.is_empty() { "folder-open" } else { "search" }, 28.0))
                .child(div().text_size(px(13.0)).text_color(color(&t.text)).child(text));
            if let Some(error) = &self.error {
                state = state
                    .child(div().max_w(px(280.0)).text_size(px(11.0)).child(error.clone()))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(self.button("file-browser-error-parent", tr("返回上一级", "Go up"))
                                .on_click(cx.listener(|this, _, window, cx| this.navigate(parent_path(&this.path), true, window, cx))))
                            .child(self.button("file-browser-error-retry", tr("重试", "Retry"))
                                .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))),
                    );
            } else if !self.query.is_empty() && !self.loading {
                state = state.child(div().text_size(px(11.0)).child(tr(
                    "只筛选当前文件夹的名称，不搜索子文件夹。",
                    "Filters only this folder, without searching subfolders.",
                )));
            }
            body = body.child(state);
        } else {
            if self.detail && !self.path.is_empty() {
                body = body.child(
                    div()
                        .h(px(26.0))
                        .flex_none()
                        .px(px(8.0))
                        .mb(px(2.0))
                        .flex()
                        .items_center()
                        .gap(px(9.0))
                        .border_b_1()
                        .border_color(color(&t.border))
                        .text_size(px(10.5))
                        .text_color(color(&t.text_muted))
                        .child(div().flex_1().min_w_0().pl(px(25.0)).child(tr("名称", "Name")))
                        .child(div().w(px(106.0)).flex_none().child(tr("修改日期", "Modified")))
                        .child(div().w(px(84.0)).flex_none().child(tr("类型", "Type")))
                        .child(div().w(px(60.0)).flex_none().flex().justify_end().child(tr("大小", "Size"))),
                );
            }
            let weak = cx.weak_entity();
            let virtual_list = list(self.list.clone(), move |index, window, cx| {
                weak.update(cx, |this, cx| this.render_row(index, window, cx))
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .flex_1()
            .min_h_0()
            .w_full();
            let weak = cx.weak_entity();
            let changed = Rc::new(move |_: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |_, cx| cx.notify());
            });
            body = body.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .pb(px(4.0))
                    .child(virtual_list)
                    .child(self.scrollbar.render(
                        "file-browser-scrollbar",
                        ScrollbarTarget::List(self.list.clone()),
                        self.epoch,
                        ScrollbarStyle { ink: color(&t.text_muted), metrics: self.tokens.scrollbar, always_visible: false },
                        changed,
                    )),
            );
        }
        let target = self.go_target();
        let go_text = if go_label.is_empty() { tr("打开", "Open").to_string() } else { go_label };
        let footer = div()
            .h(px(40.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(6.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .border_t_1()
            .border_color(color(&t.border))
            .text_size(px(11.0))
            .text_color(color(&t.text_muted))
            .child(div().flex_none().child(if self.loading {
                tr("正在读取…", "Reading…").to_string()
            } else if self.error.is_some() {
                tr("位置不可用", "Location unavailable").to_string()
            } else if self.opening {
                tr("正在打开…", "Opening…").to_string()
            } else {
                format!("{} {}{}", self.item_count(), tr("项", "items"),
                    if self.cursor.is_some() { tr(" · 已选 1 项", " · 1 selected") } else { "" })
            }))
            .child(
                div()
                    .id("file-docked-go")
                    .min_w_0()
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(5.0))
                    .bg(color(&t.accent))
                    .text_color(gpui::white())
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .opacity(if target.is_some() { 1.0 } else { 0.4 })
                    .when(target.is_some(), |button| button.cursor_pointer().hover(|style| style.opacity(0.88)))
                    .child(self.icon("folder-open", 13.0).text_color(gpui::white()))
                    .child(div().min_w_0().truncate().child(match &target {
                        Some(path) => format!("{go_text}「{}」", path_title(path)),
                        None => go_text,
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(path) = this.go_target().filter(|_| this.current() && !this.is_composing(cx)) {
                            cx.emit(FileBrowserEvent::Go(path));
                        }
                    })),
            );
        let has_path = !self.path.is_empty();
        let mut root = div()
            .id("file-browser-root")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .font_family(self.tokens.typography.font_family.clone())
            .text_color(color(&t.text))
            .bg(color(&t.background))
            .capture_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.more_open {
                    this.more_open = false;
                    cx.notify();
                }
            }))
            .child(header)
            .child(body);
        if let Some(feedback) = &self.feedback {
            root = root.child(
                div()
                    .flex_none()
                    .min_h(px(32.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_t_1()
                    .border_color(color(&t.border))
                    .bg(color(&t.tab_active))
                    .text_size(px(11.0))
                    .child(div().flex_1().min_w_0().child(feedback.clone()))
                    .child(self.icon_button("file-browser-dismiss-feedback", "close", true).w(px(22.0)).h(px(22.0))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.feedback = None;
                            cx.notify();
                        }))),
            );
        }
        root = root.child(footer);
        if self.more_open {
            root = root.child(
                div()
                    .id("file-docked-more-menu")
                    .occlude()
                    .absolute()
                    .top(px(36.0))
                    .right(px(34.0))
                    .w(px(176.0))
                    .p(px(4.0))
                    .flex()
                    .flex_col()
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(color(&t.border))
                    .bg(color(&t.background))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(self.more_item("file-docked-detail", "list", tr("详细信息", "Details"), self.detail, true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.detail = !this.detail;
                            this.more_open = false;
                            cx.notify();
                        })))
                    .child(self.more_item("file-docked-copy", "copy", tr("复制路径", "Copy path"), false, has_path)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.more_open = false;
                            if !this.path.is_empty() && this.current() {
                                cx.emit(FileBrowserEvent::CopyPath(this.path.clone()));
                            }
                            cx.notify();
                        })))
                    .child(self.more_item(
                        "file-docked-menu",
                        if in_file_menu { "delete" } else { "star" },
                        if in_file_menu { tr("从文件菜单移除", "Remove from file menu") } else { tr("加入文件菜单", "Add to file menu") },
                        false,
                        has_path,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.more_open = false;
                        if !this.path.is_empty() && this.current() {
                            cx.emit(FileBrowserEvent::ToggleFileMenu(this.path.clone()));
                        }
                        cx.notify();
                    }))),
            );
        }
        root.into_any_element()
    }
}

impl Render for FileBrowserWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        let p = self.tokens.file_browser_proposal;
        let narrow = self.content_width(window) <= p.narrow_breakpoint;
        let columns = if narrow { 1 } else { 2 };
        if self.columns != columns {
            self.columns = columns;
            self.reproject(cx);
        }
        if self.embedded.is_some() {
            return self.render_docked(cx);
        }
        let title = rsx! { <div h={px(p.caption_height)} flex_none flex items_center gap={px(9.0)} pl={px(15.0)} bg={color(&t.surface)} border_b_1 border_color={color(&t.border)} /> }
            .child(self.icon("folder-open", 17.0).text_color(color(&t.accent)))
            .child(div().id("file-browser-caption").flex_1().min_w_0().h_full().flex().items_center().gap(px(15.0)).text_size(px(12.0)).child(tr("文件", "Files")).child(div().text_size(px(11.0)).text_color(color(&t.text_muted)).truncate().child(path_title(&self.path)))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if !this.current() || this.is_composing(cx) { return; }
                    let result = if this.fixture { platform::begin_owned_move_at_client(window, event.position) }
                    else { let owner = this.host.clone(); let generation = this.display;
                        let _guard = platform::guard_condition(move || { let state = owner.file_browser_snapshot(); state.visible && state.generation == generation });
                        platform::begin_host_move_at_client(window, &this.host, "file-browser", event.position)
                    };
                    if let Err(error) = result { this.feedback = Some(error); cx.notify(); }
                    cx.stop_propagation();
                })))
            .child(self.button("file-browser-minimize", "—").w(px(40.0)).h_full().border_0().rounded_none().on_click(|_, window, _| window.minimize_window()))
            .child(self.button("file-browser-maximize", "□").w(px(40.0)).h_full().border_0().rounded_none().on_click(|_, window, _| window.zoom_window()))
            .child(self.icon_button("file-browser-close", "close", true).w(px(40.0)).h_full().rounded_none().on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))));
        let mut filter = div()
            .w(px(if narrow { 158.0 } else { 188.0 }))
            .h(px(33.0))
            .flex_none()
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(color(&t.border))
            .bg(color(&t.search_background))
            .child(self.icon("search", 15.0).text_color(color(&t.text_muted)))
            .child(div().flex_1().min_w_0().child(self.filter.clone()));
        if !self.query.is_empty() {
            filter = filter.child(
                self.icon_button("file-browser-clear", "close", true)
                    .w(px(19.0))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.query.clear();
                        this.filter
                            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                        this.reproject(cx);
                    })),
            );
        }
        let navigation = rsx! { <div h={px(p.navigation_height)} flex_none px={px(11.0)} flex items_center gap={px(if narrow { 6.0 } else { 11.0 })} bg={color(&t.surface)} border_b_1 border_color={color(&t.border)} /> }
            .child(div().flex().flex_none().gap(px(2.0))
                .child(self.icon_button("file-browser-back", "chevron-left", self.history_index > 0).on_click(cx.listener(|this, _, window, cx| this.history_move(true, window, cx))))
                .child(self.icon_button("file-browser-forward", "chevron-right", self.history_index + 1 < self.history.len()).on_click(cx.listener(|this, _, window, cx| this.history_move(false, window, cx))))
                .child(self.icon_button("file-browser-up", "arrow-up", !self.path.is_empty()).on_click(cx.listener(|this, _, window, cx| { if !this.path.is_empty() { this.navigate(parent_path(&this.path), true, window, cx); } }))))
            .child(self.render_address(cx)).child(filter);
        let mut contents = div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .px(px(if narrow { 8.0 } else { 15.0 }))
            .pt(px(if self.path.is_empty() { 13.0 } else { 20.0 }));
        if !self.path.is_empty() {
            contents = contents.child(
                div()
                    .flex_none()
                    .mx(px(7.0))
                    .mb(px(18.0))
                    .flex()
                    .items_center()
                    .gap(px(15.0))
                    .child(div().text_size(px(15.0)).child(path_title(&self.path)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(color(&t.text_muted))
                            .child(self.path.clone()),
                    ),
            );
        }
        if self.loading || self.error.is_some() || self.rows.is_empty() {
            let empty_text = if self.loading {
                tr("正在读取目录…", "Reading folder…")
            } else if self.error.is_some() {
                tr("无法访问此位置", "Unable to access this location")
            } else if self.query.is_empty() {
                tr("此文件夹为空", "This folder is empty")
            } else {
                tr("没有匹配的文件", "No matching files")
            };
            let mut state = div()
                .flex_1()
                .min_h_0()
                .px(px(20.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(11.0))
                .text_color(color(&t.text_muted))
                .child(self.icon(
                    if self.query.is_empty() {
                        "folder-open"
                    } else {
                        "search"
                    },
                    34.0,
                ))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(color(&t.text_secondary))
                        .child(empty_text),
                );
            if let Some(error) = &self.error {
                state = state
                    .child(
                        div()
                            .max_w(px(390.0))
                            .text_size(px(11.0))
                            .child(error.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                self.button("file-browser-error-parent", tr("返回上一级", "Go up"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.navigate(parent_path(&this.path), true, window, cx)
                                    })),
                            )
                            .child(
                                self.button("file-browser-error-retry", tr("重试", "Retry"))
                                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                            ),
                    );
            } else if !self.query.is_empty() && !self.loading {
                state = state.child(div().text_size(px(11.0)).child(tr(
                    "仅筛选当前目录，不搜索子文件夹。",
                    "Filters only this folder, without searching subfolders.",
                )));
            }
            contents = contents.child(state);
        } else {
            if !self.path.is_empty() {
                contents = contents.child(
                    div()
                        .h(px(29.0))
                        .flex_none()
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .border_b_1()
                        .border_color(color(&t.border))
                        .text_size(px(10.0))
                        .text_color(color(&t.text_muted))
                        .child(div().flex_1().min_w_0().child(tr("名称 ↑", "Name ↑")))
                        .child(
                            div()
                                .w(px(if narrow { 112.0 } else { 135.0 }))
                                .flex_none()
                                .child(tr("修改日期", "Modified")),
                        )
                        .when(!narrow, |row| {
                            row.child(div().w(px(91.0)).flex_none().child(tr("类型", "Type")))
                        })
                        .child(
                            div()
                                .w(px(65.0))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .child(tr("大小", "Size")),
                        )
                );
            }
            let weak = cx.weak_entity();
            let virtual_list = list(self.list.clone(), move |index, window, cx| {
                weak.update(cx, |this, cx| this.render_row(index, window, cx))
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .flex_1()
            .min_h_0()
            .w_full();
            let weak = cx.weak_entity();
            let changed = Rc::new(move |_: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |_, cx| cx.notify());
            });
            contents = contents.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .pb(px(8.0))
                    .child(virtual_list)
                    .child(self.scrollbar.render(
                        "file-browser-scrollbar",
                        ScrollbarTarget::List(self.list.clone()),
                        self.epoch,
                        ScrollbarStyle {
                            ink: color(&t.text_muted),
                            metrics: self.tokens.scrollbar,
                            always_visible: false,
                        },
                        changed,
                    )),
            );
        }
        let mut root = div()
            .id("file-browser-root")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .font_family(self.tokens.typography.font_family.clone())
            .text_color(color(&t.text))
            .bg(color(&t.background))
            .capture_key_down(
                cx.listener(|this, event, window, cx| this.key_down(event, window, cx)),
            )
            .when(self.embedded.is_none(), |root| root.child(title))
            .child(navigation)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .child(self.sidebar(narrow, cx))
                    .child(contents),
            );
        if let Some(feedback) = &self.feedback {
            root = root.child(
                div()
                    .flex_none()
                    .min_h(px(35.0))
                    .px(px(14.0))
                    .py(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .border_t_1()
                    .border_color(color(&t.border))
                    .bg(color(&t.tab_active))
                    .text_size(px(11.0))
                    .child(div().flex_1().min_w_0().child(feedback.clone()))
                    .child(
                        self.icon_button("file-browser-dismiss-feedback", "close", true)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.feedback = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        root.child(
            div()
                .h(px(p.footer_height))
                .flex_none()
                .px(px(14.0))
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(color(&t.border))
                .bg(color(&t.surface))
                .text_size(px(10.0))
                .text_color(color(&t.text_muted))
                .child(if self.loading {
                    tr("正在读取…", "Reading…").into()
                } else if self.error.is_some() {
                    tr("位置不可用", "Location unavailable").into()
                } else if self.opening {
                    tr("正在打开…", "Opening…").into()
                } else {
                    format!(
                        "{} {}{}",
                        self.item_count(),
                        tr("个项目", "items"),
                        if self.cursor.is_some() {
                            tr("   已选中 1 项", "   1 selected")
                        } else {
                            ""
                        }
                    )
                })
                .child(if self.path.is_empty() {
                    tr("单击位置进入", "Click a location to browse")
                } else {
                    tr(
                        "双击打开 · Backspace 上一级",
                        "Double-click to open · Backspace Go up",
                    )
                }),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_browser_global_io_slot_stays_claimed_until_blocking_worker_exits() {
        static TEST_IO: AtomicUsize = AtomicUsize::new(0);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = BrowserIoPermit::try_acquire(&TEST_IO, 1).unwrap();
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        // A replacement panel cannot start another actual OS read while its
        // predecessor's detached/blocking operation still owns this slot.
        assert!(BrowserIoPermit::try_acquire(&TEST_IO, 1).is_none());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(BrowserIoPermit::try_acquire(&TEST_IO, 1).is_some());
        assert_eq!(TEST_IO.load(Ordering::Acquire), 0);
    }

    #[test]
    fn file_browser_up_respects_drive_and_unc_roots() {
        assert_eq!(parent_path(r"C:\Users\demo\文档"), r"C:\Users\demo");
        assert_eq!(parent_path(r"C:\Users"), "C:\\");
        assert_eq!(parent_path("C:\\"), "");
        assert_eq!(parent_path(r"\\server\share\资料"), r"\\server\share");
        assert_eq!(parent_path(r"\\server\share"), "");
        assert_eq!(parent_path(""), "");
    }

    #[test]
    fn file_browser_capacity_handles_zero_and_large_units() {
        assert_eq!(display_size(0), "0 B");
        assert_eq!(display_size(199 * 1024_u64.pow(3)), "199 GB");
        assert_eq!(display_size((1.81 * 1024_f64.powi(4)) as u64), "1.81 TB");
        assert_eq!(path_title("C:\\"), "C:");
        assert_eq!(path_title(r"D:\项目\设计资料"), "设计资料");
    }
}
