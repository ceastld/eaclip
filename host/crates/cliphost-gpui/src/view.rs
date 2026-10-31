use gpui_rsx::rsx;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(windows)]
use crate::{
    hover::{row_hover_content, RowHoverInput, RowHoverSource},
    native_interactions::{DropDestination, NativeInteractionEvent, NativeInteractions},
};
use cliphost_core::{
    host::{ChromeCommand, ChromeTask, HostState},
    i18n::{self, tr},
};
use gpui::{
    actions, div, img, list, prelude::*, px, relative, rgb, Context, Entity, FontWeight, Hsla,
    KeyBinding, ListAlignment, ListState, MouseButton, SharedString, Subscription, Task, Timer,
    Window,
};
use crate::crisp_svg::svg;
use crate::smooth_wheel::SmoothWheelExt;
use serde_json::{json, Value};
#[cfg(windows)]
pub(crate) type SharedInteractions = Rc<std::cell::RefCell<Option<NativeInteractions>>>;
pub(crate) fn next_native_session() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
#[path = "list_preferences.rs"]
mod list_preferences;
use list_preferences::{preference_tooltip, preset_writer, AppearanceWrites, PresetSnapshot};

#[path = "list_settings.rs"]
mod list_settings;
#[path = "filter_popover.rs"]
mod filter_popover;

#[path = "hover_binding.rs"]
pub(crate) mod hover_binding;

#[path = "history_favorite_save.rs"]
mod history_favorite_save;

#[path = "history_context_menu.rs"]
mod history_context_menu;

#[path = "native_maintenance.rs"]
mod native_maintenance;

#[cfg(debug_assertions)]
#[path = "qa_content_harness.rs"]
mod qa_content_harness;

use crate::{
    ceabox::CeaBoxWindow,
    reopen::ReopenWindow,
    file_browser::FileBrowserWindow,
    appearance::{
        self, render_hover_actions, render_minimal_trail, render_row_header, row_chrome_with_wallpaper,
        AppearanceControls, AppearanceEvent, AppearanceSettings, HoverCommand, HoverInvocation,
        RowLayout, RowMetadata,
    },
    editor::{EditorTarget, EditorWindow},
    favorites::{FavoritesEvent, NativeFavoritesView},
    fifo_panel::{FifoPanel, FifoPanelEvent},
    filters::{
        build_tabs, AdvancedFilter, CategoryTab, FilterTabsEditor, FilterTabsEvent, ListRequest,
    },
    history_actions::{ClearScope, HistoryActions, HistoryActionsEvent},
    input::{InputColors, SearchEvent, SearchInput},
    lan_companion::LanCompanionWindow,
    management::{data_settings_route, ManagementWindow},
    menu::{ContextMenuPopup, HistorySelectHandler, MenuAction, MenuEntry, MenuSelection, SelectHandler},
    native_content::{
        self,
        model::{project_history_item_with_text, ContentMode, ContentProjection},
        ContentMetrics, ContentOpenRequest, ContentResourceCache, ContentResources,
    },
    native_host::NativeWindow,
    overlay::QuickPasteOverlay,
    panel::PanelWindow,
    platform,
    preview::{DesignTokens, PreviewModel, PreviewPage, PreviewRow, ThemeTokens},
    process_filters::{
        load_source_icon, preset_tabs, source_icon_element, source_icon_pixels, SourceIcon, render_preset_tab, ProcessFilterEditor, ProcessFiltersEvent,
        ProcessPreset, ProcessSelection,
    },
    service::NativeHost,
    scrollbar::{self, ScrollbarState, ScrollbarStyle, ScrollbarTarget},
    settings::{settings_page_from_route, SettingsForm, SettingsWindow},
    super_menu::SuperMenuWindow,
    toast::CopyToastOverlay,
    file_line::FileLine,
};

/// Clip settings surfaced in the footer popover, in the web panel's order.
const LIST_SETTINGS_KEYS: &[&str] = &[
    "ClipListSeparatePinnedUi",
    "ClipListImageHoverPreviewEnabled",
    "ClipListImagePreviewMaxHeightPx",
    "EnableNoFocusMode",
    "DismissBehavior",
    "ClipListExecuteMode",
    "AutoScrollToTop",
    "AutoScrollToTopIdleSeconds",
];

#[derive(Clone, Copy)]
enum HistoryDialog {
    AddText,
    Clear(Option<ClearScope>),
}

fn normalize_main_settings_route(route: String) -> String {
    if let Some(route) = data_settings_route(&route) { return route.into(); }
    match route.split(['?', '#']).next().unwrap_or(&route).trim_end_matches('/') {
        "/main/settings/mobileSync" => "/main/settings/global".into(),
        "/main/settings/explorer" => "/main/settings/user".into(),
        _ => route,
    }
}

actions!(
    clip_preview,
    [
        FocusSearch,
        Next,
        Previous,
        First,
        Last,
        RangeNext,
        RangePrevious,
        RangeFirst,
        RangeLast,
        Close,
        Paste,
        Copy,
        Delete,
        Pin
    ]
);

#[derive(Clone)]
pub enum UiModel {
    Fixture(PreviewModel),
    Live(Arc<NativeHost>),
}

impl UiModel {
    fn query(
        &self,
        request: &ListRequest,
        query: &str,
        skip: usize,
        take: usize,
    ) -> Result<PreviewPage, String> {
        match self {
            Self::Fixture(model) => Ok(model.query(request.fixture_category(), query, skip, take)),
            Self::Live(host) => crate::history_page::query_loaded_range(skip, take, |skip, take| {
                host.search_request(request, query, skip, take)
            }),
        }
    }

    fn host(&self) -> Option<Arc<HostState>> {
        match self {
            Self::Live(host) => Some(host.state()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct RenderStats {
    view_renders: usize,
    rows_built: usize,
    row_ids: HashSet<String>,
    first_view_ms: Option<f64>,
}

enum ActionNotice {
    LanQueue(usize),
}

impl ActionNotice {
    fn text(self) -> String {
        match self {
            Self::LanQueue(1) => tr("已加入局域网发送队列", "Added to LAN send queue").into(),
            Self::LanQueue(count) => cliphost_core::trf!(
                "已将 {count} 项加入局域网发送队列",
                "Added {count} items to LAN send queue"
            ),
        }
    }
}

/// The same virtualized renderer serves isolated design fixtures and the real core.
pub struct ClipboardPreview {
    #[cfg(debug_assertions)]
    qa_harness: Option<qa_content_harness::Harness>,
    window: gpui::AnyWindowHandle,
    tokens: DesignTokens,
    base_tokens: DesignTokens,
    theme_cache: crate::custom_theme::ResolvedThemeCache,
    model: UiModel,
    page: PreviewPage,
    committed_query: Option<(String, ListRequest)>,
    dark: bool,
    category: String,
    query: String,
    /// Active row: keyboard cursor and single-item execution target.
    selected_id: String,
    /// Ordered multi-selection (Ctrl/Shift click); always contains the active row.
    selected_ids: Vec<String>,
    anchor: Option<usize>,
    search: Entity<SearchInput>,
    keyboard_paste_active: bool,
    keyboard_paste_reason: Option<String>,
    keyboard_paste_visible: bool,
    keyboard_paste_amount: f32,
    keyboard_paste_motion: crate::motion::Tweens<()>,
    list: ListState,
    history_scrollbar: ScrollbarState,
    history_idle: crate::history_idle::HistoryIdle,
    history_away_from_top: bool,
    history_top_hovered: bool,
    history_top_motion: crate::motion::Tweens<()>,
    focus: gpui::FocusHandle,
    no_focus: bool,
    snapshot: bool,
    generation: u64,
    page_generation: u64,
    pending: Option<Task<()>>,
    page_pending: Option<Task<()>>,
    native: Option<NativeWindow>,
    #[cfg(windows)]
    interactions: SharedInteractions,
    native_session: u64,
    #[cfg(windows)]
    native_stamp: Option<(isize, u64, u64, bool)>,
    #[cfg(windows)]
    native_hover_tickets: hover_binding::HoverBindings,
    native_drag_over: Option<u64>,
    native_dragging: bool,
    native_suppress_click: Option<Instant>,
    history_content_drag: Option<crate::favorite_drag::ContentDrag>,
    drag_tab_hover: Option<Instant>,
    maintenance: Option<Task<()>>,
    window_state: Value,
    configuration: Value,
    window_background: crate::window_background::BackgroundState,
    window_background_task: Option<Task<()>>,
    quick_paste: Option<gpui::WindowHandle<QuickPasteOverlay>>,
    menu: Option<gpui::WindowHandle<ContextMenuPopup>>,
    favorite_popup: Option<gpui::WindowHandle<crate::favorite_save::FavoriteSavePopup>>,
    favorite_lease: Option<u64>,
    favorite_trigger_id: Option<String>,
    favorite_hover_task: Option<Task<()>>,
    favorite_hover_pending: Option<(String, u64)>,
    favorite_hover_epoch: u64,
    favorite_root_saving: bool,
    favorite_save_undo: Option<String>,
    favorite_feedback: Option<history_favorite_save::FavoriteFeedback>,
    favorite_feedback_generation: u64,
    toast: Option<gpui::WindowHandle<CopyToastOverlay>>,
    settings_window: Option<gpui::WindowHandle<SettingsWindow>>,
    management_window: Option<gpui::WindowHandle<ManagementWindow>>,
    lan_companion: Option<gpui::WindowHandle<LanCompanionWindow>>,
    lan_companion_open: bool,
    editor_window: Option<gpui::WindowHandle<EditorWindow>>,
    editor_followed_selection: Option<String>,
    exit_pending: bool,
    main_transition_pending: bool,
    pending_main_route: Option<(Option<String>, ChromeTask)>,
    super_menu: Option<gpui::WindowHandle<SuperMenuWindow>>,
    panel_window: Option<gpui::WindowHandle<PanelWindow>>,
    ceabox_window: Option<gpui::WindowHandle<CeaBoxWindow>>,
    reopen_window: Option<gpui::WindowHandle<ReopenWindow>>,
    /// 2026-10-28 top-edge file line; idle until its setting is on.
    file_line: Option<Entity<FileLine>>,
    file_browser_window: Option<gpui::WindowHandle<FileBrowserWindow>>,
    favorites: Option<Entity<NativeFavoritesView>>,
    favorites_active: bool,
    favorites_visible: bool,
    favorites_subscription: Option<Subscription>,
    fifo_panel: Option<(Entity<FifoPanel>, Subscription, u64)>,
    history_actions: Option<Entity<HistoryActions>>,
    history_dialog: bool,
    history_visible: bool,
    list_settings: Option<Entity<SettingsForm>>,
    list_settings_appearance: bool,
    list_settings_generation: u64,
    /// Enter animation key for the category-row filter popovers.
    filter_popover_generation: u64,
    appearance_controls: Option<(Entity<AppearanceControls>, Subscription)>,
    appearance_writes: Arc<Mutex<AppearanceWrites>>,
    process_presets: Vec<ProcessPreset>,
    process_selection: ProcessSelection,
    process_initialized: bool,
    process_loaded: bool,
    process_loading: bool,
    process_open_pending: bool,
    /// Preset to edit once the pending load opens the popover.
    process_edit_target: Option<String>,
    process_reload_requested: bool,
    process_icon_applied: HashSet<String>,
    process_presentation: Option<(isize, u64)>,
    process_generation: u64,
    process_editor: Option<(Entity<ProcessFilterEditor>, Subscription)>,
    process_write_gate: Arc<Mutex<()>>,
    source_icons: HashMap<String, Option<SourceIcon>>,
    /// Device pixels the cached source icons were extracted at.
    source_icon_pixels: u32,
    content_cache: ContentResourceCache,
    /// Display height the cached history rasters were prepared for.
    content_image_height: u32,
    /// Rows whose completed resources may change their height, re-measured
    /// together with other completions on the next frame.
    content_dirty_rows: HashSet<String>,
    content_flush_pending: bool,
    source_icon_pending: HashSet<String>,
    source_icon_order: VecDeque<String>,
    /// Category bar from `ClipFilterConfig`; `category` is the selected key.
    tabs: Vec<CategoryTab>,
    advanced: AdvancedFilter,
    /// Open search-box filter popover with its (start, end, single) date fields.
    filter_panel: Option<([Entity<SearchInput>; 3], Vec<Subscription>)>,
    tag_editor: Option<(Entity<FilterTabsEditor>, Subscription)>,
    /// 「⋯」 category overflow and management menu.
    more_menu: bool,
    /// Core lease id and the ids it was opened for.
    menu_lease: Option<(u64, Vec<String>)>,
    menu_anchor: Option<(i32, i32)>,
    pin_overlay: (bool, bool),
    history_removals: crate::history_removal::RemovalTracker,
    history_delete_pending: HashSet<String>,
    history_refill_target: Option<usize>,
    history_delete_range_valid: bool,
    loading: bool,
    background_refresh: bool,
    error: Option<String>,
    notice: Option<String>,
    notice_task: Option<Task<()>>,
    action_reply_sequence: u64,
    stats: Arc<Mutex<RenderStats>>,
    stats_enabled: bool,
    started: Instant,
    last_query_ms: f64,
    language: i18n::LanguageMode,
    _subscriptions: Vec<Subscription>,
}

fn caption_exclusion() -> impl IntoElement {
    gpui::canvas(
        |bounds, window, _| platform::record_caption_exclusion(window, bounds),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

pub(crate) fn color(hex: &str) -> Hsla {
    let raw = hex.trim_start_matches('#');
    let value = u32::from_str_radix(raw, 16).expect("validated theme color");
    if raw.len() == 8 {
        gpui::rgba(value).into()
    } else {
        rgb(value).into()
    }
}

pub(crate) fn label_width(text: &str, size: f32, family: &str, window: &Window) -> f32 {
    let text: SharedString = text.to_owned().into();
    let run = gpui::TextRun {
        len: text.len(),
        font: gpui::font(family.to_owned()),
        color: gpui::white(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    f32::from(
        window
            .text_system()
            .shape_line(text, px(size), &[run], None)
            .width,
    )
}

impl ClipboardPreview {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tokens: DesignTokens,
        model: UiModel,
        dark: bool,
        category: String,
        query: String,
        no_focus: bool,
        snapshot: bool,
        started: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(host) = model.host() {
            host.set_native_history_shortcuts_enabled(true);
            host.set_clipboard_search_active(!query.is_empty());
        }
        let search = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            let t = tokens.theme(dark);
            // The search field draws its own fill; the input stays transparent.
            input.set_colors(
                InputColors {
                    background: gpui::transparent_black(),
                    foreground: color(&t.text),
                    placeholder: color(&t.text_muted).opacity(0.6),
                    border: gpui::transparent_black(),
                    selection: color(&t.tab_active),
                    caret: color(&t.accent),
                },
                cx,
            );
            input.set_metrics(
                tokens.layout.search_height - 2.0,
                tokens.typography.category_font_size,
                0.0,
                cx,
            );
            input.set_placeholder("", cx);
            input.set_text(query.clone(), window, cx);
            input
        });
        cx.bind_keys([
            KeyBinding::new("ctrl-f", FocusSearch, Some("ClipPreview")),
            KeyBinding::new("down", Next, Some("ClipPreview")),
            KeyBinding::new("up", Previous, Some("ClipPreview")),
            KeyBinding::new("home", First, Some("ClipPreview")),
            KeyBinding::new("end", Last, Some("ClipPreview")),
            KeyBinding::new("shift-down", RangeNext, Some("ClipPreview && !ClipSearchInput")),
            KeyBinding::new("shift-up", RangePrevious, Some("ClipPreview && !ClipSearchInput")),
            KeyBinding::new("shift-home", RangeFirst, Some("ClipPreview && !ClipSearchInput")),
            KeyBinding::new("shift-end", RangeLast, Some("ClipPreview && !ClipSearchInput")),
            KeyBinding::new("ctrl-c", Copy, Some("ClipPreview && !ClipSearchInput")),
            KeyBinding::new("delete", Delete, Some("ClipPreview && !ClipSearchInput")),
            KeyBinding::new("ctrl-p", Pin, Some("ClipPreview && !ClipSearchInput")),
        ]);
        let subscribed =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(text) => {
                    this.query = text.clone();
                    this.refresh(cx);
                }
                SearchEvent::Submit => {
                    this.focus.focus(window);
                    this.restore_no_focus(window);
                    this.execute_selected("paste", cx);
                }
                SearchEvent::Escape => {
                    if this.model.host().is_some() {
                        this.dismiss(window);
                        return;
                    }
                    this.query.clear();
                    this.search
                        .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                    this.refresh(cx);
                    this.focus.focus(window);
                    this.restore_no_focus(window);
                }
            });
        let page = match &model {
            UiModel::Fixture(_) => model
                .query(&ListRequest::category(&category), &query, 0, 1_000)
                .expect("valid fixture"),
            UiModel::Live(_) => PreviewPage {
                rows: vec![],
                total: 0,
                skip: 0,
                take: 100,
                next_skip: 0,
                has_more: false,
            },
        };
        let fixture_selection = match &model {
            UiModel::Fixture(m) => m.fixture().selected_id.as_str(),
            _ => "",
        };
        let selected_id = page
            .rows
            .iter()
            .find(|row| row.item.id == fixture_selection)
            .or_else(|| page.rows.first())
            .map(|row| row.item.id.clone())
            .unwrap_or_default();
        let focus = cx.focus_handle();
        focus.focus(window);
        let list = ListState::new(page.rows.len(), ListAlignment::Top, px(150.0));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event, _, cx| {
            let end = event.visible_range.end;
            let _ = weak.update(cx, |this, cx| {
                this.history_idle.activity(Instant::now());
                // GPUI invokes this while ListState is mutably borrowed.
                // Read the final offset during render, after that borrow ends.
                if end.saturating_add(20) >= this.page.rows.len() {
                    this.load_more(cx);
                }
            });
        });
        let mut result = Self {
            window: window.window_handle(),
            base_tokens: tokens.clone(),
            theme_cache: crate::custom_theme::ResolvedThemeCache::default(),
            tokens,
            stats_enabled: matches!(model, UiModel::Fixture(_)),
            model,
            list,
            history_scrollbar: ScrollbarState::default(),
            history_idle: crate::history_idle::HistoryIdle::default(),
            history_away_from_top: false,
            history_top_hovered: false,
            history_top_motion: crate::motion::Tweens::default(),
            page,
            committed_query: None,
            dark,
            category,
            query,
            selected_ids: if selected_id.is_empty() {
                vec![]
            } else {
                vec![selected_id.clone()]
            },
            anchor: None,
            selected_id,
            search,
            keyboard_paste_active: false,
            keyboard_paste_reason: None,
            keyboard_paste_visible: false,
            keyboard_paste_amount: 0.0,
            keyboard_paste_motion: crate::motion::Tweens::default(),
            focus,
            no_focus,
            snapshot,
            generation: 0,
            page_generation: 0,
            pending: None,
            page_pending: None,
            native: None,
            #[cfg(windows)]
            interactions: Rc::new(std::cell::RefCell::new(None)),
            native_session: next_native_session(),
            #[cfg(windows)]
            native_stamp: None,
            #[cfg(windows)]
            native_hover_tickets: hover_binding::HoverBindings::default(),
            native_drag_over: None,
            native_dragging: false,
            native_suppress_click: None,
            history_content_drag: None,
            drag_tab_hover: None,
            maintenance: None,
            window_state: Value::Null,
            configuration: Value::Null,
            window_background: crate::window_background::BackgroundState::default(),
            window_background_task: None,
            quick_paste: None,
            menu: None,
            favorite_popup: None,
            favorite_lease: None,
            favorite_trigger_id: None,
            favorite_hover_task: None,
            favorite_hover_pending: None,
            favorite_hover_epoch: 0,
            favorite_root_saving: false,
            favorite_save_undo: None,
            favorite_feedback: None,
            favorite_feedback_generation: 0,
            toast: None,
            settings_window: None,
            management_window: None,
            lan_companion: None,
            lan_companion_open: false,
            editor_window: None,
            editor_followed_selection: None,
            #[cfg(debug_assertions)]
            qa_harness: None,
            exit_pending: false,
            main_transition_pending: false,
            pending_main_route: None,
            super_menu: None,
            panel_window: None,
            ceabox_window: None,
            reopen_window: None,
            file_line: None,
            file_browser_window: None,
            favorites: None,
            favorites_active: false,
            favorites_visible: false,
            favorites_subscription: None,
            fifo_panel: None,
            history_actions: None,
            history_dialog: false,
            history_visible: false,
            list_settings: None,
            list_settings_appearance: true,
            list_settings_generation: 0,
            filter_popover_generation: 0,
            appearance_controls: None,
            appearance_writes: Arc::new(Mutex::new(AppearanceWrites::default())),
            process_presets: Vec::new(),
            process_selection: ProcessSelection::default(),
            process_initialized: false,
            process_loaded: false,
            process_loading: false,
            process_open_pending: false,
            process_edit_target: None,
            process_reload_requested: false,
            process_icon_applied: HashSet::new(),
            process_presentation: None,
            process_generation: 0,
            process_editor: None,
            process_write_gate: Arc::new(Mutex::new(())),
            source_icons: HashMap::new(),
            source_icon_pixels: 0,
            content_cache: ContentResourceCache::default(),
            content_image_height: 0,
            content_dirty_rows: HashSet::new(),
            content_flush_pending: false,
            source_icon_pending: HashSet::new(),
            source_icon_order: VecDeque::new(),
            tabs: build_tabs(&Value::Null, true),
            advanced: AdvancedFilter::default(),
            filter_panel: None,
            more_menu: false,
            tag_editor: None,
            menu_lease: None,
            menu_anchor: None,
            pin_overlay: (false, false),
            history_removals: crate::history_removal::RemovalTracker::default(),
            history_delete_pending: HashSet::new(),
            history_refill_target: None,
            history_delete_range_valid: false,
            loading: false,
            background_refresh: false,
            error: None,
            notice: None,
            notice_task: None,
            action_reply_sequence: 0,
            stats: Arc::new(Mutex::new(RenderStats::default())),
            started,
            last_query_ms: 0.0,
            language: i18n::displayed_language(),
            _subscriptions: vec![subscribed],
        };
        if let Some(host) = result.model.host() {
            match NativeWindow::attach(window, &host) {
                Ok(native) => result.native = Some(native),
                Err(error) => result.error = Some(error),
            }
            // Attach restores the core's remembered expanded dimensions. The
            // design fixture size must not overwrite the live window afterward.
            result.window_state = host.window_state_dto();
            result.configuration = host.config_get();
            result.sync_window_background(cx);
            result.load_process_presets(false, window, cx);
            let actions = cx.new(|cx| {
                HistoryActions::new(host.clone(), result.tokens.clone(), dark, window, cx)
            });
            result._subscriptions.push(cx.subscribe_in(
                &actions,
                window,
                |this, _, event, window, cx| {
                    match event {
                        HistoryActionsEvent::Changed => this.refresh(cx),
                        HistoryActionsEvent::Error(error) => this.error = Some(error.clone()),
                        HistoryActionsEvent::DialogChanged(open) => {
                            this.history_dialog = *open;
                            this.cancel_native_hover();
                            this.sync_overlay_guard();
                            this.sync_native_interactions(cx);
                            if !open && !this.favorites_active {
                                this.focus.focus(window);
                                this.restore_no_focus(window);
                            }
                        }
                    }
                    cx.notify();
                },
            ));
            result.history_actions = Some(actions);
            result.sync_native_interactions(cx);
            result.sync_tabs();
            result.refresh_pin_overlay(cx);
            result.start_live(window, cx);
            result.refresh(cx);
        }
        result
            ._subscriptions
            .push(cx.observe_window_bounds(window, |this, _, cx| {
                this.cancel_native_hover();
                this.close_fifo(cx);
            }));
        result
            ._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.close_fifo(cx);
                }
            }));
        result
    }

    fn restore_no_focus(&mut self, window: &Window) {
        let result = if let Some(host) = self.model.host() {
            NativeWindow::restore_browsing(window, &host)
        } else {
            platform::set_no_activate(window, self.no_focus)
        };
        if let Err(error) = result {
            self.error = Some(error);
        }
    }

    fn cancel_native_hover(&mut self) {
        #[cfg(windows)]
        {
            if let Some(native) = self.interactions.borrow().as_ref() {
                native.cancel_hover();
            }
            self.native_hover_tickets.clear();
        }
    }

    fn sync_history_top_button(&mut self, cx: &mut Context<Self>) {
        let offset = self.list.logical_scroll_top();
        let away = offset.item_ix > 0 || offset.offset_in_item > px(4.0);
        if self.history_away_from_top != away {
            self.history_away_from_top = away;
            if !away {
                self.history_top_hovered = false;
                self.history_top_motion.clear();
            }
            cx.notify();
        }
    }

    /// Both explicit and automatic returns discard an old execution target,
    /// while retaining the current query and filters.
    fn return_history_to_top(&mut self, cx: &mut Context<Self>) {
        self.history_scrollbar.cancel();
        self.cancel_native_hover();
        self.selected_id.clear();
        self.selected_ids.clear();
        self.anchor = None;
        self.list.scroll_to(gpui::ListOffset { item_ix: 0, offset_in_item: px(0.0) });
        self.sync_history_top_button(cx);
        cx.notify();
    }

    fn sync_native_interactions(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        {
            let UiModel::Live(service) = &self.model else {
                return;
            };
            let host = service.state();
            if cliphost_core::is_headless() || host.host_hwnd() == 0 {
                return;
            }
            if self.file_line.is_none() {
                self.file_line = Some(FileLine::start(host.clone(), self.tokens.clone(), self.dark, cx));
            }
            if self.interactions.borrow().is_none() {
                match NativeInteractions::attach(
                    service.clone(),
                    host.host_hwnd(),
                    self.native_session,
                    DropDestination {
                        label: "clipboard",
                        parent_id: None,
                        insert_index: -1,
                    },
                ) {
                    Ok(native) => *self.interactions.borrow_mut() = Some(native),
                    Err(error) => {
                        if self.error.as_ref() != Some(&error) {
                            self.error = Some(error);
                            cx.notify();
                        }
                        return;
                    }
                }
            }
            let enabled = !self.exit_pending
                && self.list_settings.is_none()
                && self.filter_panel.is_none()
                && self.tag_editor.is_none()
                && self.process_editor.is_none()
                && !self.process_open_pending
                && !self.history_dialog
                && self.fifo_panel.is_none();
            if self.favorites_active {
                self.history_scrollbar.cancel();
                if let Some(favorites) = &self.favorites {
                    favorites.update(cx, |favorites, cx| {
                        favorites.maintain_native_interactions(enabled, cx)
                    });
                }
                return;
            }
            let stamp = host.native_clipboard_presentation_stamp();
            if self.native_stamp != Some(stamp) {
                self.history_scrollbar.cancel();
                self.native_stamp = Some(stamp);
                self.native_session = next_native_session();
                self.native_hover_tickets.clear();
                self.native_drag_over = None;
                self.native_dragging = false;
                cx.notify();
            }
            let state = host.hide_snapshot();
            let accepting =
                enabled && state.visible && !state.is_collapsed && self.menu_lease.is_none() && self.favorite_lease.is_none();
            if !accepting {
                self.history_scrollbar.cancel();
            }
            if let Some(native) = self.interactions.borrow_mut().as_mut() {
                if let Err(error) = native.sync_session(
                    self.native_session,
                    accepting,
                    DropDestination {
                        label: "clipboard",
                        parent_id: None,
                        insert_index: -1,
                    },
                ) {
                    if self.error.as_ref() != Some(&error) {
                        self.error = Some(error);
                        cx.notify();
                    }
                }
                let _ = native.set_menu_open(self.menu_lease.is_some() || self.favorite_lease.is_some());
            }
        }
        #[cfg(not(windows))]
        let _ = cx;
    }

    fn drain_native_interactions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(windows)]
        {
            let events = self
                .interactions
                .borrow_mut()
                .as_mut()
                .map(NativeInteractions::drain)
                .unwrap_or_default();
            let Some(host) = self.model.host() else {
                return;
            };
            for event in events {
                if self.favorites_active {
                    if let Some(favorites) = &self.favorites {
                        favorites.update(cx, |favorites, cx| {
                            favorites.native_interaction_event(event, window, cx)
                        });
                    }
                    continue;
                }
                match event {
                    NativeInteractionEvent::ContentSaved { result, .. } => {
                        match result {Ok(_)=>self.refresh(cx),Err(error)=>self.error=Some(error)}
                    }
                    NativeInteractionEvent::DropSaved { request, result } => {
                        if request.page_session() != self.native_session
                            || !host.native_drop_request_is_current(&request)
                        {
                            continue;
                        }
                        self.native_drag_over = None;
                        match result {
                            Ok(_) => self.refresh(cx),
                            Err(error) => self.error = Some(error),
                        }
                    }
                    NativeInteractionEvent::DragOver { ticket, over } => {
                        if over && ticket.is_current() && ticket.session() == self.native_session {
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
                        self.native_suppress_click =
                            Some(Instant::now() + Duration::from_millis(200));
                        if let Err(error) = result {
                            self.error = Some(error);
                        }
                    }
                    NativeInteractionEvent::Error(error) => self.error = Some(error),
                }
                cx.notify();
            }
        }
        #[cfg(not(windows))]
        let _ = (window, cx);
    }

    fn start_history_drag(&mut self, drag: &crate::favorite_drag::ContentDrag, window: &mut Window, cx: &mut Context<Self>) {
        if self.favorites_active
            || self.exit_pending
            || self.loading
            || self.history_dialog
            || drag.generation != self.page_generation
            || self.page_generation != self.generation
            || drag.session != self.native_session
            || drag.item_ids.is_empty()
            || drag
                .item_ids
                .iter()
                .any(|id| !self.page.rows.iter().any(|row| &row.item.id == id))
        {
            return;
        }
        self.cancel_native_hover();
        crate::favorite_drag::activate_drag(drag,window,cx);
        self.history_content_drag=Some(drag.clone());
        self.native_suppress_click=Some(Instant::now()+Duration::from_millis(300));
        if let Some(host)=self.model.host() {
            let drag=drag.clone();
            cx.spawn(async move |this,cx| {
                let result=cx.background_executor().spawn(async move {host.favorite_drag_revision()}).await;
                match result {Ok(revision)=>drag.set_revision(revision),Err(error)=>{let _=this.update(cx,|this,cx| {this.error=Some(error);cx.notify();});}}
            }).detach();
        }
        cx.notify();
    }

    #[cfg(windows)]
    fn history_hover_binding(
        &self,
        index: usize,
        id: &str,
        generation: u64,
        session: u64,
    ) -> Option<hover_binding::HoverBinding> {
        if self.model.host().is_none() || cliphost_core::is_headless() {
            return None;
        }
        let row = self.page.rows.get(index).filter(|row| row.item.id == id)?;
        let plan = row_hover_content(
            RowHoverSource::History,
            RowHoverInput {
                id,
                kind: &row.item.kind,
                title: &row.item.title,
                summary: &row.item.summary,
                display_text: &row.item.display_text,
                file_paths: &row.item.file_paths,
                im_rich_segments: &row.item.im_rich_segments,
            },
        );
        Some(self.native_hover_tickets.row(
            id,
            index,
            generation,
            session,
            plan,
            self.configuration["clipListImageHoverPreviewEnabled"].as_bool() != Some(false),
        ))
    }

    fn hover_history(
        &mut self,
        index: usize,
        id: &str,
        generation: u64,
        session: u64,
        event: hover_binding::HoverEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(windows)]
        {
            if self.favorites_active
                || self.exit_pending
                || self.history_dialog
                || self.list_settings.is_some()
                || self.filter_panel.is_some()
                || self.more_menu
                || self.tag_editor.is_some()
                || self.process_editor.is_some()
                || self.process_open_pending
                || self.fifo_panel.is_some()
                || self.menu_lease.is_some()
                || self.favorite_lease.is_some()
                || self.loading
                || generation != self.page_generation
                || self.generation != generation
                || session != self.native_session
                || self.native_dragging
                || !self
                    .page
                    .rows
                    .get(index)
                    .is_some_and(|row| row.item.id == id)
            {
                event.reject();
                return;
            }
            let owner = self
                .interactions
                .borrow()
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
        let _ = (index, id, generation, session, event, window, cx);
    }

    fn set_clipboard_route(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        let active = route
            .split(['?', '#'])
            .next()
            .unwrap_or(route)
            .starts_with("/favorites");
        if active != self.favorites_active {
            self.history_scrollbar.cancel();
            self.cancel_native_hover();
            self.native_session = next_native_session();
            #[cfg(windows)]
            if let Some(native) = self.interactions.borrow_mut().as_mut() {
                let _ = native.sync_session(
                    self.native_session,
                    false,
                    DropDestination {
                        label: "clipboard",
                        parent_id: None,
                        insert_index: -1,
                    },
                );
            }
            self.close_process_editor(window, cx);
            self.close_fifo(cx);
            self.close_menu(cx);
            self.list_settings = None;
            self.filter_panel = None;
            self.tag_editor = None;
            if let Some(actions) = &self.history_actions {
                actions.update(cx, |actions, cx| {
                    if active {
                        actions.suspend(window, cx);
                    } else {
                        actions.resume(cx);
                    }
                });
            }
            // Retire accepted list input from the old page before changing its
            // owner. Closing the transient guard never restores that epoch.
            host.apply_window_action("setSpaOverlayOpen", true);
            host.apply_window_action("setSpaOverlayOpen", false);
            window.blur();
        }
        self.favorites_active = active;
        host.chrome_web_action(&json!({"kind":"clipboardHostChrome", "route":route,
            "sourceHostWindow":"clipboard", "isClipboardEditing":crate::editor::has_active_input_windows()}));
        if active {
            if self.favorites.is_none() {
                let (tokens, dark) = (self.clipboard_tokens(), self.dark);
                let entity =
                    cx.new(|cx| NativeFavoritesView::new(host.clone(), tokens, dark, window, cx));
                let wakeup = host.chrome_command_notifier();
                self._subscriptions.push(cx.observe(&entity, move |this, _, cx| {
                    if this.maintenance_delay(cx).is_none_or(|delay| delay > Duration::from_millis(50)) {
                        wakeup.notify_one();
                    }
                }));
                let subscription = cx.subscribe_in(&entity, window, |this, _, event, window, cx| {
                    match event {
                        FavoritesEvent::Edit(id) => this.open_editor(id.clone(), true, cx),
                        FavoritesEvent::OpenSettings => this.open_settings_route("/main/settings", cx),
                        FavoritesEvent::SwitchPrimaryTab => this.set_clipboard_route(
                            crate::history_keyboard::primary_tab_route(true), window, cx,
                        ),
                        FavoritesEvent::OpenListSettings => {
                            let root = this.window;
                            let owner = cx.weak_entity();
                            cx.defer(move |cx| {
                                let _ = root.update(cx, |_, window, cx| {
                                    let _ = owner.update(cx, |this, cx| {
                                        this.toggle_list_settings(window, cx)
                                    });
                                });
                            });
                        }
                        FavoritesEvent::OpenPanel(id) => {
                            let id = id.clone();
                            this.run_host(false, None, cx, move |host| {
                                host.panel_show(&json!({"favorId":id}))
                            });
                        }
                    }
                    cx.notify();
                });
                #[cfg(windows)]
                entity.update(cx, |favorites, cx| {
                    favorites.bind_native_interactions(self.interactions.clone(), cx)
                });
                entity.update(cx, |favorites, cx| {
                    favorites.set_config(&self.configuration, cx)
                });
                self.favorites_subscription = Some(subscription);
                self.favorites = Some(entity);
                self.refresh_clipboard_colors(cx);
            }
            if let Some(favorites) = &self.favorites {
                favorites.update(cx, |favorites, cx| favorites.set_route(route, window, cx));
            }
        } else {
            if let Some(favorites) = &self.favorites {
                favorites.update(cx, |favorites, cx| favorites.suspend(cx));
            }
            self.focus.focus(window);
            self.restore_no_focus(window);
        }
        self.sync_native_interactions(cx);
        cx.notify();
    }

    fn sync_overlay_guard(&self) {
        if let Some(host) = self.model.host() {
            host.sync_native_clipboard_overlay(
                self.process_editor.is_some() || self.process_open_pending,
                self.list_settings.is_some()
                    || self.filter_panel.is_some()
                    || self.more_menu
                    || self.tag_editor.is_some()
                    || self.fifo_panel.is_some()
                    || self.history_dialog
                    || self.process_editor.is_some()
                    || self.process_open_pending,
            );
        }
    }

    fn sync_keyboard_paste_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self.editor_window.is_some() || crate::editor::has_active_input_windows();
        let searching = !self.query.is_empty() || self.search.read(cx).focus_handle().is_focused(window)
            || self.search.read(cx).is_composing();
        let overlay = self.list_settings.is_some() || self.filter_panel.is_some()
            || self.more_menu || self.tag_editor.is_some() || self.fifo_panel.is_some()
            || self.history_dialog || self.process_editor.is_some() || self.process_open_pending
            || self.menu_lease.is_some() || self.favorite_lease.is_some();
        let number_paste = self.configuration["usingNumberKeyToPaste"].as_bool().unwrap_or(true);
        let mut reason = if editing {
            Some(tr("正在编辑，数字会输入编辑器。", "Editing: numbers go to the editor."))
        } else if searching {
            Some(tr("正在搜索，数字会输入搜索框。", "Searching: numbers go to the search field."))
        } else if overlay {
            Some(tr("菜单或弹层打开时，数字键粘贴暂停。", "Number paste pauses while a menu or popup is open."))
        } else if !number_paste {
            Some(tr("已关闭数字键粘贴，数字会输入搜索框。", "Number paste is disabled; numbers go to search."))
        } else if self.page.rows.is_empty() || self.loading || self.page_generation != self.generation {
            Some(tr("当前没有就绪的可粘贴条目。", "No clipboard items are ready to paste."))
        } else { None };
        let (visible, owns_digits, keyboard_mode) = if let Some(host) = self.model.host() {
            let snapshot = host.hide_snapshot();
            let presented = host.native_clipboard_list_presented()
                || (snapshot.is_collapsed && snapshot.behavior == cliphost_core::model::DismissBehavior::Fold);
            (snapshot.visible && !self.favorites_active && !self.exit_pending,
                host.clipboard_digit_routing_active() && presented && !snapshot.is_layout_busy
                    && !snapshot.move_interaction_active,
                if snapshot.no_focus_session_active {
                    host.clipboard_keyboard_mode()
                } else {
                    cliphost_core::ClipboardKeyboardMode::Full
                })
        } else {
            (!self.favorites_active && !self.exit_pending, self.no_focus,
                cliphost_core::ClipboardKeyboardMode::Full)
        };
        if keyboard_mode == cliphost_core::ClipboardKeyboardMode::Released && !editing {
            reason = Some(tr(
                "已让出键盘。点击剪贴板内部或重新唤起后恢复数字粘贴。",
                "Keyboard released. Click inside clipboard or reopen it to resume number paste.",
            ));
        } else if keyboard_mode == cliphost_core::ClipboardKeyboardMode::DigitsOnly {
            if !self.query.is_empty() {
                reason = Some(tr(
                    "已有搜索条件，数字键粘贴暂停。外部输入不会改变搜索。",
                    "Number paste pauses with a search query. External typing leaves the query unchanged.",
                ));
            } else if !number_paste {
                reason = Some(tr(
                    "已关闭数字键粘贴，输入仍交给外部窗口。",
                    "Number paste is disabled; input goes to the external window.",
                ));
            }
        }
        let active = visible && owns_digits && reason.is_none();
        if !active && reason.is_none() {
            reason = Some(tr("当前未接管数字键。", "Number keys are not currently captured."));
        }
        let reason = reason.map(str::to_owned);
        if self.keyboard_paste_active != active || self.keyboard_paste_reason != reason
            || self.keyboard_paste_visible != visible
        {
            self.keyboard_paste_active = active;
            self.keyboard_paste_reason = reason;
            self.keyboard_paste_visible = visible;
            cx.notify();
        }
    }

    fn close_fifo(&mut self, cx: &mut Context<Self>) {
        if let Some((panel, _, lease)) = self.fifo_panel.take() {
            panel.update(cx, |panel, cx| panel.retire(cx));
            if let Some(host) = self.model.host() {
                host.release_native_context_menu(lease);
            }
            self.sync_overlay_guard();
            cx.notify();
        }
    }

    fn open_fifo(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        if self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || ids.is_empty()
            || self.page_generation != self.generation
        {
            return;
        }
        let anchor = self.menu_anchor.or_else(platform::cursor_position);
        self.close_fifo(cx);
        self.close_menu(cx);
        self.list_settings = None;
        self.filter_panel = None;
        self.tag_editor = None;
        let lease = crate::menu::next_lease();
        if !host.acquire_native_context_menu(lease, "clipboard") {
            return;
        }
        host.set_native_context_menu_hwnd(lease, host.host_hwnd());
        let params = json!({"ids":ids,"endId":ids.first(),"nativeMenuLease":lease,
            "visibleIds":self.page.rows.iter().map(|row| &row.item.id).collect::<Vec<_>>()});
        let root_window = self.window;
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = root_window.update(cx, |_, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if !host.native_context_menu_is_current(lease) {
                        return;
                    }
                    let mut params = params;
                    let (x, y) = match fifo_anchor(window, host.host_hwnd(), anchor) {
                        Ok(point) => point,
                        Err(error) => {
                            host.release_native_context_menu(lease);
                            this.error = Some(error);
                            cx.notify();
                            return;
                        }
                    };
                    params["x"] = json!(x);
                    params["y"] = json!(y);
                    this.cancel_native_hover();
                    let (tokens, dark) = (this.tokens.clone(), this.dark);
                    let entity =
                        cx.new(|cx| FifoPanel::new(host.clone(), tokens, dark, params, window, cx));
                    let subscription =
                        cx.subscribe_in(&entity, window, |this, _, event, _, cx| match event {
                            FifoPanelEvent::Started(_) | FifoPanelEvent::Cancelled => {
                                this.close_fifo(cx)
                            }
                        });
                    this.fifo_panel = Some((entity, subscription, lease));
                    this.sync_overlay_guard();
                    cx.notify();
                });
            });
        });
    }

    fn open_editor(&mut self, id: String, favorite: bool, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        if id.is_empty() || self.exit_pending {
            return;
        }
        // Editing from a hover/menu button selects that row too, so the next
        // list refresh cannot immediately switch back to the old selection.
        if !favorite {
            if let Some(index) = self.page.rows.iter().position(|row| row.item.id == id) {
                self.select_single(index);
                cx.notify();
            }
        }
        self.close_menu(cx);
        let (tokens, dark) = (self.tokens.clone(), self.dark);
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = view.update(cx, |this, cx| {
                if this.exit_pending {
                    return;
                }
                // External/toast edits may target an item outside this page.
                // Follow only a subsequent list selection change in that case.
                if let Some(handle) = this.lan_companion {
                    let _ = handle.update(cx, |lan, window, cx| lan.dismiss(window, cx));
                }
                this.editor_followed_selection = (!favorite).then(|| this.selected_id.clone());
                let result = if favorite {
                    EditorWindow::show_favorite(&mut this.editor_window, host, tokens, dark, id, cx)
                } else {
                    EditorWindow::show(&mut this.editor_window, host, tokens, dark, id, cx)
                };
                if let Err(error) = result {
                    this.error = Some(error);
                    cx.notify();
                }
                this.refresh_independent_surfaces(cx);
            });
        });
    }

    fn editor_is_open(&self, cx: &gpui::App) -> bool {
        self.editor_window.is_some_and(|handle| {
            handle.read(cx).is_ok_and(|editor| !editor.is_closed())
        })
    }

    fn lan_companion_is_open(&self, cx: &gpui::App) -> bool {
        self.lan_companion.is_some_and(|handle| handle.read(cx).is_ok_and(|lan| lan.is_open()))
    }

    fn open_lan_companion(&mut self, toggle: bool, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        if self.exit_pending { return; }
        let Some(host) = self.model.host() else { return; };
        self.close_menu(cx);
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = view.update(cx, |this, cx| {
                if this.exit_pending || task.is_some_and(|task| !host.prepare_chrome_task(task)) { return; }
                if toggle && this.lan_companion_is_open(cx) {
                    if let Some(handle) = this.lan_companion {
                        let _ = handle.update(cx, |lan, window, cx| lan.dismiss(window, cx));
                    }
                    cx.notify();
                    return;
                }
                // This is a visibility switch, so preserve the editor's dirty,
                // IME, picker and in-flight write guards without starting a save.
                if let Some(handle) = this.editor_window {
                    match handle.update(cx, |editor, window, cx| editor.toggle_close_if_clean(window, cx)) {
                        Ok(Ok(false)) => {
                            this.show_notice(tr("请先保存修改或等待编辑处理完成，再切换到互传。", "Save changes or wait for the editor to finish before switching to LAN transfer.").into(), cx);
                            return;
                        }
                        Ok(Err(error)) => { this.error = Some(error); cx.notify(); return; }
                        _ => {}
                    }
                    this.editor_window = None;
                    this.editor_followed_selection = None;
                }
                // A route invoked from the tray can arrive while the owner is
                // hidden or folded. Use its ordinary show transition first.
                if !host.native_clipboard_list_presented() { host.request_show(); }
                if let Err(error) = LanCompanionWindow::show(
                    &mut this.lan_companion, host, this.tokens.clone(), this.dark, cx,
                ) {
                    this.error = Some(error);
                }
                this.refresh_independent_surfaces(cx);
                cx.notify();
            });
        });
    }

    fn toggle_editor(&mut self, cx: &mut Context<Self>) {
        if self.exit_pending { return; }
        if !self.editor_is_open(cx) {
            self.open_editor(self.selected_id.clone(), false, cx);
            return;
        }
        let Some(handle) = self.editor_window else { return; };
        let view = cx.weak_entity();
        // The editor owns its draft. Check and close inside its Window borrow;
        // never auto-save or discard to satisfy a toolbar toggle.
        cx.defer(move |cx| {
            if !view.read_with(cx, |this, _| !this.exit_pending && this.editor_window == Some(handle)).unwrap_or(false) { return; }
            let result = handle.update(cx, |editor, window, cx| editor.toggle_close_if_clean(window, cx));
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(Ok(false)) => this.show_notice(tr("请先保存修改或等待处理完成，再关闭编辑。", "Save changes or wait for the editor to finish before closing.").into(), cx),
                    Ok(Err(error)) => this.error = Some(error),
                    _ => {}
                }
                // maintain_editor retires the handle and restores no-focus.
                cx.notify();
            });
        });
    }

    /// Selection changes use the existing revision-checked editor lifecycle.
    /// Defer across Window borrows and never activate the editor while browsing.
    fn sync_editor_selection(&mut self, cx: &mut Context<Self>) {
        if self.exit_pending || self.favorites_active
            || self.editor_followed_selection.is_none() || !self.editor_is_open(cx)
        {
            return;
        }
        if self.selected_id.is_empty()
            || self.editor_followed_selection.as_ref() == Some(&self.selected_id)
        {
            return;
        }
        let handle = self.editor_window.unwrap();
        let id = self.selected_id.clone();
        self.editor_followed_selection = Some(id.clone());
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let current = view.read_with(cx, |this, _| {
                !this.exit_pending && !this.favorites_active
                    && this.editor_window == Some(handle) && this.selected_id == id
            }).unwrap_or(false);
            let delivered = current && handle.update(cx, |editor, window, cx| {
                    editor.follow_selection(EditorTarget::History(id.clone()), window, cx);
                }).is_ok();
            if !delivered {
                let _ = view.update(cx, |this, cx| {
                    if this.editor_window == Some(handle)
                        && this.editor_followed_selection.as_ref() == Some(&id)
                    {
                        this.editor_followed_selection = Some(String::new());
                        cx.notify();
                    }
                });
            }
        });
    }

    fn maintain_editor(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.editor_window.is_some() && !self.editor_is_open(cx) {
            self.editor_window = None;
            self.editor_followed_selection = None;
            self.restore_no_focus(window);
            cx.notify();
        }
        self.sync_editor_selection(cx);
        let lan_open = self.lan_companion_is_open(cx);
        if self.lan_companion_open && !lan_open {
            self.restore_no_focus(window);
            cx.notify();
        }
        self.lan_companion_open = lan_open;
        if let (Some(handle), Some(host)) = (self.lan_companion, self.model.host()) {
            if lan_open {
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |lan, window, cx| {
                        lan.sync_owner_presented(host.native_clipboard_list_presented(), window, cx);
                    });
                });
            }
        }
    }

    fn open_history_dialog(&mut self, dialog: HistoryDialog, cx: &mut Context<Self>) {
        self.cancel_native_hover();
        if self.favorites_active || self.exit_pending {
            return;
        }
        self.retire_process_editor(cx);
        self.close_menu(cx);
        self.close_fifo(cx);
        self.list_settings = None;
        self.filter_panel = None;
        self.tag_editor = None;
        let root = self.window;
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = root.update(cx, |_, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if this.favorites_active || this.exit_pending {
                        return;
                    }
                    if let Some(actions) = &this.history_actions {
                        actions.update(cx, |actions, cx| {
                            match dialog {
                                HistoryDialog::AddText => actions.open_add(window, cx),
                                HistoryDialog::Clear(None) => actions.open_clear(window, cx),
                                HistoryDialog::Clear(Some(scope)) => {
                                    actions.open_clear_scope(scope, window, cx);
                                }
                            }
                        });
                    }
                });
            });
        });
    }

    fn request_exit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_pending {
            return;
        }
        if let Some((editor, _)) = &self.tag_editor {
            if editor.read(cx).is_composing(cx) {
                self.error = Some(
                    tr(
                        "请先完成分类名称的输入，再退出程序。",
                        "Finish composing the category name before exiting.",
                    )
                    .into(),
                );
                crate::settings::update_exit_cancelled(self.error.as_deref().unwrap_or_default());
                cx.notify();
                return;
            }
            editor.update(cx, |editor, cx| editor.set_leave_locked(true, cx));
        }
        self.exit_pending = true;
        crate::filters::set_filter_tab_exit_pending(true);
        if let Some(actions) = &self.history_actions {
            actions.update(cx, |actions, cx| actions.set_exit_pending(true, cx));
        }
        crate::native_interactions::set_native_drop_exit_pending(true);
        self.cancel_native_hover();
        #[cfg(windows)]
        if let Some(native) = self.interactions.borrow_mut().as_mut() {
            let _ = native.sync_session(
                next_native_session(),
                false,
                DropDestination {
                    label: "clipboard",
                    parent_id: None,
                    insert_index: -1,
                },
            );
        }
        self.retire_process_editor(cx);
        let favorites_store = self.favorites.as_ref().map(|favorites| {
            favorites.update(cx, |favorites, _| favorites.flush_for_exit())
        });
        // Keep the root alive until live editors have committed their newest
        // drafts and destroyed windows' accepted writes have drained as well.
        cx.defer_in(window, move |this, window, cx| {
            let mut waits = Vec::new();
            if let Some(handle) = this.management_window {
                let _ = handle.update(cx, |management, _, cx| {
                    management.set_exit_pending(true, cx)
                });
            }
            if let Some(handle) = this.settings_window {
                let _ = handle.update(cx, |settings, _, cx| settings.set_exit_pending(true, cx));
            }
            if let Some(handle) = this.lan_companion {
                let _ = handle.update(cx, |lan, window, cx| {
                    lan.set_exit_pending(true, cx);
                    lan.dismiss(window, cx);
                });
            }
            if let Some(handle) = this.editor_window {
                if handle
                    .update(cx, |editor, _, _| !editor.is_closed())
                    .unwrap_or(false)
                {
                    waits.push(EditorWindow::save_and_close(handle, cx));
                }
            }
            if let Some(handle) = this.panel_window {
                waits.push(PanelWindow::save_embedded_settings_for_exit(handle, cx));
            }
            if let Some(handle) = this.reopen_window {
                waits.push(ReopenWindow::save_and_close(handle, cx));
            }
            if let Some(handle) = this.settings_window {
                if handle.update(cx, |_, _, _| ()).is_ok() {
                    waits.push(SettingsWindow::save_and_close(handle, cx));
                }
            }
            let appearance_queue = this.appearance_writes.clone();

            cx.spawn_in(window, async move |this, cx| {
                let mut errors = Vec::new();
                for wait in waits {
                    match wait.await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => errors.push(error),
                        Err(_) => errors.push(
                            tr(
                                "保存等待已中断，程序继续运行。",
                                "Saving was interrupted; the application remains open.",
                            )
                            .into(),
                        ),
                    }
                }
                if let Err(error) = crate::editor::flush_editor_writes().await {
                    errors.push(error);
                }
                if let Err(error) = crate::history_actions::flush_history_action_writes().await {
                    errors.push(error);
                }
                if let Err(error) = crate::settings::flush_settings_writes().await {
                    errors.push(error);
                }
                if let Err(error) = crate::management::flush_management_lan_writes().await {
                    errors.push(error);
                }
                if let Err(error) = crate::filters::flush_filter_tab_writes().await {
                    errors.push(error);
                }
                // Accepted appearance writes outlive a dismissed popup;
                // leave failed intent visible instead of quitting over it.
                while appearance_queue.lock().unwrap().running {
                    Timer::after(Duration::from_millis(10)).await;
                }
                if let Some(error) = appearance_queue.lock().unwrap().failure.clone() {
                    errors.push(error);
                }
                if let Err(error) = crate::native_interactions::flush_native_drop_writes().await {
                    errors.push(error);
                }
                if let Err(error) = crate::process_filters::flush_process_filter_writes().await {
                    errors.push(error);
                }
                // GPUI's app-quit hook has only 100 ms. Drain this shared store
                // before quitting so replacement retries and pending revisions
                // can finish, and leave a failed save available for retry.
                if let Some(store) = favorites_store {
                    if let Err(error) = cx
                        .background_executor()
                        .spawn(async move { store.flush() })
                        .await
                    {
                        errors.push(error);
                    }
                }
                if errors.is_empty() {
                    if let Err(error) = cx.background_executor()
                        .spawn(async { crate::settings::authorize_update_exit() }).await {
                        errors.push(error);
                    }
                }
                if !errors.is_empty() {
                    crate::settings::update_exit_cancelled(&errors.join("\n"));
                }
                let exit_result = this.update_in(cx, |this, _, cx| {
                    this.exit_pending = false;
                    if errors.is_empty() {
                        log::warn!(target: "cliphost_gpui::lifecycle", "Accepted writes drained; quitting GPUI");
                        cx.quit();
                    } else {
                        crate::filters::set_filter_tab_exit_pending(false);
                        if let Some((editor, _)) = &this.tag_editor {
                            editor.update(cx, |editor, cx| editor.set_leave_locked(false, cx));
                        }
                        if let Some(actions) = &this.history_actions {
                            actions.update(cx, |actions, cx| actions.set_exit_pending(false, cx));
                        }
                        crate::native_interactions::set_native_drop_exit_pending(false);
                        if let Some(handle) = this.management_window {
                            cx.defer(move |cx| {
                                let _ = handle.update(cx, |management, _, cx| {
                                    management.set_exit_pending(false, cx)
                                });
                            });
                        }
                        if let Some(handle) = this.settings_window {
                            cx.defer(move |cx| {
                                let _ = handle.update(cx, |settings, _, cx| settings.set_exit_pending(false, cx));
                            });
                        }
                        if let Some(handle) = this.lan_companion {
                            cx.defer(move |cx| {
                                let _ = handle.update(cx, |lan, _, cx| lan.set_exit_pending(false, cx));
                            });
                        }
                        this.sync_native_interactions(cx);
                        this.error = Some(errors.join("\n"));
                        cx.notify();
                    }
                });
                if exit_result.is_err() {
                    crate::settings::update_exit_cancelled("Exit continuation was interrupted");
                }
            })
            .detach();
        });
    }

    fn present_main_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_transition_pending || self.exit_pending {
            return;
        }
        let Some((route, task)) = self.pending_main_route.take() else {
            return;
        };
        let route = route.map(normalize_main_settings_route);
        let Some(host) = self.model.host() else {
            return;
        };
        if !host.prepare_chrome_task(task) {
            return;
        }
        if route.as_deref().is_some_and(|route| route.split(['?', '#']).next() == Some("/main/lan")) {
            self.open_lan_companion(false, Some(task), cx);
            return;
        }
        let page = route.as_deref().map(settings_page_from_route);
        if let Some(None) = page {
            self.error = Some(tr("GPUI 尚未迁移此页面，可使用保留的 Tauri 版本。", "This page is not yet available in GPUI. You can use the retained Tauri version.").into());
            cx.notify();
            return;
        }
        cx.defer_in(window, move |this, _, cx| {
            if this.exit_pending || !host.prepare_chrome_task(task) {
                return;
            }
            let _permission = platform::guard_task(&host, task);
            if let Some(handle) = this.management_window.take() {
                ManagementWindow::close(handle, cx);
            }
            if let Err(error) = SettingsWindow::show(
                &mut this.settings_window,
                host,
                this.tokens.clone(),
                this.dark,
                page.flatten(),
                cx,
            ) {
                this.error = Some(error);
                cx.notify();
            }
            this.refresh_independent_surfaces(cx);
            if let (Some(handle), Some(route)) = (this.settings_window, route) {
                let _ = handle.update(cx, |settings, window, cx| {
                    settings.set_route(&route, window, cx)
                });
            }
        });
    }

    pub fn enable_diagnostics(&mut self) {
        self.stats_enabled = true;
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.process_editor.is_some() || self.process_open_pending || self.history_dialog {
            return;
        }
        if self.favorites_active {
            if let Some(favorites) = &self.favorites {
                favorites.update(cx, |favorites, cx| favorites.focus_search(window, cx));
            }
            return;
        }
        self.close_fifo(cx);
        // Activating from a deferred callback avoids Windows message reentry while
        // GPUI still holds the input event's App/Window borrow.
        cx.defer_in(window, |this, window, cx| {
            let result = if let Some(host) = this.model.host() {
                NativeWindow::request_search_focus(window, &host)
            } else {
                platform::set_no_activate(window, false).map(|_| window.activate_window())
            };
            if let Err(error) = result {
                this.error = Some(error);
            } else {
                this.search.read(cx).focus_handle().focus(window);
            }
            cx.notify();
        });
    }

    fn blur_search_from_pointer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.read(cx).focus_handle().is_focused(window)
            || self.search.read(cx).is_composing()
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.history_dialog
        {
            return;
        }
        self.focus.focus(window);
        self.restore_no_focus(window);
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_with_delay(Duration::from_millis(70), cx);
    }

    fn retire_history_query(&mut self, cx: &mut Context<Self>) {
        self.history_scrollbar.cancel();
        if !self.favorites_active {
            if let Some(host) = self.model.host() {
                host.retire_native_clipboard_list_input();
            }
            self.cancel_native_hover();
            self.native_session = next_native_session();
            self.sync_native_interactions(cx);
        }
        self.generation += 1;
        self.pending = None;
        self.page_pending = None;
    }

    fn refresh_with_delay(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if let Some(host) = self.model.host() {
            // Publish the new query before another low-level key can be routed.
            host.set_clipboard_search_active(!self.query.is_empty());
        }
        self.retire_history_query(cx);
        self.history_refill_target = None;
        self.history_delete_range_valid = false;
        let generation = self.generation;
        let model = self.model.clone();
        let query = self.query.clone();
        let request = self.current_request();
        let committed_query = (query.clone(), request.clone());
        let same_query = self.committed_query.as_ref() == Some(&committed_query);
        self.background_refresh = same_query && !self.page.rows.is_empty();
        let take = if matches!(model, UiModel::Live(_)) {
            if same_query { self.page.rows.len().max(100) } else { 100 }
        } else {
            1_000
        };
        self.loading = true;
        let query_error = self.error.clone();
        self.pending = Some(cx.spawn(async move |this, cx| {
            // Dropping the previous task coalesces quick typing before costly ranking.
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            let began = Instant::now();
            let page = cx
                .background_executor()
                .spawn(async move {
                    model.query(&request, &query, 0, take)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.last_query_ms = began.elapsed().as_secs_f64() * 1_000.0;
                let page = match page {
                    Ok(page) => page,
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                        return;
                    }
                };
                // A successful read must not erase a newer failed mutation.
                if this.error == query_error {
                    this.error = None;
                }
                this.page_generation = generation;
                this.history_removals.forget(page.rows.iter().map(|row| row.item.id.as_str()));
                let old_ids: Vec<&str> = this.page.rows.iter().map(|row| row.item.id.as_str()).collect();
                let new_ids: Vec<&str> = page.rows.iter().map(|row| row.item.id.as_str()).collect();
                let scroll_anchor = same_query.then(|| crate::history_scroll::refreshed_anchor(
                    &old_ids, &new_ids, this.list.logical_scroll_top(),
                ));
                this.anchor = if same_query {
                    this.anchor.and_then(|index| old_ids.get(index))
                        .and_then(|id| new_ids.iter().position(|current| current == id))
                } else { None };
                let selected = if same_query {
                    crate::history_scroll::refreshed_selection(&old_ids, &new_ids, &this.selected_id)
                } else {
                    new_ids.iter().position(|id| *id == this.selected_id)
                        .or_else(|| (!page.rows.is_empty()).then_some(0))
                };
                this.selected_id = selected.and_then(|index| page.rows.get(index))
                    .map(|row| row.item.id.clone()).unwrap_or_default();
                let present: HashSet<&str> =
                    page.rows.iter().map(|row| row.item.id.as_str()).collect();
                this.selected_ids.retain(|id| present.contains(id.as_str()));
                if this.selected_ids.is_empty() {
                    if !this.selected_id.is_empty() { this.selected_ids.push(this.selected_id.clone()); }
                    this.anchor = None;
                } else if !this.selected_ids.contains(&this.selected_id) {
                    this.selected_id = this.selected_ids[0].clone();
                }
                this.history_scrollbar.cancel();
                let old_keys: Vec<_> = this.page.rows.iter().map(crate::history_page::row_key).collect();
                let new_keys: Vec<_> = page.rows.iter().map(crate::history_page::row_key).collect();
                if same_query {
                    crate::list_reconcile::reconcile(&this.list, &old_keys, &new_keys);
                    let previous: HashSet<_> = old_keys.iter().collect();
                    let unchanged: HashSet<_> = new_keys.iter().filter(|key| previous.contains(key)).map(|key| key.0.as_str()).collect();
                    let rows: HashMap<_, _> = page.rows.iter().map(|row| (row.item.id.as_str(), row)).collect();
                    let mode = this.content_mode();
                    let foreground = &this.tokens.theme(this.dark).text;
                    this.content_cache.advance_retaining(generation, |key| {
                        rows.get(key.item_id.as_str()).is_some_and(|row| {
                            let (resource_mode, resource_foreground) = Self::content_resource_style(row, mode, foreground);
                            (row.item.kind == "ImageClipItem" || unchanged.contains(key.item_id.as_str()))
                                && key.mode == resource_mode && key.foreground == resource_foreground
                                && Self::content_projection(row, mode).as_ref() == Some(&key.projection)
                        })
                    });
                } else {
                    this.list.reset(page.rows.len());
                    // Query/sort changes retire page work, but an unchanged image
                    // source can keep its already prepared raster.
                    this.content_cache.advance_retaining(generation, |key| {
                        page.rows.iter().find(|row| row.item.id == key.item_id)
                            .filter(|row| row.item.kind == "ImageClipItem")
                            .is_some_and(|row| Self::content_projection(row, ContentMode::Standard).as_ref() == Some(&key.projection))
                    });
                }
                if let Some(anchor) = scroll_anchor {
                    this.list.scroll_to(anchor);
                }
                this.page = page;
                this.committed_query = Some(committed_query);
                this.sync_history_top_button(cx);
                this.content_cache.release_evicted(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let take = self.history_refill_target
            .map(|target| target.min(self.page.total).saturating_sub(self.page.rows.len()).max(1))
            .unwrap_or(100);
        self.load_more_count(take, cx);
    }

    fn load_more_count(&mut self, take: usize, cx: &mut Context<Self>) {
        if self.loading || self.page_generation != self.generation
            || self.page_pending.is_some() || !self.page.has_more
            || !self.history_delete_pending.is_empty() {
            return;
        }
        let generation = self.generation;
        let model = self.model.clone();
        let query = self.query.clone();
        let request = self.current_request();
        let skip = self.page.next_skip;
        self.page_pending = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { model.query(&request, &query, skip, take) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.page_pending = None;
                match result {
                    Ok(page) => {
                        this.history_removals.forget(page.rows.iter().map(|row| row.item.id.as_str()));
                        let start = this.page.rows.len();
                        let existing: HashSet<_> = this
                            .page
                            .rows
                            .iter()
                            .map(|row| row.item.id.clone())
                            .collect();
                        this.page.rows.extend(
                            page.rows
                                .into_iter()
                                .filter(|row| !existing.contains(&row.item.id)),
                        );
                        this.page.total = page.total;
                        this.page.next_skip = page.next_skip;
                        this.page.has_more = page.has_more;
                        if this.history_refill_target.is_some_and(|target| {
                            this.page.rows.len() >= target.min(this.page.total) || !this.page.has_more
                        }) {
                            this.history_refill_target = None;
                        }
                        this.history_scrollbar.invalidate_measurements();
                        this.list.splice(start..start, this.page.rows.len() - start);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        }));
    }

    fn maintenance_delay(&self, cx: &gpui::App) -> Option<Duration> {
        let host = self.model.host();
        let active = host.as_ref().is_some_and(|host| host.needs_native_ui_poll())
            || self.editor_window.is_some()
            || self.lan_companion_open
            || self.main_transition_pending
            || self.native_dragging
            || self.history_content_drag.as_ref().is_some_and(|drag| drag.active());
        let poll = self.native.as_ref().and_then(|native| native.poll_interval(active));
        // NativeWindow intentionally defers hide policy during HWND setup and
        // captured scrollbar drags; do not spin on an overdue FIFO deadline.
        let can_expire_fifo = self.native.as_ref().is_some_and(NativeWindow::ready_for_maintenance)
            && !self.history_scrollbar.is_dragging()
            && !self.favorites.as_ref().is_some_and(|favorites| favorites.read(cx).scrollbar_drag_active());
        native_maintenance::next_delay(
            poll,
            self.content_cache.next_idle_deadline().into_iter()
                .chain(self.favorites.as_ref().and_then(|favorites| favorites.read(cx).next_idle_content_deadline()))
                .chain(self.history_idle.next_hidden_deadline())
                .chain(can_expire_fifo.then(|| host.as_ref().and_then(|host| host.next_native_ui_deadline())).flatten()),
            Instant::now(),
        )
    }

    fn start_live(&mut self, window: &Window, cx: &mut Context<Self>) {
        let UiModel::Live(service) = &self.model else {
            return;
        };
        let mut events = service.subscribe();
        let chrome = service.host().chrome_command_notifier();
        let local = Arc::new(tokio::sync::Notify::new());
        let local_observer = local.clone();
        // Background result application and local UI transitions notify their
        // entity. Recompute deadlines even when the HWND is already hidden.
        self._subscriptions.push(cx.observe_self(move |this, cx| {
            // Active maintenance already sees local changes on its next tick.
            // Avoid feeding animation/render notifications back into that pump.
            if this.maintenance_delay(cx).is_none_or(|delay| delay > Duration::from_millis(50)) {
                local_observer.notify_one();
            }
        }));
        self.maintenance = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(delay) = this.read_with(cx, |this, cx| this.maintenance_delay(cx)) else { break; };
                let mut first_event = match native_maintenance::wait_for_work(&mut events, &chrome, &local, delay).await {
                    Ok(event) => event.map(Ok),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) =>
                        Some(Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped))),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let mut changed = false;
                let mut favorites_changed = false;
                let mut history_invalidations = Some(HashSet::new());
                let mut history_removals = Some(HashSet::new());
                let mut favorite_invalidations = Some(HashSet::new());
                let mut configuration_changed = false;
                let mut settings_changed = false;
                let mut foreground_changed = false;
                let mut presets_changed = false;
                let mut service_error = None;
                let mut lan_events = HashMap::new();
                loop {
                    match first_event.take().unwrap_or_else(|| events.try_recv()) {
                        Ok(event) => {
                            if matches!(
                                event.name.as_str(),
                                "lan.share.changed" | "lan.peers.changed" | "deviceSyncChanged"
                            ) {
                                lan_events.insert(event.name.clone(), event.data.clone());
                            }
                            if event.name == "favoritesChanged"
                                || event.name == "nativeRefreshRequired"
                            {
                                favorites_changed = true;
                                if let Some(ids) = crate::list_reconcile::invalidation_ids(&event.data, "upsertIds") {
                                    if let Some(pending) = &mut favorite_invalidations { pending.extend(ids); }
                                } else { favorite_invalidations = None; }
                            }
                            if event.name == "clipConfigChanged" {
                                configuration_changed = true;
                            }
                            if event.name == "nativeSettingsChanged" || event.name == "reopenHistoryChanged" {
                                settings_changed = true;
                            }
                            if event.name == "foregroundProcessChanged" {
                                foreground_changed = true;
                            }
                            if event.name == "processFilterPresetsChanged" {
                                presets_changed = true;
                            }
                            if event.name == "nativeServiceError" {
                                service_error = Some(event.data.to_string());
                            }
                            if event.name.contains("clip")
                                || event.name.contains("Clip")
                                || event.name == "nativeRefreshRequired"
                            {
                                changed = true;
                                if event.name == "clipChanged" {
                                    match crate::history_removal::pure_delete_ids(&event.data) {
                                        Some(ids) => {
                                            if let Some(pending) = &mut history_removals { pending.extend(ids); }
                                        }
                                        None => history_removals = None,
                                    }
                                    if let Some(ids) = crate::list_reconcile::invalidation_ids(&event.data, "items") {
                                        if let Some(pending) = &mut history_invalidations { pending.extend(ids); }
                                    } else { history_invalidations = None; }
                                } else {
                                    history_invalidations = None;
                                    history_removals = None;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                            changed = true;
                            favorites_changed = true;
                            history_invalidations = None;
                            history_removals = None;
                            favorite_invalidations = None;
                            configuration_changed = true;
                            presets_changed = true;
                            // Recover both peer/status and chat snapshots after a
                            // dropped LAN event, including events without a DTO.
                            lan_events.insert("lan.peers.changed".to_string(), Value::Null);
                        }
                        Err(_) => break,
                    }
                }
                if this
                    .update_in(cx, |this, window, cx| {
                        cx.defer(crate::favorite_drag::maintain);
                        if this.history_content_drag.as_ref().is_some_and(|drag|!drag.active()) {
                            let drag=this.history_content_drag.take().unwrap();
                            this.native_dragging=false;
                            this.native_suppress_click=Some(Instant::now()+Duration::from_millis(300));
                            this.drag_tab_hover=None;
                            if !drag.is_claimed() && this.favorites_active {
                                this.set_clipboard_route("/clipboard",window,cx);
                            }
                        }
                        let Some(host) = this.model.host() else {
                            return;
                        };
                        this.maintain_editor(window, cx);
                        this.sync_overlay_guard();
                        if let Some((lease, _)) = this.menu_lease.as_ref() {
                            let lease = *lease;
                            for event in host.drain_native_context_menu_keys(lease) {
                                if this.exit_pending || !host.native_context_menu_key_is_current(&event) {
                                    continue;
                                }
                                if let Some(menu) = this.menu {
                                    ContextMenuPopup::handle_history_key(menu, lease, event.key(), cx);
                                }
                            }
                        }
                        if this.fifo_panel.as_ref().is_some_and(|(_, _, lease)| {
                            !host.native_context_menu_is_current(*lease)
                        }) {
                            this.close_fifo(cx);
                        }
                        if this.fifo_panel.is_some()
                            && host.native_fifo_keyboard_target_left()
                        {
                            this.close_fifo(cx);
                        }
                        if let Some((panel, _, _)) = &this.fifo_panel {
                            let composing = this.search.read(cx).is_composing();
                            panel.update(cx, |panel, cx| {
                                panel.sync_native_keyboard(window, composing, cx);
                            });
                        }
                        for _ in 0..64 {
                            let Some(event) = host.take_native_fifo_key() else {
                                break;
                            };
                            if !this.exit_pending && host.native_fifo_key_is_current(&event) {
                                if let Some((panel, _, _)) = &this.fifo_panel {
                                    panel.update(cx, |panel, cx| {
                                        panel.handle_native_key(&event, window, cx);
                                    });
                                }
                            }
                        }
                        if configuration_changed {
                            this.merge_authoritative_config(&host.config_get(), cx);
                            this.history_scrollbar.cancel();
                            crate::history_scroll::remeasure(&this.list);
                        }
                        if configuration_changed || settings_changed {
                            if let Some(handle) = this.reopen_window {
                                ReopenWindow::refresh(handle, cx);
                            }
                            if let Some(form) = &this.list_settings {
                                form.update(cx, |form, cx| form.request_authoritative_refresh(cx));
                            }
                            if let Some(handle) = this.settings_window {
                                cx.defer(move |cx| {
                                    let _ = handle.update(cx, |settings, window, cx| {
                                        settings.request_authoritative_refresh(window, cx);
                                    });
                                });
                            }
                        }
                        this.refresh_theme(window, cx);
                        if let Some(handle) = this.settings_window {
                            if !lan_events.is_empty() {
                                let sync_events = lan_events.clone();
                                cx.defer(move |cx| {
                                    let _ = handle.update(cx, |settings, window, cx| {
                                        for (name, data) in sync_events {
                                            settings.handle_device_sync_event(&name, &data, window, cx);
                                        }
                                    });
                                });
                            }
                        }
                        if let Some(handle) = this.lan_companion {
                            if !lan_events.is_empty() {
                                let companion_events = lan_events.clone();
                                cx.defer(move |cx| {
                                    let _ = handle.update(cx, |lan, window, cx| {
                                        for (name, data) in companion_events {
                                            lan.handle_lan_event(&name, &data, window, cx);
                                        }
                                    });
                                });
                            }
                        }
                        if let Some(handle) = this.management_window {
                            if !lan_events.is_empty() {
                                cx.defer(move |cx| {
                                    let _ = handle.update(cx, |management, window, cx| {
                                        for (name, data) in lan_events {
                                            management.handle_lan_event(&name, &data, window, cx);
                                        }
                                    });
                                });
                            }
                        }
                        if presets_changed {
                            this.process_reload_requested = true;
                        }
                        if this.process_reload_requested
                            && !this.process_loading
                            && this.process_editor.is_none()
                            && !this.process_open_pending
                        {
                            this.process_reload_requested = false;
                            this.load_process_presets(false, window, cx);
                        }
                        if foreground_changed
                            && this
                                .process_selection
                                .uses_foreground(&this.process_presets)
                        {
                            changed = true;
                            history_removals = None;
                        }
                        if let Some(error) = service_error {
                            this.error = Some(error);
                            cx.notify();
                        }
                        let content_drag_active = this.history_scrollbar.is_dragging()
                            || this.favorites.as_ref().is_some_and(|favorites| {
                                favorites.read(cx).scrollbar_drag_active()
                            });
                        if let Some(native) = &mut this.native {
                            match platform::with_hit_test_probe(|| {
                                native.poll(window, &host, content_drag_active)
                            }) {
                                Ok(tasks) => {
                                    for task in tasks {
                                        this.dispatch(task, window, cx);
                                    }
                                }
                                Err(error) => {
                                    if this.error.as_ref() != Some(&error) {
                                        this.error = Some(error);
                                        cx.notify();
                                    }
                                }
                            }
                        }
                        // Deliver the exact presentation/scope accepted by the
                        // native hook. Never capture a newer menu request here.
                        for _ in 0..128 {
                            let Some(event) = host.take_native_super_menu_key() else {
                                break;
                            };
                            if !this.exit_pending {
                                if let Some(handle) = this.super_menu {
                                    SuperMenuWindow::handle_native_key(handle, event, cx);
                                }
                            }
                        }
                        if let Some(route) = host.take_pending_nav("clipboard") {
                            this.set_clipboard_route(&route, window, cx);
                        }
                        let snapshot = host.hide_snapshot();
                        let presentation = host.native_clipboard_presentation_stamp();
                        if (!snapshot.visible
                            || snapshot.is_collapsed
                            || this
                                .process_presentation
                                .is_some_and(|stamp| stamp != (presentation.0, presentation.1)))
                            && (this.process_editor.is_some() || this.process_open_pending)
                        {
                            this.close_process_editor(window, cx);
                        }
                        this.sync_native_interactions(cx);
                        this.drain_native_interactions(window, cx);
                        this.fill_source_icons(window, cx);
                        this.fill_content_resources(cx);
                        let state = host.window_state_dto();
                        let favorites_visible =
                            this.favorites_active && host.hide_snapshot().visible;
                        let history_visible =
                            !this.favorites_active && host.hide_snapshot().visible;
                        if let Some(actions) = &this.history_actions {
                            actions.update(cx, |actions, cx| {
                                if history_visible != this.history_visible {
                                    if history_visible {
                                        actions.resume(cx);
                                    } else {
                                        actions.suspend(window, cx);
                                    }
                                }
                                actions.maintain(cx);
                            });
                        }
                        if this.history_visible
                            && !history_visible
                            && this.configuration["clearFilterWhileHide"].as_bool() == Some(true)
                        {
                            this.query.clear();
                            host.set_clipboard_search_active(false);
                            this.search
                                .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                            this.advanced = AdvancedFilter::default();
                            this.process_selection.clear();
                            this.category = this
                                .tabs
                                .iter()
                                .find(|tab| !tab.is_pinned())
                                .map(|tab| tab.key.clone())
                                .unwrap_or_else(|| "all".into());
                            this.filter_panel = None;
                            changed = true;
                        }
                        this.history_visible = history_visible;
                        let now = Instant::now();
                        this.history_idle.configure(
                            this.configuration["autoScrollToTop"].as_bool().unwrap_or(false),
                            this.configuration["autoScrollToTopIdleSeconds"].as_u64().unwrap_or(60).clamp(1, 86_400),
                            now,
                        );
                        let presence = if !snapshot.visible {
                            crate::history_idle::HistoryPresence::Hidden
                        } else if !this.favorites_active && host.native_clipboard_list_presented() {
                            crate::history_idle::HistoryPresence::Presented
                        } else {
                            crate::history_idle::HistoryPresence::Concealed
                        };
                        // A pinned window keeps its place while someone is on it.
                        let in_use = snapshot.pointer_in_host || snapshot.is_active || snapshot.has_fake_focus;
                        let busy = snapshot.spa_modal_open || snapshot.has_open_host_overlay
                            || snapshot.move_interaction_active || snapshot.is_layout_busy
                            || content_drag_active || this.native_dragging
                            || this.history_content_drag.as_ref().is_some_and(|drag| drag.active())
                            || this.search.read(cx).is_composing()
                            || this.editor_window.is_some() || crate::editor::has_active_input_windows()
                            || this.list_settings.is_some() || this.filter_panel.is_some()
                            || this.tag_editor.is_some() || this.more_menu || this.history_dialog
                            || this.process_editor.is_some() || this.process_open_pending
                            || this.fifo_panel.is_some() || this.menu_lease.is_some()
                            || this.favorite_lease.is_some();
                        if this.history_idle.poll(now, presence, busy, in_use) {
                            this.return_history_to_top(cx);
                        }
                        this.sync_history_top_button(cx);
                        if favorites_visible != this.favorites_visible {
                            this.favorites_visible = favorites_visible;
                            if let Some(favorites) = &this.favorites {
                                favorites.update(cx, |favorites, cx| {
                                    if favorites_visible {
                                        favorites.resume(cx);
                                    } else {
                                        favorites.suspend(cx);
                                    }
                                });
                            }
                        }
                        if let Some(favorites) = &this.favorites {
                            // Arm the grace on the same Hide that suspends the
                            // favorites owner; there may be no further tick.
                            favorites.update(cx, |favorites, cx| favorites.reclaim_idle_content(cx));
                        }
                        if state != this.window_state {
                            this.window_state = state;
                            cx.notify();
                        }
                        this.sync_keyboard_paste_status(window, cx);
                        if let Some(overlay) = this.quick_paste {
                            QuickPasteOverlay::maintain(overlay, &host, cx);
                        }
                        if let Some(menu) = this.super_menu {
                            SuperMenuWindow::maintain(menu, cx);
                        }
                        if let Some(panel) = this.panel_window {
                            PanelWindow::maintain(panel, &host, this.dark, cx);
                            if favorites_changed {
                                PanelWindow::invalidate_content_resources(panel, favorite_invalidations.as_ref(), cx);
                                PanelWindow::refresh(panel, cx);
                            }
                        }
                        if let Some(ceabox) = this.ceabox_window {
                            CeaBoxWindow::maintain(ceabox, &host, this.dark, cx);
                            if changed || favorites_changed {
                                CeaBoxWindow::refresh(ceabox, cx);
                            }
                        }
                        if let Some(reopen) = this.reopen_window {
                            ReopenWindow::maintain(reopen, this.dark, cx);
                        }
                        if let Some(browser) = this.file_browser_window {
                            FileBrowserWindow::maintain(browser, &host, this.dark, cx);
                        }
                        if this
                            .menu_lease
                            .as_ref()
                            .is_some_and(|(lease, _)| !host.native_context_menu_is_current(*lease))
                            || this.favorite_lease.is_some_and(|lease| !host.native_context_menu_is_current(lease))
                        {
                            this.close_menu(cx);
                        }
                        if changed {
                            if let Some(ids) = history_removals {
                                let local = ids.iter().all(|id| this.history_delete_pending.contains(id));
                                this.apply_confirmed_history_removals(&ids, local, cx);
                            } else {
                                this.history_removals.clear();
                                this.content_cache.advance_retaining(this.page_generation, |key| {
                                    // Even a configuration/unknown event can keep
                                    // the old page's raster until the refresh has
                                    // re-probed its source and compared versions.
                                    this.page.rows.iter().any(|row| row.item.id == key.item_id
                                        && row.item.kind == "ImageClipItem")
                                        || history_invalidations.as_ref().is_some_and(|ids| !ids.contains(&key.item_id))
                                });
                                this.content_cache.release_evicted(cx);
                                this.refresh(cx);
                            }
                        }
                        if favorites_changed && this.favorites_active {
                            if let Some(favorites) = &this.favorites {
                                favorites.update(cx, |favorites, cx| {
                                    favorites.invalidate_content_resources(favorite_invalidations.as_ref(), cx);
                                    favorites.refresh(cx);
                                });
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn dispatch(&mut self, task: ChromeTask, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        if !host.prepare_chrome_task(task) {
            return;
        }
        if !self.favorites_active && matches!(task.command(),
            ChromeCommand::RouteDigit { .. } | ChromeCommand::ClipboardNavKey { .. }
            | ChromeCommand::ClipboardSearchBackspace | ChromeCommand::ClipboardAppendSearch(_)
            | ChromeCommand::ClipboardActivateSearch | ChromeCommand::ClipboardKeyboardFocus)
        {
            self.history_idle.activity(Instant::now());
        }
        if task.command()==ChromeCommand::ClipboardHandleEscape
            && crate::favorite_drag::cancel_active(window,cx) {
            self.native_suppress_click=Some(Instant::now()+Duration::from_millis(300));
            return;
        }
        if self.process_editor.is_some() || self.process_open_pending {
            if task.command() == ChromeCommand::ClipboardHandleEscape {
                if let Some((editor, _)) = &self.process_editor {
                    editor.update(cx, |editor, cx| editor.escape(window, cx));
                } else {
                    self.close_process_editor(window, cx);
                }
                return;
            }
            if matches!(
                task.command(),
                ChromeCommand::RouteDigit { .. }
                    | ChromeCommand::ClipboardNavKey { .. }
                    | ChromeCommand::ClipboardSwitchTab { .. }
                    | ChromeCommand::ClipboardSearchBackspace
                    | ChromeCommand::ClipboardKeyboardFocus
                    | ChromeCommand::ClipboardActivateSearch
                    | ChromeCommand::ClipboardAppendSearch(_)
            ) {
                return;
            }
        }
        if self.history_dialog {
            if task.command() == ChromeCommand::ClipboardHandleEscape {
                if let Some(actions) = &self.history_actions {
                    actions.update(cx, |actions, cx| actions.handle_escape(window, cx));
                }
                return;
            }
            if matches!(
                task.command(),
                ChromeCommand::RouteDigit { .. }
                    | ChromeCommand::ClipboardNavKey { .. }
                    | ChromeCommand::ClipboardSwitchTab { .. }
                    | ChromeCommand::ClipboardSearchBackspace
                    | ChromeCommand::ClipboardKeyboardFocus
                    | ChromeCommand::ClipboardActivateSearch
                    | ChromeCommand::ClipboardAppendSearch(_)
            ) {
                return;
            }
        }
        if matches!(task.command(), ChromeCommand::ClipboardSwitchTab { .. }) {
            let composing = if self.favorites_active {
                self.favorites.as_ref().is_some_and(|favorites| favorites.read(cx).is_composing(cx))
            } else {
                self.search.read(cx).is_composing()
            };
            if self.fifo_panel.is_none()
                && self.list_settings.is_none()
                && self.filter_panel.is_none()
                && self.tag_editor.is_none()
                && !composing
            {
                self.set_clipboard_route(
                    crate::history_keyboard::primary_tab_route(self.favorites_active), window, cx,
                );
            }
            return;
        }
        if self.favorites_active {
            if let Some(favorites) = &self.favorites {
                if favorites.update(cx, |favorites, cx| {
                    favorites.handle_chrome_input(task, window, cx)
                }) {
                    return;
                }
            }
            // A retired favorites key cannot fall through into hidden history.
            if matches!(
                task.command(),
                ChromeCommand::RouteDigit { .. }
                    | ChromeCommand::ClipboardNavKey { .. }
                    | ChromeCommand::ClipboardSwitchTab { .. }
                    | ChromeCommand::ClipboardSearchBackspace
                    | ChromeCommand::ClipboardKeyboardFocus
                    | ChromeCommand::ClipboardActivateSearch
                    | ChromeCommand::ClipboardAppendSearch(_)
                    | ChromeCommand::ClipboardHandleEscape
            ) {
                return;
            }
        }
        match task.command() {
            ChromeCommand::ShowQuickPaste => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) {
                        return;
                    }
                    let _guard = platform::guard_task(&host, task);
                    if let Err(error) = QuickPasteOverlay::show(
                        &mut this.quick_paste,
                        host,
                        this.tokens.clone(),
                        this.dark,
                        cx,
                    ) {
                        this.error = Some(error);
                        cx.notify();
                    }
                });
            }
            ChromeCommand::HideQuickPaste => {
                if let Some(overlay) = self.quick_paste {
                    QuickPasteOverlay::hide(overlay, cx);
                }
            }
            ChromeCommand::ShowToast => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) {
                        return;
                    }
                    let _guard = platform::guard_task(&host, task);
                    let (tokens, dark) = (this.tokens.clone(), this.dark);
                    if let Err(error) =
                        CopyToastOverlay::show(&mut this.toast, host, tokens, dark, cx)
                    {
                        this.error = Some(error);
                        cx.notify();
                    }
                });
            }
            ChromeCommand::HideToast => {
                if let Some(toast) = self.toast {
                    CopyToastOverlay::hide(toast, cx);
                }
            }
            ChromeCommand::DismissContextMenu(lease) => {
                if self
                    .fifo_panel
                    .as_ref()
                    .is_some_and(|(_, _, id)| *id == lease)
                {
                    self.close_fifo(cx);
                    return;
                }
                if self.menu_lease.as_ref().is_some_and(|(id, _)| *id == lease) || self.favorite_lease == Some(lease) {
                    self.close_menu(cx);
                }
                if let Some(panel) = self.panel_window {
                    PanelWindow::handle_chrome_input(panel, task, cx);
                }
            }
            ChromeCommand::ShowMain => {
                if self.exit_pending {
                    return;
                }
                let route = host.take_pending_nav("main");
                self.pending_main_route = Some((route, task));
                self.present_main_route(window, cx);
            }
            ChromeCommand::HideMain => {
                self.pending_main_route = None;
                if let Some(handle) = self.management_window.take() {
                    cx.defer(move |cx| ManagementWindow::close(handle, cx));
                }
                if let Some(handle) = self.settings_window {
                    cx.defer(move |cx| SettingsWindow::close(handle, cx));
                }
            }
            ChromeCommand::ShowSuperMenu => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) {
                        return;
                    }
                    let _permission = platform::guard_task(&host, task);
                    let hover = None;
                    #[cfg(windows)]
                    let hover = match &this.model {
                        UiModel::Live(service) => service.hover_service().ok(),
                        _ => hover,
                    };
                    if let Err(error) = SuperMenuWindow::show(
                        &mut this.super_menu,
                        host,
                        hover,
                        this.configuration["clipListImageHoverPreviewEnabled"].as_bool() != Some(false),
                        this.tokens.clone(),
                        this.dark,
                        cx,
                    ) {
                        this.error = Some(error);
                        cx.notify();
                    }
                    this.refresh_independent_surfaces(cx);
                });
            }
            ChromeCommand::HideSuperMenu => {
                if let Some(menu) = self.super_menu {
                    SuperMenuWindow::hide(menu, cx);
                }
            }
            ChromeCommand::ShowCeaBox => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) { return; }
                    let _permission = platform::guard_task(&host, task);
                    if let Err(error) = CeaBoxWindow::show(
                        &mut this.ceabox_window, host.clone(), this.tokens.clone(), this.dark, cx,
                    ) {
                        host.ceabox_hide();
                        this.error = Some(error);
                        cx.notify();
                    }
                });
            }
            ChromeCommand::HideCeaBox => {
                if let Some(ceabox) = self.ceabox_window { CeaBoxWindow::hide(ceabox, cx); }
            }
            ChromeCommand::ShowReopen => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) { return; }
                    let _permission = platform::guard_task(&host, task);
                    if let Err(error) = ReopenWindow::show(
                        &mut this.reopen_window, host.clone(), this.tokens.clone(), this.dark, cx,
                    ) {
                        host.reopen_hide();
                        this.error = Some(error);
                        cx.notify();
                    }
                });
            }
            ChromeCommand::HideReopen => {
                if let Some(reopen) = self.reopen_window { ReopenWindow::hide(reopen, cx); }
            }
            ChromeCommand::ShowFileBrowser => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) { return; }
                    let _permission = platform::guard_task(&host, task);
                    if let Err(error) = FileBrowserWindow::show(
                        &mut this.file_browser_window, host.clone(), this.tokens.clone(), this.dark, cx,
                    ) {
                        host.file_browser_hide();
                        this.error = Some(error);
                        cx.notify();
                    }
                });
            }
            ChromeCommand::HideFileBrowser => {
                if let Some(browser) = self.file_browser_window { FileBrowserWindow::hide(browser, cx); }
            }
            ChromeCommand::ShowPanel => {
                cx.defer_in(window, move |this, _, cx| {
                    if !host.prepare_chrome_task(task) {
                        return;
                    }
                    let _permission = platform::guard_task(&host, task);
                    if let Err(error) = PanelWindow::show(
                        &mut this.panel_window,
                        host,
                        this.tokens.clone(),
                        this.dark,
                        cx,
                    ) {
                        this.error = Some(error);
                        cx.notify();
                    }
                    if let (Some(panel), UiModel::Live(service)) = (this.panel_window, &this.model)
                    {
                        PanelWindow::bind_native_host(panel, service.clone(), cx);
                    }
                    this.refresh_independent_surfaces(cx);
                });
            }
            ChromeCommand::HidePanel => {
                if let Some(panel) = self.panel_window {
                    PanelWindow::hide(panel, cx);
                }
            }
            ChromeCommand::PanelBack
            | ChromeCommand::PanelRefresh
            | ChromeCommand::PanelSettings
            | ChromeCommand::PanelNavKey(_)
            | ChromeCommand::HostKeyboardFocus("panel")
            | ChromeCommand::BeginCaptionDrag("panel") => {
                if let Some(panel) = self.panel_window {
                    PanelWindow::handle_chrome_input(panel, task, cx);
                }
            }
            ChromeCommand::OpenQuickEdit => {
                if let Some(route) = host.take_toast_route() {
                    let id = route
                        .split_once('?')
                        .into_iter()
                        .flat_map(|(_, query)| query.split('&'))
                        .find_map(|pair| pair.strip_prefix("id="))
                        .unwrap_or_default()
                        .to_string();
                    self.open_editor(id, false, cx);
                }
            }
            ChromeCommand::OpenLog => {
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            let value = host.latest_log()?;
                            let path = value["path"]
                                .as_str()
                                .ok_or(tr("日志路径不可用", "Log path is unavailable"))?;
                            std::process::Command::new(if cfg!(windows) {
                                "explorer.exe"
                            } else {
                                "xdg-open"
                            })
                            .arg(path)
                            .spawn()
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if let Err(error) = result {
                            this.error = Some(error);
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            ChromeCommand::Exit => {
                log::warn!(target: "cliphost_gpui::lifecycle", "Exit command received from host (menu, API, RPC or restart)");
                self.request_exit(window, cx);
            }
            ChromeCommand::ClipboardKeyboardFocus | ChromeCommand::ClipboardActivateSearch => {
                self.focus_search(window, cx)
            }
            ChromeCommand::ClipboardAppendSearch(ch) => {
                self.query.push(ch);
                self.search.update(cx, |input, cx| {
                    input.set_text(self.query.clone(), window, cx)
                });
                self.refresh(cx);
            }
            ChromeCommand::ClipboardSearchBackspace => {
                use unicode_segmentation::UnicodeSegmentation;
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.search.update(cx, |input, cx| {
                    input.set_text(self.query.clone(), window, cx)
                });
                self.refresh(cx);
            }
            ChromeCommand::RouteDigit { digit, repeat } => {
                let number_paste = self.configuration["usingNumberKeyToPaste"]
                    .as_bool()
                    .unwrap_or(true);
                if !number_paste || !self.query.is_empty() {
                    self.query.push(char::from(b'0' + digit));
                    self.search.update(cx, |input, cx| {
                        input.set_text(self.query.clone(), window, cx)
                    });
                    self.refresh(cx);
                    return;
                }
                if repeat {
                    return;
                }
                let index = if digit == 0 { 9 } else { digit as usize - 1 };
                if index < self.page.rows.len() {
                    self.select_single(index);
                    self.execute_selected_for_task("paste", Some(task), cx);
                }
            }
            ChromeCommand::ClipboardNavKey { key, shift } => match key {
                "Enter" => self.execute_selected_for_task("paste", Some(task), cx),
                "Delete" => self.execute_selected("delete", cx),
                "Copy" => self.execute_selected_for_task("copy", Some(task), cx),
                "Pin" => self.execute_selected_for_task("pin", Some(task), cx),
                _ => {
                    if let Some(key) = crate::history_keyboard::Key::from_name(key) {
                        self.navigate_key(key, shift, window, cx);
                    }
                }
            },
            ChromeCommand::ClipboardHandleEscape => {
                if self.list_settings.is_some() {
                    if self.list_settings.as_ref().is_some_and(|form| form.read(cx).list_settings_can_close(cx)) {
                        self.close_list_settings(window, cx);
                    }
                    return;
                }
                if self.search.read(cx).is_composing() {
                    return;
                }
                host.clipboard_escape_unhandled(
                    Some(&task.escape_session_token()),
                    task.escape_input_token().as_deref(),
                );
            }
            _ => {}
        }
    }

    fn execute_selected(&mut self, operation: &'static str, cx: &mut Context<Self>) {
        self.execute_selected_for_task(operation, None, cx);
    }

    fn execute_selected_for_task(
        &mut self,
        operation: &'static str,
        task: Option<ChromeTask>,
        cx: &mut Context<Self>,
    ) {
        if self.favorites_active
            || self.fifo_panel.is_some()
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.list_settings.is_some()
            || self.filter_panel.is_some()
            || self.more_menu
            || self.tag_editor.is_some()
            || self.exit_pending
        {
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        if self.selected_id.is_empty()
            || !self
                .page
                .rows
                .iter()
                .any(|row| row.item.id == self.selected_id)
            || self.search.read(cx).is_composing()
            || self.page_generation != self.generation
            || (operation == "paste" && self.loading)
        {
            return;
        }
        if operation == "delete" {
            self.delete_history_items(self.selection(), cx);
            return;
        }
        let id = self.selected_id.clone();
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let reply_scope = (
            self.generation,
            self.native_session,
            self.action_reply_sequence,
            self.error.clone(),
        );
        let source = host.host_hwnd();
        let paste_request = if operation == "paste" {
            match host.capture_native_paste(task) {
                Ok(request) => Some(request),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let clipboard_request = if operation == "copy" {
            match host.capture_native_clipboard_action(task) {
                Ok(request) => Some(request),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let ids = self.selection();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let params = json!({"id": id, "ids": ids, "sourceHandle": source});
                    match operation {
                        "paste" if ids.len() > 1 => host.paste_native_items(
                            &json!({ "ids": ids, "mode": "selection", "sourceHandle": source }),
                            &paste_request.expect("captured paste request"), false,
                        ),
                        "paste" => host
                            .paste_native(&params, &paste_request.expect("captured paste request")),
                        "copy" => host.copy_items_native(
                            &params,
                            &clipboard_request.expect("captured clipboard request"),
                        ),
                        "pin" => each_id(&ids, |id| host.toggle_pin(&json!({ "id": id }))),
                        _ => Err("Unknown operation".into()),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.exit_pending
                    || this.favorites_active
                    || reply_scope
                        != (
                            this.generation,
                            this.native_session,
                            this.action_reply_sequence,
                            this.error.clone(),
                        )
                {
                    return;
                }
                match result {
                    Ok(_) => {
                        this.error = None;
                        if operation == "pin" {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn select_single(&mut self, index: usize) {
        if let Some(row) = self.page.rows.get(index) {
            self.selected_id = row.item.id.clone();
            self.selected_ids = vec![self.selected_id.clone()];
            self.anchor = Some(index);
        }
    }

    fn click_select(&mut self, index: usize, toggle: bool, range: bool) {
        let rows: Vec<&str> = self
            .page
            .rows
            .iter()
            .map(|row| row.item.id.as_str())
            .collect();
        let mut state = (
            std::mem::take(&mut self.selected_id),
            std::mem::take(&mut self.selected_ids),
            self.anchor,
        );
        click_selection(&rows, &mut state, index, toggle, range);
        (self.selected_id, self.selected_ids, self.anchor) = state;
    }

    fn selection(&self) -> Vec<String> {
        if self.selected_ids.is_empty() && !self.selected_id.is_empty() {
            vec![self.selected_id.clone()]
        } else {
            self.selected_ids.clone()
        }
    }

    fn selection_entries(&self, ids: &[String]) -> Vec<MenuEntry> {
        let items: Vec<_> = ids.iter()
            .filter_map(|id| self.page.rows.iter().find(|row| row.item.id == *id))
            .map(|row| &row.item).collect();
        history_context_menu::entries(&items, ids.len(), self.pin_overlay)
    }

    fn open_row_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(id) = self.page.rows.get(index).map(|row| row.item.id.clone()) else {
            return;
        };
        if !self.selected_ids.contains(&id) {
            self.select_single(index);
        }
        let ids = self.selection();
        let entries = self.selection_entries(&ids);
        cx.notify();
        self.open_menu(ids, entries, cx);
    }

    /// Acquire the core menu lease first: it retires queued list input and
    /// lets the low-level hook close the popup on an outside click or Escape.
    fn open_menu(&mut self, ids: Vec<String>, entries: Vec<MenuEntry>, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        if self.history_dialog || self.search.read(cx).is_composing() {
            return;
        }
        let Some(point) = platform::cursor_position() else {
            return;
        };
        self.close_menu(cx);
        let lease = crate::menu::next_lease();
        if !host.acquire_native_context_menu(lease, "clipboard") {
            return;
        }
        self.cancel_native_hover();
        #[cfg(windows)]
        if let Some(native) = self.interactions.borrow().as_ref() {
            let _ = native.set_menu_open(true);
        }
        self.menu_anchor = Some(point);
        let history_context = (!ids.is_empty()).then(|| {
            let items: Vec<_> = ids.iter()
                .filter_map(|id| self.page.rows.iter().find(|row| row.item.id == *id))
                .map(|row| &row.item).collect();
            history_context_menu::context(&items, ids.len())
        });
        // This is a local in-memory discovery snapshot; opening the menu does
        // not trigger discovery, pairing, filesystem reads or network work.
        let peers = history_context.as_ref().map(|_| history_context_menu::peers(&host.lan_peers())).unwrap_or_default();
        self.menu_lease = Some((lease, ids));
        self.refresh_pin_overlay(cx);
        let view = cx.weak_entity();
        let on_select: SelectHandler = Rc::new(move |lease, key, cx| {
            let _ = view.update(cx, |this, cx| this.menu_selected(lease, MenuSelection::Action(key), cx));
        });
        let view = cx.weak_entity();
        let history_on_select: HistorySelectHandler = Rc::new(move |lease, selection, cx| {
            let _ = view.update(cx, |this, cx| this.menu_selected(lease, selection, cx));
        });
        let (tokens, dark) = (self.tokens.clone(), self.dark);
        let view = cx.weak_entity();
        // Opening a window must leave the current mouse event dispatch.
        cx.defer(move |cx| {
            let _ = view.update(cx, |this, cx| {
                if this.menu_lease.as_ref().map(|(id, _)| *id) != Some(lease)
                    || !host.native_context_menu_is_current(lease)
                {
                    return;
                }
                let mut menu = this.menu;
                let result = if let Some(context) = history_context {
                    ContextMenuPopup::show_history(
                        &mut menu, &host, host.host_hwnd(), lease, point,
                        entries, context, peers, tokens, dark, history_on_select, cx,
                    )
                } else { ContextMenuPopup::show(
                    &mut menu,
                    &host,
                    host.host_hwnd(),
                    lease,
                    point,
                    entries,
                    tokens,
                    dark,
                    on_select,
                    cx,
                ) };
                this.menu = menu;
                if let Err(error) = result {
                    host.release_native_context_menu(lease);
                    this.menu_lease = None;
                    this.error = Some(error);
                    cx.notify();
                }
            });
        });
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.cancel_favorite_hover();
        self.favorite_trigger_id = None;
        if let Some(lease) = self.favorite_lease.take() {
            if let Some(host) = self.model.host() { host.release_native_context_menu(lease); }
        }
        if let Some(popup) = self.favorite_popup {
            crate::favorite_save::FavoriteSavePopup::hide(popup, cx);
        }
        self.menu_anchor = None;
        #[cfg(windows)]
        if let Some(native) = self.interactions.borrow().as_ref() {
            let _ = native.set_menu_open(false);
        }
        if let Some((lease, _)) = self.menu_lease.take() {
            if let Some(host) = self.model.host() {
                host.release_native_context_menu(lease);
            }
        }
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
        cx.notify();
    }

    fn menu_selected(&mut self, lease: u64, selection: MenuSelection, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        let Some((_, ids)) = self.menu_lease.take_if(|(id, _)| *id == lease) else {
            return;
        };
        self.menu_anchor = None;
        #[cfg(windows)]
        if let Some(native) = self.interactions.borrow().as_ref() {
            let _ = native.set_menu_open(false);
        }
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
        // A choice only counts while the core still holds this lease.
        if host.release_native_context_menu(lease) {
            match selection {
                MenuSelection::Action(key) => self.run_menu_action(key, ids, cx),
                MenuSelection::LanDevice(device_id) => {
                    let count = ids.len();
                    self.send_history_to_lan_target(ids, count, format!("dm:{device_id}"), cx);
                }
                MenuSelection::Dismiss => {}
            }
        }
        cx.notify();
    }

    fn run_menu_action(&mut self, key: &'static str, ids: Vec<String>, cx: &mut Context<Self>) {
        if self.exit_pending {
            return;
        }
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let Some(host) = self.model.host() else {
            return;
        };
        let first = ids.first().cloned().unwrap_or_default();
        let count = ids.len();
        let clipboard_request = if matches!(
            key,
            "copy-only" | "copy-as-gif" | "pin-snipaste" | "pin-pixpin"
        ) {
            match host.capture_native_clipboard_action(None) {
                Ok(request) => Some(request),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        match key {
            "add-text" => self.open_history_dialog(HistoryDialog::AddText, cx),
            "clear-history" => self.open_history_dialog(HistoryDialog::Clear(None), cx),
            "edit" => self.open_editor(first, false, cx),
            "fifo-paste" => self.open_fifo(ids, cx),
            "scroll-top" => {
                self.history_idle.activity(Instant::now());
                self.return_history_to_top(cx);
            }
            "paste" | "reverse-paste" | "paste-plain-text" | "paste-standard-text" => {
                let request = match host.capture_native_paste(None) {
                    Ok(request) => request,
                    Err(error) => {
                        self.error = Some(error);
                        cx.notify();
                        return;
                    }
                };
                let mode = if key == "reverse-paste" {
                    "descending"
                } else {
                    "selection"
                };
                let plain = key.starts_with("paste-");
                self.run_host(false, None, cx, move |host| {
                    host.paste_native_items(&json!({ "ids": ids, "mode": mode }), &request, plain)
                });
            }
            "copy-only" => self.run_host(false, None, cx, move |host| {
                host.copy_items_native(
                    &json!({ "ids": ids }),
                    &clipboard_request.expect("captured clipboard request"),
                )
            }),
            "favorite" => {
                self.open_favorite_save(ids, cx);
            }
            "pin-toggle" => self.run_host(true, None, cx, move |host| {
                each_id(&ids, |id| host.toggle_pin(&json!({ "id": id })))
            }),
            "delete" => self.delete_history_items(ids, cx),
            "reveal-file" | "reveal-folder" => {
                let request = match host.capture_native_reveal() {
                    Ok(request) => request,
                    Err(error) => {
                        self.error = Some(error);
                        cx.notify();
                        return;
                    }
                };
                let target = if key == "reveal-file" {
                    "file"
                } else {
                    "folder"
                };
                self.run_host(false, None, cx, move |host| {
                    host.reveal_item_native(&json!({ "id": first, "target": target }), &request)
                });
            }
            "lan-share" => {
                self.send_history_to_lan(ids, count, cx);
            }
            "copy-as-gif" => self.run_host(false, None, cx, move |host| {
                host.copy_gif_native(
                    &json!({ "id": first }),
                    &clipboard_request.expect("captured clipboard request"),
                )
            }),
            "split-lines" => self.run_host(true, None, cx, move |host| {
                host.split_lines(&json!({ "id": first }))
            }),
            "split-files" => self.run_host(true, None, cx, move |host| {
                host.split_files(&json!({ "id": first }))
            }),
            "pin-snipaste" | "pin-pixpin" => {
                let provider = if key == "pin-snipaste" {
                    "snipaste"
                } else {
                    "pixpin"
                };
                self.run_host(false, None, cx, move |host| {
                    host.pin_overlay_native(
                        &json!({ "ids": ids, "provider": provider }),
                        &clipboard_request.expect("captured clipboard request"),
                    )
                });
            }
            "distinct" => self.run_host(true, None, cx, move |host| {
                host.distinct(&json!({ "scope": "all" }))
            }),
            _ => {}
        }
    }

    /// Run a core operation off the UI thread; success may refresh the list
    /// or show a short footer notice, failure stays in the error line.
    fn send_history_to_lan(&mut self, ids: Vec<String>, count: usize, cx: &mut Context<Self>) {
        self.send_history_to_lan_target(ids, count, "all".into(), cx);
    }

    fn send_history_to_lan_target(&mut self, ids: Vec<String>, count: usize, conversation: String, cx: &mut Context<Self>) {
        if self.exit_pending { return; }
        if ids.iter().collect::<std::collections::HashSet<_>>().len() > 100 {
            self.error = Some(tr("一次最多发送 100 项，请缩小选择范围。", "Send up to 100 items at a time. Select fewer items.").into());
            cx.notify();
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        // Register before spawning/awaiting so normal Exit also observes work
        // accepted from the history menu when no LAN page has ever been opened.
        let (_, wait) = crate::management::SharedHistorySend::accept_to(&host, ids, conversation);
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let scope = (
            self.generation,
            self.native_session,
            self.action_reply_sequence,
            self.error.clone(),
        );
        cx.spawn(async move |this, cx| {
            let result = wait.await.unwrap_or_else(|_| {
                Err(tr("发送等待已中断。", "Waiting for the send was interrupted.").into())
            });
            let _ = this.update(cx, |this, cx| {
                if this.exit_pending
                    || this.favorites_active
                    || scope
                        != (
                            this.generation,
                            this.native_session,
                            this.action_reply_sequence,
                            this.error.clone(),
                        )
                {
                    return;
                }
                match result {
                    Ok(_) => {
                        this.error = None;
                        this.show_notice(ActionNotice::LanQueue(count).text(), cx);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Delete on a worker, but remove each confirmed row before rebuilding the
    /// loaded range. Query/session changes cannot discard a durable deletion.
    fn delete_history_items(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else { return; };
        let first_write = self.history_delete_pending.is_empty();
        let ids: Vec<_> = ids.into_iter()
            .filter(|id| !id.is_empty() && self.history_delete_pending.insert(id.clone()))
            .collect();
        if ids.is_empty() { return; }
        let current = self.page_generation == self.generation && !self.loading;
        self.history_delete_range_valid = current && (first_write || self.history_delete_range_valid);
        if current {
            // Cancel offset reads BEFORE the write. A tail fetched with the old
            // skip after deletion would otherwise silently leave a page gap.
            self.retire_history_query(cx);
            self.page_generation = self.generation;
            self.content_cache.advance_retaining(self.page_generation, |_| true);
            self.content_cache.release_evicted(cx);
        }
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let sequence = self.action_reply_sequence;
        let previous_error = self.error.clone();
        let worker_ids = ids.clone();
        cx.spawn(async move |this, cx| {
            let (removed, error) = cx.background_executor().spawn(async move {
                confirmed_deletions(&worker_ids, |id| host.delete_item(&json!({"id": id})))
            }).await;
            let _ = this.update(cx, |this, cx| {
                for id in ids { this.history_delete_pending.remove(&id); }
                if this.exit_pending { return; }
                let show_reply = !this.favorites_active
                    && this.action_reply_sequence == sequence && this.error == previous_error;
                this.apply_confirmed_history_removals(&removed, true, cx);
                if show_reply { this.error = error; }
                if this.history_refill_target.is_some() { this.load_more(cx); }
                cx.notify();
            });
        }).detach();
    }

    fn apply_confirmed_history_removals(
        &mut self,
        ids: &HashSet<String>,
        local: bool,
        cx: &mut Context<Self>,
    ) {
        let removed = self.history_removals.confirm(ids);
        if removed.is_empty() { return; }
        let page_was_current = self.page_generation == self.generation;
        let old: Vec<String> = self.page.rows.iter().map(|row| row.item.id.clone()).collect();
        let old_ids: Vec<&str> = old.iter().map(String::as_str).collect();
        let plan = crate::history_removal::plan_removal(
            &self.page, &removed,
            local && self.history_delete_range_valid && page_was_current && !self.loading,
            self.history_refill_target,
        );
        let scroll = self.list.logical_scroll_top();
        crate::history_removal::remove_from_page(&mut self.page, &removed);
        let new_ids: Vec<&str> = self.page.rows.iter().map(|row| row.item.id.as_str()).collect();
        let scroll = crate::history_scroll::refreshed_anchor(&old_ids, &new_ids, scroll);
        self.anchor = self.anchor.and_then(|index| old_ids.get(index))
            .and_then(|id| new_ids.iter().position(|current| current == id));
        self.selected_id = crate::history_scroll::refreshed_selection(&old_ids, &new_ids, &self.selected_id)
            .and_then(|index| new_ids.get(index)).copied().unwrap_or_default().to_owned();
        self.selected_ids.retain(|id| !removed.contains(id));
        if self.selected_ids.is_empty() {
            if !self.selected_id.is_empty() { self.selected_ids.push(self.selected_id.clone()); }
            self.anchor = None;
        } else if !self.selected_ids.contains(&self.selected_id) {
            self.selected_id = self.selected_ids[0].clone();
        }
        self.history_scrollbar.cancel();
        // IDs suffice: deletion cannot change surviving rows' content/heights.
        crate::list_reconcile::reconcile(&self.list, &old_ids, &new_ids);
        self.list.scroll_to(scroll);
        // Retire stale input and queries before they can restore removed rows.
        if let crate::history_removal::RemovalPlan::Refill { target } = plan {
            self.retire_history_query(cx);
            self.page_generation = self.generation;
            self.loading = false;
            self.background_refresh = false;
            let target = target.min(self.page.total);
            self.history_refill_target = (target > self.page.rows.len()).then_some(target);
        } else {
            // Unknown IDs/filter changes need authoritative counts/order, but
            // confirmed rows have already disappeared from the visible page.
            self.refresh_with_delay(Duration::ZERO, cx);
        }
        if page_was_current {
            // Surviving rows still belong to this query. Keep subsequent delete
            // gestures usable while the authoritative range is being reloaded.
            self.page_generation = self.generation;
        }
        self.content_cache.advance_retaining(self.page_generation, |key| !removed.contains(&key.item_id));
        self.content_cache.release_evicted(cx);
        if self.history_refill_target.is_some() { self.load_more(cx); }
        self.sync_history_top_button(cx);
        cx.notify();
    }

    fn run_host(
        &mut self,
        refresh: bool,
        notice: Option<ActionNotice>,
        cx: &mut Context<Self>,
        job: impl FnOnce(&HostState) -> Result<Value, String> + Send + 'static,
    ) {
        let Some(host) = self.model.host() else {
            return;
        };
        self.action_reply_sequence = self.action_reply_sequence.wrapping_add(1);
        let reply_scope = (
            self.generation,
            self.native_session,
            self.action_reply_sequence,
            self.error.clone(),
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { job(&host) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.exit_pending
                    || this.favorites_active
                    || reply_scope
                        != (
                            this.generation,
                            this.native_session,
                            this.action_reply_sequence,
                            this.error.clone(),
                        )
                {
                    return;
                }
                match result {
                    Ok(_) => {
                        this.error = None;
                        if let Some(notice) = notice {
                            this.show_notice(notice.text(), cx);
                        }
                        if refresh {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn show_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        self.favorite_feedback_generation = self.favorite_feedback_generation.wrapping_add(1);
        let generation = self.favorite_feedback_generation;
        self.favorite_save_undo = None;
        self.favorite_feedback = None;
        self.notice = Some(notice);
        self.notice_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(2_500)).await;
            let _ = this.update(cx, |this, cx| {
                if this.favorite_feedback_generation != generation { return; }
                this.notice = None;
                this.favorite_feedback = None;
                cx.notify();
            });
        }));
    }

    fn refresh_pin_overlay(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { host.pin_overlay_status() })
                .await;
            let _ = this.update(cx, |this, _| {
                this.pin_overlay = (
                    status["snipasteAvailable"].as_bool().unwrap_or(false),
                    status["pixPinAvailable"].as_bool().unwrap_or(false),
                );
            });
        })
        .detach();
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.history_scrollbar.cancel();
        if let Some(host) = self.model.host() {
            host.window_hide();
        } else {
            window.remove_window();
        }
    }

    fn open_file(&mut self, id: String, path: String, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        if self.favorites_active
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.exit_pending
            || self.loading
            || self.native_dragging
            || self
                .native_suppress_click
                .is_some_and(|until| Instant::now() < until)
            || self.page_generation != self.generation
            || !self.page.rows.iter().any(|row| row.item.id == id)
        {
            return;
        }
        let request = match host.capture_native_clipboard_action(None) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.run_host(false, None, cx, move |host| {
            host.open_item_path_native(&json!({"id":id, "path":path}), &request)
        });
    }

    fn window_action(&mut self, action: &str, key: &str, cx: &mut Context<Self>) {
        if let Some(host) = self.model.host() {
            let value = !self
                .window_state
                .get(key)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.window_state = host.apply_window_action(action, value);
            cx.notify();
        }
    }

    fn navigate(
        &mut self,
        offset: isize,
        absolute: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = match (absolute, offset < 0) {
            (true, true) => crate::history_keyboard::Key::Home,
            (true, false) => crate::history_keyboard::Key::End,
            (false, true) => crate::history_keyboard::Key::Up,
            (false, false) => crate::history_keyboard::Key::Down,
        };
        self.navigate_key(key, false, window, cx);
    }

    fn navigate_key(
        &mut self,
        key: crate::history_keyboard::Key,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.favorites_active
            || self.fifo_panel.is_some()
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.list_settings.is_some()
            || self.filter_panel.is_some()
            || self.more_menu
            || self.tag_editor.is_some()
        {
            return;
        }
        if self.search.read(cx).is_composing() || self.exit_pending {
            return;
        }
        if key == crate::history_keyboard::Key::SelectAll
            && self.page_generation != self.generation
        {
            return;
        }
        self.history_idle.activity(Instant::now());
        let rows: Vec<&str> = self
            .page
            .rows
            .iter()
            .map(|row| row.item.id.as_str())
            .collect();
        let state = crate::history_keyboard::SelectionState {
            active: rows.iter().position(|id| *id == self.selected_id),
            selected_ids: self.selected_ids.clone(),
            anchor: self.anchor,
        };
        match crate::history_keyboard::transition(&rows, &state, key, shift) {
            crate::history_keyboard::Transition::Unchanged => return,
            crate::history_keyboard::Transition::Category(direction) => {
                let current = self.tabs.iter().position(|tab| tab.key == self.category);
                if let Some(index) = crate::history_keyboard::category_index(
                    self.tabs.len(), current, direction,
                ) {
                    self.category = self.tabs[index].key.clone();
                    self.refresh(cx);
                }
            }
            crate::history_keyboard::Transition::Selection { state, reveal } => {
                self.selected_id = state.active
                    .and_then(|index| self.page.rows.get(index))
                    .map(|row| row.item.id.clone())
                    .unwrap_or_default();
                self.selected_ids = state.selected_ids;
                self.anchor = state.anchor;
                if let Some(index) = reveal {
                    crate::history_scroll::reveal(&self.list, index);
                } else if key == crate::history_keyboard::Key::Home {
                    // A mouse scroll need not change the active row.
                    crate::history_scroll::reveal(&self.list, 0);
                }
            }
        }
        self.focus.focus(window);
        self.sync_history_top_button(cx);
        cx.notify();
    }

    fn current_request(&self) -> ListRequest {
        let request = match self.tabs.iter().find(|tab| tab.key == self.category) {
            Some(tab) => tab.request(&self.advanced),
            None => ListRequest {
                advanced: self.advanced.clone(),
                ..ListRequest::category(&self.category)
            },
        };
        request.with_process_filter(self.process_selection.spec(&self.process_presets))
    }

    fn clipboard_tokens(&self) -> DesignTokens {
        if self.window_background.snapshot().is_some() {
            crate::window_glass::tokens(&self.tokens, self.window_background.options())
        } else {
            self.tokens.clone()
        }
    }

    fn popover_surface(&self) -> gpui::Hsla {
        let theme = self.tokens.theme(self.dark);
        if self.window_background.snapshot().is_some() {
            crate::window_glass::primary_strong(theme, self.window_background.options())
        } else {
            color(&theme.surface)
        }
    }

    fn sync_window_background(&mut self, cx: &mut Context<Self>) {
        self.window_background.configure(&self.configuration);
        self.refresh_clipboard_colors(cx);
        self.load_window_background(cx);
    }

    fn load_window_background(&mut self, cx: &mut Context<Self>) {
        // Keep the physical task occupied until its blocking read returns.
        // New config replaces only BackgroundState's pending request.
        if self.window_background_task.is_some() { return; }
        let (Some(host), Some(request)) = (self.model.host(), self.window_background.pending_request()) else { return; };
        self.window_background_task = Some(cx.spawn(async move |this, cx| {
            let reply = cx.background_executor().spawn(async move {
                crate::window_background::load_background(&host, request)
            }).await;
            let _ = this.update(cx, |this, cx| {
                this.window_background_task = None;
                if this.window_background.apply(reply, &this.configuration) {
                    this.refresh_clipboard_colors(cx);
                    cx.notify();
                }
                this.load_window_background(cx);
            });
        }));
    }

    fn refresh_clipboard_colors(&mut self, cx: &mut Context<Self>) {
        let tokens = self.clipboard_tokens();
        let dark = self.dark;
        let t = tokens.theme(dark);
        let colors = InputColors {
            background: color(&t.search_background), foreground: color(&t.text),
            placeholder: color(&t.text_muted), border: color(&t.border_secondary),
            selection: color(&t.tab_active), caret: color(&t.accent),
        };
        let search_colors = InputColors {
            background: gpui::transparent_black(),
            border: gpui::transparent_black(),
            placeholder: colors.placeholder.opacity(0.6),
            ..colors
        };
        self.search.update(cx, |input, cx| input.set_colors(search_colors, cx));
        if let Some((fields, _)) = &self.filter_panel {
            for input in fields { input.update(cx, |input, cx| input.set_colors(colors, cx)); }
        }
        if let Some((controls, _)) = &self.appearance_controls {
            controls.update(cx, |controls, cx| controls.set_design_tokens(tokens.clone(), dark, cx));
        }
        if let Some(form) = &self.list_settings {
            let enabled = self.window_background.snapshot().is_some();
            form.update(cx, |form, cx| form.set_window_background_theme(
                enabled, self.window_background.options().clone(), self.tokens.clone(), dark, cx,
            ));
        }
        if let Some((editor, _)) = &self.process_editor {
            editor.update(cx, |editor, cx| editor.set_design_tokens(tokens.clone(), dark, cx));
        }
        if let Some(favorites) = &self.favorites {
            let enabled = self.window_background.snapshot().is_some();
            let (hover, active) = crate::window_glass::tree_colors(
                self.tokens.theme(dark), self.window_background.options(),
            );
            favorites.update(cx, |favorites, cx| {
                favorites.set_window_background(enabled, tokens.clone(), dark, cx);
                favorites.set_window_background_tree_colors(hover, active, cx);
            });
        }
        if let Some((editor, _)) = &self.tag_editor {
            editor.update(cx, |editor, cx| editor.set_design_tokens(tokens.clone(), dark, cx));
        }
        if let Some(actions) = &self.history_actions {
            // These dialog surfaces use the base CSS variables; they do not
            // inherit the main list's explicit wallpaper glass overrides.
            actions.update(cx, |actions, cx| actions.set_design_tokens(self.tokens.clone(), dark, cx));
        }
        if let Some((panel, _, _)) = &self.fifo_panel {
            panel.update(cx, |panel, cx| panel.set_design_tokens(self.tokens.clone(), dark, cx));
        }
        self.refresh_independent_surfaces(cx);
    }

    fn refresh_independent_surfaces(&self, cx: &mut Context<Self>) {
        if let Some(line) = self.file_line.clone() {
            let (tokens, dark) = (self.tokens.clone(), self.dark);
            cx.defer(move |cx| line.update(cx, |line, cx| line.set_theme(tokens, dark, cx)));
        }
        let (settings, management, lan, editor, panel, menu, ceabox, reopen, browser) = (
            self.settings_window, self.management_window, self.lan_companion, self.editor_window,
            self.panel_window, self.super_menu, self.ceabox_window, self.reopen_window, self.file_browser_window,
        );
        if settings.is_none() && management.is_none() && lan.is_none() && editor.is_none()
            && panel.is_none() && menu.is_none() && ceabox.is_none() && reopen.is_none() && browser.is_none() { return; }
        let snapshot = self.window_background.snapshot();
        let options = self.window_background.options().clone();
        let tokens = self.tokens.clone();
        let dark = self.dark;
        cx.defer(move |cx| {
            if let Some(handle) = settings {
                let _ = handle.update(cx, |settings, _, cx| {
                    settings.set_window_background(snapshot.clone(), options.clone(), tokens.clone(), dark, cx);
                });
            }
            if let Some(handle) = management {
                let _ = handle.update(cx, |management, _, cx| {
                    management.set_window_background(snapshot.clone(), options.clone(), tokens.clone(), dark, cx);
                });
            }
            if let Some(handle) = lan {
                let _ = handle.update(cx, |lan, _, cx| {
                    lan.set_window_background(snapshot.clone(), options.clone(), tokens.clone(), dark, cx);
                });
            }
            if let Some(handle) = editor {
                let _ = handle.update(cx, |editor, _, cx| {
                    editor.set_window_background(snapshot.clone(), options.clone(), tokens.clone(), dark, cx);
                });
            }
            if let Some(handle) = panel {
                let _ = handle.update(cx, |panel, _, cx| {
                    panel.set_window_background(snapshot.clone(), options.clone(), tokens.clone(), dark, cx);
                });
            }
            if let Some(handle) = menu {
                let _ = handle.update(cx, |menu, _, cx| menu.set_design_tokens(tokens.clone(), dark, cx));
            }
            if let Some(handle) = ceabox {
                let _ = handle.update(cx, |ceabox, _, cx| ceabox.set_design_tokens(tokens.clone(), dark, cx));
            }
            if let Some(handle) = reopen {
                let _ = handle.update(cx, |reopen, _, cx| reopen.set_design_tokens(tokens.clone(), dark, cx));
            }
            if let Some(handle) = browser {
                let _ = handle.update(cx, |browser, _, cx| browser.set_design_tokens(tokens, dark, cx));
            }
        });
    }

    fn merge_authoritative_config(&mut self, value: &Value, cx: &mut Context<Self>) {
        self.configuration = self.appearance_writes.lock().unwrap().merge(value);
        self.sync_window_background(cx);
        if let Some((controls, _)) = &self.appearance_controls {
            controls.update(cx, |controls, cx| {
                controls.set_config(&self.configuration, cx)
            });
        }
        if let Some(favorites) = &self.favorites {
            favorites.update(cx, |favorites, cx| {
                favorites.set_config(&self.configuration, cx)
            });
        }
        self.sync_tabs();
        cx.notify();
    }

    fn save_appearance(&mut self, patch: Value, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        let queue = self.appearance_writes.clone();
        let start = {
            let mut writes = queue.lock().unwrap();
            writes.accept(patch);
            self.configuration = writes.merge(&self.configuration);
            if writes.running {
                false
            } else {
                writes.running = true;
                true
            }
        };
        if let Some((controls, _)) = &self.appearance_controls {
            controls.update(cx, |controls, cx| {
                controls.set_config(&self.configuration, cx)
            });
        }
        if let Some(favorites) = &self.favorites {
            favorites.update(cx, |favorites, cx| {
                favorites.set_config(&self.configuration, cx)
            });
        }
        // Changing row geometry invalidates virtual-list measurements without
        // changing the query, selection order or ID ownership.
        self.history_scrollbar.cancel();
        crate::history_scroll::remeasure(&self.list);
        cx.notify();
        if !start {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                let write = {
                    let mut writes = queue.lock().unwrap();
                    match writes.pending.pop_front() {
                        Some(write) => write,
                        None => {
                            writes.running = false;
                            break;
                        }
                    }
                };
                let worker = host.clone();
                let patch = write.patch.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { worker.config_update(&patch) })
                    .await;
                {
                    let mut writes = queue.lock().unwrap();
                    if let Err(error) = &result {
                        writes.failure = Some(error.clone());
                    }
                    writes.finish(&write, result.is_ok());
                }
                let _ = this.update(cx, |this, cx| {
                    // An event/response may have been assembled before another
                    // writer completed; get current authority and retain queued or
                    // rejected local fields through the per-field revision merge.
                    this.merge_authoritative_config(&host.config_get(), cx);
                    if let Err(error) = result {
                        this.error = Some(error);
                    }
                    this.history_scrollbar.cancel();
                    crate::history_scroll::remeasure(&this.list);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_process_presets(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        if self.process_loading {
            if open {
                self.process_open_pending = true;
                self.sync_overlay_guard();
                cx.notify();
            }
            return;
        }
        self.process_generation = self.process_generation.wrapping_add(1);
        let generation = self.process_generation;
        self.process_loading = true;
        self.process_reload_requested = false;
        self.process_open_pending = open;
        self.sync_overlay_guard();
        let presentation = host.native_clipboard_presentation_stamp();
        self.process_presentation = open.then_some((presentation.0, presentation.1));
        let gate = self.process_write_gate.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = host.clone();
            let read_gate = gate.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _serial = read_gate
                        .lock()
                        .map_err(|_| "Process preset write queue is unavailable.".to_string())?;
                    PresetSnapshot::from_value(worker.process_filter_presets_get()?)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.process_generation != generation {
                    return;
                }
                this.process_loading = false;
                let open = this.process_open_pending;
                this.process_open_pending = false;
                match result {
                    Ok(snapshot) => {
                        this.process_loaded = true;
                        this.process_initialized = snapshot.initialized;
                        this.process_presets = snapshot.presets.clone();
                        this.process_selection.reconcile(&this.process_presets);
                        this.refresh(cx);
                        if open
                            && !this.favorites_active
                            && !this.exit_pending
                            && host.native_clipboard_presentation_stamp() == presentation
                            && host.hide_snapshot().visible
                            && !host.hide_snapshot().is_collapsed
                        {
                            let writer = preset_writer(host.clone(), &snapshot, gate);
                            let entity = cx.new(|cx| {
                                ProcessFilterEditor::new(
                                    host.clone(),
                                    this.clipboard_tokens(),
                                    this.dark,
                                    snapshot.presets,
                                    this.process_selection.clone(),
                                    this.process_edit_target.take(),
                                    writer,
                                    window,
                                    cx,
                                )
                            });
                            let subscription =
                                cx.subscribe_in(&entity, window, |this, _, event, window, cx| {
                                    match event {
                                        ProcessFiltersEvent::Changed { presets, selection } => {
                                            this.process_presets = presets.clone();
                                            this.process_selection = selection.clone();
                                            this.process_initialized = true;
                                            this.refresh(cx);
                                        }
                                        ProcessFiltersEvent::Saved
                                        | ProcessFiltersEvent::Cancelled => {
                                            this.close_process_editor(window, cx);
                                        }
                                    }
                                    cx.notify();
                                });
                            // Reopen exposes the latest retained accepted write,
                            // including Add/rename/delete intent and its original
                            // CAS writer. Restoration is read-only until Retry.
                            if let Some(failure) =
                                crate::process_filters::failed_process_filter_writes()
                                    .into_iter()
                                    .max_by_key(|failure| failure.write_id)
                            {
                                if let Err(error) = entity.update(cx, |editor, cx| {
                                    editor.restore_failed_write(failure.write_id, window, cx)
                                }) {
                                    this.error = Some(error);
                                }
                            }
                            this.process_icon_applied.clear();
                            this.process_editor = Some((entity, subscription));
                            this.filter_popover_generation =
                                this.filter_popover_generation.wrapping_add(1);
                            this.sync_overlay_guard();
                            this.engage_process_keyboard(window, cx);
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                this.sync_overlay_guard();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_process_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_process_editor_with(None, window, cx);
    }

    /// Right-click on a process chip: edit or delete that preset.
    fn edit_process_preset(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_process_editor_with(Some(id), window, cx);
    }

    fn open_process_editor_with(
        &mut self,
        editing: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.favorites_active || self.exit_pending || self.history_dialog {
            return;
        }
        self.close_process_editor(window, cx);
        self.process_edit_target = editing;
        self.close_menu(cx);
        self.close_fifo(cx);
        self.list_settings = None;
        self.filter_panel = None;
        self.tag_editor = None;
        self.load_process_presets(true, window, cx);
    }

    /// Retire only UI callbacks. The CAS worker owns accepted persistence and
    /// keeps running; reopening GET waits on its serial gate before capturing
    /// a new revision. Ordinary cancellation does not save a draft.
    fn retire_process_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.process_editor.is_some() || self.process_open_pending;
        self.process_generation = self.process_generation.wrapping_add(1);
        self.process_loading = false;
        self.process_open_pending = false;
        self.process_presentation = None;
        self.process_icon_applied.clear();
        if was_open {
            self.process_reload_requested = true;
            if let Some(host) = self.model.host() {
                host.apply_window_action(
                    "setSpaModalOpen",
                    self.history_dialog || crate::editor::has_active_input_windows(),
                );
            }
        }
        if let Some((editor, _)) = self.process_editor.take() {
            editor.update(cx, |editor, cx| editor.invalidate(cx));
        }
        self.sync_overlay_guard();
        if was_open {
            cx.notify();
        }
        was_open
    }

    fn close_process_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.retire_process_editor(cx) {
            window.blur();
            self.focus.focus(window);
            self.restore_no_focus(window);
        }
    }

    fn engage_process_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.process_generation;
        cx.defer_in(window, move |this, window, cx| {
            if generation != this.process_generation {
                return;
            }
            let Some((editor, _)) = &this.process_editor else {
                return;
            };
            // Preserve the input selected by the original mouse-down, including
            // rename and IME. Activation is deferred past GPUI's mouse borrow.
            let handles = editor.read(cx).input_focus_handles(cx);
            let focused = handles
                .iter()
                .find(|handle| handle.is_focused(window))
                .cloned();
            if let Some(host) = this.model.host() {
                if let Err(error) = NativeWindow::request_search_focus(window, &host) {
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
            }
            if let Some(focused) = focused {
                focused.focus(window);
            }
        });
    }

    fn process_editor_view(
        &self,
        editor: Option<Entity<ProcessFilterEditor>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = self.tokens.theme(self.dark);
        let content = div()
            .size_full()
            .min_h_0()
            .capture_any_mouse_down(cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                if event.button == MouseButton::Left {
                    this.engage_process_keyboard(window, cx);
                }
            }))
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .when(editor.is_none(), |content| {
                content.child(
                    div().p(px(14.0)).text_size(px(12.0)).text_color(color(&t.text_muted))
                        .child(tr("加载中…", "Loading…")),
                )
            })
            .children(editor)
            .into_any_element();
        self.filter_popover_shell(
            "process-filter",
            crate::process_filters::PREFERRED_HEIGHT,
            content,
            |this, window, cx| this.close_process_editor(window, cx),
            window,
            cx,
        )
    }

    fn fill_source_icons(&mut self, window: &Window, cx: &mut Context<Self>) {
        let pixels = source_icon_pixels(window.scale_factor());
        if pixels != self.source_icon_pixels {
            // Monitor DPI changed: re-extract at the new device size. The
            // editor keeps its applied icons until it is reopened.
            self.source_icon_pixels = pixels;
            self.source_icons.clear();
            self.source_icon_order.clear();
        }
        // The catalog loads after the editor. Reuse cached icons once per
        // editor lifetime, without invalidating its layout on every poll.
        if let Some((editor, _)) = &self.process_editor {
            let ready: Vec<_> = editor
                .read(cx)
                .catalog()
                .iter()
                .filter(|source| !self.process_icon_applied.contains(&source.exe_name))
                .filter_map(|source| {
                    self.source_icons
                        .get(&source.exe_name)
                        .and_then(Clone::clone)
                        .map(|icon| (source.exe_name.clone(), icon))
                })
                .collect();
            for (name, icon) in ready {
                self.process_icon_applied.insert(name.clone());
                editor.update(cx, |editor, cx| editor.set_source_icon(name, icon, cx));
            }
        }
        if self.source_icon_pending.len() >= 4 {
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        let mut names = Vec::new();
        if AppearanceSettings::from_config(&self.configuration)
            .resolved()
            .show_process
        {
            let viewport = self.list.viewport_bounds();
            // Scan only the viewport, as content resources do. Filtering all
            // loaded rows would still seek through the entire lazy-list tree
            // each maintenance tick, even when fewer than 32 rows are visible.
            let top = self.list.logical_scroll_top().item_ix;
            let mut visible: Vec<_> = (top..self.page.rows.len())
                .map_while(|index| self.list.bounds_for_item(index)
                    .filter(|bounds| bounds.top() < viewport.bottom())
                    .map(|bounds| (index, bounds)))
                .filter(|(_, bounds)| bounds.bottom() > viewport.top())
                .map(|(index, _)| index)
                .take(32)
                .collect();
            if visible.is_empty() {
                // First layout or a re-measure: prefetch at the current anchor,
                // never the first twelve history rows after scrolling away.
                visible = (top..self.page.rows.len()).take(12).collect();
            }
            names.extend(visible.into_iter()
                .map(|index| self.page.rows[index].item.source_process_name.clone()));
        }
        if let Some((editor, _)) = &self.process_editor {
            names.extend(
                editor
                    .read(cx)
                    .catalog()
                    .iter()
                    .filter(|source| !self.process_icon_applied.contains(&source.exe_name))
                    .map(|source| source.exe_name.clone()),
            );
        }
        for name in names {
            if self.source_icon_pending.len() >= 4 {
                break;
            }
            if name.is_empty()
                || crate::history_sync_source::icon_name(&name).is_some()
                || self.source_icons.contains_key(&name)
                || !self.source_icon_pending.insert(name.clone())
            {
                continue;
            }
            let worker = host.clone();
            let requested = name.clone();
            cx.spawn(async move |this, cx| {
                let icon = cx
                    .background_executor()
                    .spawn(async move { load_source_icon(&worker, &requested, pixels).ok() })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.source_icon_pending.remove(&name);
                    if this.source_icon_pixels != pixels {
                        return;
                    }
                    this.source_icons.insert(name.clone(), icon.clone());
                    this.source_icon_order.push_back(name.clone());
                    while this.source_icon_order.len() > 128 {
                        if let Some(old) = this.source_icon_order.pop_front() {
                            this.source_icons.remove(&old);
                        }
                    }
                    if let Some((editor, _)) = &this.process_editor {
                        if editor
                            .read(cx)
                            .catalog()
                            .iter()
                            .any(|source| source.exe_name == name)
                        {
                            this.process_icon_applied.insert(name.clone());
                            if let Some(icon) = icon {
                                editor.update(cx, |editor, cx| {
                                    editor.set_source_icon(name, icon, cx)
                                });
                            }
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn content_mode(&self) -> ContentMode {
        if AppearanceSettings::from_config(&self.configuration)
            .resolved()
            .layout
            == RowLayout::Minimal
        {
            ContentMode::Minimal
        } else {
            ContentMode::Standard
        }
    }

    fn content_projection(row: &PreviewRow, mode: ContentMode) -> Option<ContentProjection> {
        if row.item.kind == "ImageClipItem" {
            return crate::history_image_resources::primary_image_projection(
                &row.item,
                row.thumbnail.as_ref().map(|image| image.source_version.as_str()).unwrap_or(""),
            );
        }
        project_history_item_with_text(
            &row.item,
            mode,
            &row.preview_text,
            &row.item.highlight_hit_ranges,
        )
    }

    fn content_resource_style<'a>(
        row: &PreviewRow,
        mode: ContentMode,
        foreground: &'a str,
    ) -> (ContentMode, &'a str) {
        if row.item.kind == "ImageClipItem" {
            // A raster thumbnail has no theme foreground or list-layout style.
            (ContentMode::Standard, "")
        } else {
            (mode, foreground)
        }
    }

    fn fill_content_resources(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else { return; };
        if !self.favorites_active {
            if let Some(favorites) = &self.favorites {
                favorites.update(cx, |favorites, cx| favorites.reclaim_idle_content(cx));
            }
        }
        let image_height = AppearanceSettings::from_config(&self.configuration)
            .image_max_height_px.clamp(1, 640) as u32;
        if self.content_image_height != image_height {
            // Primary rasters (keyed with an empty foreground) were sized for
            // the old setting; text/file resources stay.
            self.content_image_height = image_height;
            self.content_cache.retain(|key| !key.foreground.is_empty());
            self.content_cache.release_evicted(cx);
        }
        let visibility = host.hide_snapshot();
        let away = self.favorites_active || !visibility.visible || visibility.is_collapsed;
        if self.loading
            || away
            || self.exit_pending
            || self.page_generation != self.generation
        {
            host.report_visible_file_history(&[]);
            self.content_cache.set_visible_items(std::iter::empty::<&str>());
            if away && self.content_cache.reclaim_idle() {
                self.content_cache.release_evicted(cx);
            }
            return;
        }
        let generation = self.page_generation;
        let mode = self.content_mode();
        let foreground = self.tokens.theme(self.dark).text.clone();
        let viewport = self.list.viewport_bounds();
        // Walk from the scroll anchor only. Rows above it have no bounds, and
        // treating the first rows as visible kept them pinned in the content
        // budget, ahead of the thumbnails actually on screen.
        let top = self.list.logical_scroll_top().item_ix;
        let mut on_screen: Vec<usize> = (top..self.page.rows.len())
            .map_while(|index| self.list.bounds_for_item(index)
                .filter(|bounds| bounds.top() < viewport.bottom())
                .map(|bounds| (index, bounds)))
            .filter(|(_, bounds)| bounds.bottom() > viewport.top())
            .map(|(index, _)| index)
            .take(32)
            .collect();
        if on_screen.is_empty() {
            // Nothing measured yet (first layout, or after a re-measure).
            on_screen = (top..self.page.rows.len()).take(12).collect();
        }
        // On-screen rows reserve worker slots first; a bounded prefetch
        // below/above follows so short scrolls find ready rasters.
        let visible_rows: Vec<_> = native_content::cache::prefetch_order(&on_screen, self.page.rows.len())
            .into_iter()
            .map(|index| (index, &self.page.rows[index]))
            .collect();
        // Start at the measured scroll anchor and stop at the viewport bottom.
        // Resource prefetch rows above the viewport never enter maintenance.
        let visible_files: Vec<_> = self.page.rows.iter().enumerate()
            .skip(self.list.logical_scroll_top().item_ix)
            .map_while(|(index, row)| self.list.bounds_for_item(index)
                .filter(|bounds| bounds.top() < viewport.bottom())
                .map(|bounds| (row, bounds)))
            .filter(|(row, bounds)| matches!(row.item.kind.as_str(), "FileClipItem" | "ImageClipItem")
                && viewport.size.height > px(0.0) && bounds.bottom() > viewport.top())
            .take(32)
            .map(|(row, _)| row.item.id.clone())
            .collect();
        host.report_visible_file_history(&visible_files);
        let rows: Vec<_> = visible_rows.into_iter()
            .filter_map(|(_, row)| {
                Self::content_projection(row, mode).map(|projection| {
                    (row.item.id.clone(), projection, row.item.kind == "ImageClipItem")
                })
            })
            .filter(|(_, projection, _)| projection.needs_resources())
            .take(32)
            .collect();
        self.content_cache.set_visible_items(rows.iter().map(|(id, _, _)| id.as_str()));
        for (id, projection, primary_image) in rows {
            let resource_mode = if primary_image { ContentMode::Standard } else { mode };
            let resource_foreground = if primary_image { "" } else { &foreground };
            let Some(ticket) =
                self.content_cache
                    .reserve(generation, &id, &projection, resource_mode, resource_foreground)
            else {
                continue;
            };
            let worker = host.clone();
            let requested_projection = projection.clone();
            let requested_foreground = foreground.clone();
            let reply_foreground = foreground.clone();
            let requested_id = id.clone();
            cx.spawn(async move |this, cx| {
                let (resources, thumbnail, retryable) = cx
                    .background_executor()
                    .spawn(async move {
                        if primary_image {
                            let loaded = crate::history_image_resources::load_primary_display_image(
                                &worker, &requested_id, image_height,
                            );
                            let retryable = loaded.failure.is_some_and(|failure| failure.is_retryable());
                            return (loaded.resources, loaded.thumbnail, retryable);
                        }
                        (native_content::load_content_resources(
                            &worker,
                            &requested_projection,
                            &requested_foreground,
                        ), None, false)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let current = !this.exit_pending
                        && this.page_generation == generation
                        && this.generation == generation
                        && (primary_image || (this.content_mode() == mode
                            && this.tokens.theme(this.dark).text == reply_foreground))
                        && this
                            .page
                            .rows
                            .iter()
                            .find(|row| row.item.id == id)
                            .and_then(|row| Self::content_projection(row, mode))
                            .as_ref()
                            == Some(&projection);
                    let mut updated_projection = None;
                    // Image rows already reserve their probed size; only a
                    // changed size (or any other resource) needs re-measuring.
                    let mut remeasure = !primary_image;
                    if current {
                        if let Some(thumbnail) = thumbnail {
                            if let Some(row) = this.page.rows.iter_mut().find(|row| row.item.id == id) {
                                remeasure |= row.thumbnail.as_ref()
                                    .is_none_or(|old| (old.width, old.height) != (thumbnail.width, thumbnail.height));
                                row.thumbnail = Some(thumbnail);
                                updated_projection = Self::content_projection(row, mode)
                                    .filter(|updated| updated != &projection);
                            }
                        }
                    }
                    // A recovered probe/new source gets a new key. Retire its old
                    // ticket before publishing the freshly prepared image under
                    // the source version that was actually read.
                    let active_ticket = if let Some(updated) = updated_projection {
                        this.content_cache.finish(ticket, ContentResources::default(), false);
                        this.content_cache.reserve(generation, &id, &updated, ContentMode::Standard, "")
                    } else { Some(ticket) };
                    if let Some(ticket) = active_ticket {
                        if retryable {
                            this.content_cache.finish_retryable(ticket, resources, current);
                        } else {
                            this.content_cache.finish(ticket, resources, current);
                        }
                    }
                    this.content_cache.release_evicted(cx);
                    if current && remeasure {
                        this.content_dirty_rows.insert(id);
                    }
                    // Even a rejected completion frees a worker slot: chain the
                    // next load and fold this result into one frame's update.
                    this.schedule_content_flush(cx);
                });
            })
            .detach();
        }
        self.content_cache.release_evicted(cx);
    }

    /// Coalesce resource completions: one re-measure pass and one scheduling
    /// pass per frame, instead of a splice/notify per arriving image.
    fn schedule_content_flush(&mut self, cx: &mut Context<Self>) {
        if self.content_flush_pending {
            return;
        }
        self.content_flush_pending = true;
        let owner = cx.weak_entity();
        let scheduled = self.window.update(cx, |_, window, _| {
            window.on_next_frame(move |_, cx| {
                let _ = owner.update(cx, |this, cx| this.flush_content_completions(cx));
            });
        });
        if scheduled.is_err() {
            let owner = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = owner.update(cx, |this, cx| this.flush_content_completions(cx));
            });
        }
        cx.notify();
    }

    fn flush_content_completions(&mut self, cx: &mut Context<Self>) {
        self.content_flush_pending = false;
        let dirty = std::mem::take(&mut self.content_dirty_rows);
        if !dirty.is_empty() {
            // Re-measure only these rows; reset() would clear the scroll anchor.
            let anchor = self.list.logical_scroll_top();
            self.history_scrollbar.invalidate_measurements();
            for index in self.page.rows.iter().enumerate()
                .filter(|(_, row)| dirty.contains(&row.item.id))
                .map(|(index, _)| index)
            {
                self.list.splice(index..index + 1, 1);
            }
            self.list.scroll_to(anchor);
        }
        self.fill_content_resources(cx);
        cx.notify();
    }

    fn open_content(
        &mut self,
        request: ContentOpenRequest,
        index: usize,
        generation: u64,
        session: u64,
        cx: &mut Context<Self>,
    ) {
        if self.favorites_active
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.exit_pending
            || self.loading
            || self.native_dragging
            || self
                .native_suppress_click
                .is_some_and(|until| Instant::now() < until)
            || generation != self.generation
            || self.page_generation != generation
            || session != self.native_session
            || !self
                .page
                .rows
                .get(index)
                .is_some_and(|row| row.item.id == request.item_id)
        {
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        let source = match host.capture_native_clipboard_action(None) {
            Ok(source) => source,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.run_host(false, None, cx, move |host| match request.target {
            native_content::links::OpenTarget::Http(uri) => {
                host.shell_open_uri_native(&json!({"uri":uri}), &source)
            }
            native_content::links::OpenTarget::ShellPath(path) => {
                host.shell_open_path_native(&json!({"path":path}), &source)
            }
        });
    }

    fn hover_action(
        &mut self,
        invocation: HoverInvocation,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.favorites_active
            || self.history_dialog
            || self.process_editor.is_some()
            || self.process_open_pending
            || self.exit_pending
            || generation != self.generation
            || self.page_generation != self.generation
            || !self
                .page
                .rows
                .iter()
                .any(|row| row.item.id == invocation.item_id)
        {
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        let id = invocation.item_id;
        if invocation.command == HoverCommand::Favorite {
            self.favorite_save_invoked(id, invocation.anchor, invocation.hovered, cx);
            return;
        }
        match invocation.command {
            HoverCommand::Edit => self.open_editor(id, false, cx),
            HoverCommand::Paste => match host.capture_native_paste(None) {
                Ok(request) => self.run_host(false, None, cx, move |host| {
                    host.paste_native(&json!({"id":id}), &request)
                }),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                }
            },
            HoverCommand::Copy => match host.capture_native_clipboard_action(None) {
                Ok(request) => self.run_host(false, None, cx, move |host| {
                    host.copy_items_native(&json!({"ids":[id]}), &request)
                }),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                }
            },
            HoverCommand::TogglePin => self.run_host(true, None, cx, move |host| {
                host.toggle_pin(&json!({"id":id}))
            }),
            HoverCommand::Favorite => unreachable!("favorite picker handled above"),
            HoverCommand::Delete => self.delete_history_items(vec![id], cx),
        }
    }

    fn refresh_theme(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.model.host().is_none() {
            return;
        }
        let mode = self.configuration["uiThemeMode"].as_str().unwrap_or("auto");
        let dark = if mode.eq_ignore_ascii_case("dark") {
            true
        } else if mode.eq_ignore_ascii_case("light") {
            false
        } else {
            matches!(
                window.appearance(),
                gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
            )
        };
        let resolved = self.theme_cache.resolve(&self.base_tokens, &self.configuration);
        let palette_changed = resolved.as_ref()
            .is_some_and(|tokens| self.tokens.theme(dark) != tokens.theme(dark));
        if let Some(tokens) = resolved {
            self.tokens = tokens;
        }
        if dark == self.dark && !palette_changed {
            return;
        }
        self.dark = dark;
        self.refresh_clipboard_colors(cx);
        self.close_menu(cx);
        cx.notify();
    }

    /// Rebuild the category bar from the core's filter items and the
    /// separate-pinned setting; keep the selection when its tab still exists.
    fn sync_tabs(&mut self) {
        let Some(host) = self.model.host() else {
            return;
        };
        let separate = self.configuration["clipListSeparatePinnedUi"]
            .as_bool()
            .unwrap_or(false);
        self.tabs = build_tabs(&host.filters_get(), separate);
        if !self.tabs.iter().any(|tab| tab.key == self.category) {
            let fallback = match self.category.as_str() {
                "all" | "text" | "richtext" | "image" | "file" | "url" => self
                    .tabs
                    .iter()
                    .find(|tab| tab.request_type == self.category && !tab.is_pinned()),
                _ => None,
            };
            self.category = fallback
                .or(self.tabs.first())
                .map(|tab| tab.key.clone())
                .unwrap_or_else(|| "all".into());
        }
        let sort = i32::from(self.configuration["autoSortAfterPaste"].as_bool() == Some(true));
        self.advanced.sort_type = sort;
    }

    /// Text fields other than search need the same native activation handoff;
    /// the field's own mouse-down then takes GPUI focus.
    fn engage_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.defer_in(window, |this, window, cx| {
            let result = if let Some(host) = this.model.host() {
                NativeWindow::request_search_focus(window, &host)
            } else {
                platform::set_no_activate(window, false).map(|_| window.activate_window())
            };
            if let Err(error) = result {
                this.error = Some(error);
                cx.notify();
            }
        });
    }

    fn make_field(
        &self,
        text: String,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SearchInput> {
        let t = self.clipboard_tokens().theme(self.dark).clone();
        let height = self.tokens.settings.control_height - 2.0;
        cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(
                InputColors {
                    background: color(&t.search_background),
                    foreground: color(&t.text),
                    placeholder: color(&t.text_muted),
                    border: color(&t.border_secondary),
                    selection: color(&t.tab_active),
                    caret: color(&t.accent),
                },
                cx,
            );
            input.set_metrics(height, 12.0, 8.0, cx);
            input.set_placeholder(placeholder, cx);
            input.set_double_click_clears(false);
            input.set_text(text, window, cx);
            input
        })
    }

    fn toggle_filter_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_process_editor(window, cx);
        if self.filter_panel.take().is_some() {
            self.close_filter_panel(window, cx);
            return;
        }
        self.list_settings = None;
        let fields = [
            self.make_field(
                self.advanced.start.clone(),
                tr("开始 YYYY-MM-DD", "Start YYYY-MM-DD"),
                window,
                cx,
            ),
            self.make_field(
                self.advanced.end.clone(),
                tr("结束 YYYY-MM-DD", "End YYYY-MM-DD"),
                window,
                cx,
            ),
            self.make_field(
                self.advanced.date.clone(),
                tr("日期 YYYY-MM-DD", "Date YYYY-MM-DD"),
                window,
                cx,
            ),
        ];
        let mut subscriptions = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            subscriptions.push(
                cx.subscribe_in(field, window, move |this, _, event, _, cx| {
                    if let SearchEvent::Changed(text) = event {
                        let text = text.trim().to_string();
                        match index {
                            0 => this.advanced.start = text,
                            1 => this.advanced.end = text,
                            _ => this.advanced.date = text,
                        }
                        this.refresh(cx);
                    }
                }),
            );
        }
        self.filter_panel = Some((fields, subscriptions));
        cx.notify();
    }

    fn close_filter_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_panel = None;
        self.focus.focus(window);
        self.restore_no_focus(window);
        cx.notify();
    }

    fn set_advanced(&mut self, update: impl FnOnce(&mut AdvancedFilter), cx: &mut Context<Self>) {
        let before = self.advanced.sort_type;
        update(&mut self.advanced);
        if self.advanced.sort_type != before {
            // 「粘贴后置顶」 is the one persisted choice (web `persistAutoSortAfterPaste`).
            if let Some(host) = self.model.host() {
                let enabled = self.advanced.sort_type == 1;
                if let Err(error) = host.config_update(&json!({ "autoSortAfterPaste": enabled })) {
                    self.error = Some(error);
                }
            }
        }
        self.refresh(cx);
    }

    fn open_tag_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_pending {
            return;
        }
        let Some(host) = self.model.host() else {
            self.show_notice(
                tr(
                    "设计预览不连接设置",
                    "Settings are unavailable in design preview",
                )
                .into(),
                cx,
            );
            return;
        };
        self.open_tag_editor_for(host, window, cx);
    }

    #[cfg(debug_assertions)]
    pub fn open_tag_editor_visual(&mut self, host: Arc<HostState>, window: &mut Window, cx: &mut Context<Self>) {
        assert!(
            matches!(self.model, UiModel::Fixture(_)) && cliphost_core::is_headless(),
            "isolated debug fixture only"
        );
        self.open_tag_editor_for(host, window, cx);
    }

    fn open_tag_editor_for(&mut self, host: Arc<HostState>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_process_editor(window, cx);
        self.list_settings = None;
        self.filter_panel = None;
        let (tokens, dark) = (self.clipboard_tokens(), self.dark);
        let failed = crate::filters::failed_filter_tab_writes()
            .into_iter()
            .filter(|failure| failure.data_root == host.data_root().to_string_lossy())
            .max_by_key(|failure| failure.id);
        let editor = cx.new(|cx| FilterTabsEditor::new(host, tokens, dark, window, cx));
        if let Some(failure) = failed {
            if let Err(error) = editor.update(cx, |editor, cx| {
                editor.restore_failed_write(failure.id, window, cx)
            }) {
                self.error = Some(error);
            }
        }
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            this.tag_editor = None;
            if let FilterTabsEvent::Saved = event {
                this.sync_tabs();
                this.refresh(cx);
                this.show_notice(tr("分类标签已保存", "Category tabs saved").into(), cx);
            }
            this.focus.focus(window);
            this.restore_no_focus(window);
            cx.notify();
        });
        self.tag_editor = Some((editor, subscription));
        self.filter_popover_generation = self.filter_popover_generation.wrapping_add(1);
        cx.notify();
    }

    fn open_settings_route(&mut self, route: &str, cx: &mut Context<Self>) {
        if route.split(['?', '#']).next() == Some("/main/lan") {
            self.open_lan_companion(true, None, cx);
            return;
        }
        match self.model.host() {
            Some(host) => host.show_web_route(route),
            None => self.show_notice(
                tr(
                    "设计预览不连接设置",
                    "Settings are unavailable in design preview",
                )
                .into(),
                cx,
            ),
        }
    }

    fn toggle_list_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list_settings.is_none() && self.search.read(cx).is_composing() { return; }
        self.close_process_editor(window, cx);
        self.filter_panel = None;
        self.tag_editor = None;
        if self.list_settings.is_some() {
            self.close_list_settings(window, cx);
            return;
        }
        let Some(host) = self.model.host() else {
            self.show_notice(
                tr(
                    "设计预览不连接设置",
                    "Settings are unavailable in design preview",
                )
                .into(),
                cx,
            );
            return;
        };
        self.create_list_settings(host, window, cx);
    }

    fn chip(
        &self,
        id: SharedString,
        label: &'static str,
        active: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let hover = color(&t.hover);
        rsx! {
            <div id={id} h={px(24.0)} px={px(8.0)} flex items_center flex_none rounded={px(6.0)}
                border_1 text_size={px(12.0)} cursor_pointer />
        }
            .when(active, |chip| {
                chip.bg(color(&t.tab_active))
                    .border_color(color(&t.accent))
                    .text_color(color(&t.accent))
            })
            .when(!active, |chip| {
                chip.border_color(color(&t.border_secondary))
                    .text_color(color(&t.text_secondary))
                    .hover(move |style| style.bg(hover))
            })
            .child(label)
    }

    fn boxed_field(
        &self,
        field: &Entity<SearchInput>,
        window: &Window,
        cx: &gpui::App,
    ) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        let focused = field.read(cx).focus_handle().is_focused(window);
        rsx! {
            <div flex flex_1 min_w_0 rounded={px(6.0)} border_1 />
        }
            .border_color(color(if focused {
                &t.accent
            } else {
                &t.border_secondary
            }))
            .overflow_hidden()
            .child(field.clone())
    }

    /// Search-box filter popover (web `clip-filter-panel`).
    fn filter_panel_view(
        &self,
        fields: [Entity<SearchInput>; 3],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = self.tokens.theme(self.dark);
        let l = &self.tokens.layout;
        let s = &self.tokens.settings;
        let advanced = &self.advanced;
        let title = |text: &'static str| {
            rsx! {
                <div pt={px(8.0)} pb={px(4.0)} text_size={px(12.0)}
                    font_weight={FontWeight::SEMIBOLD} text_color={color(&t.text_muted)}>
                    {text}
                </div>
            }
        };
        let usage = [
            (0, tr("全部", "All")),
            (1, tr("已使用", "Used")),
            (2, tr("未使用", "Unused")),
        ];
        let created = [
            (0, tr("忽略", "Any time")),
            (1, tr("1天", "1 day")),
            (2, tr("2天", "2 days")),
            (7, tr("7天", "7 days")),
            (30, tr("30天", "30 days")),
            (-1, tr("按范围", "Date range")),
            (-2, tr("按日期", "On date")),
        ];
        let sort_on = advanced.sort_type == 1;
        let knob = s.switch_height - 4.0;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            let hover = color(&t.tertiary);
            rsx! {
                <div id={id} h={px(26.0)} px={px(12.0)} flex items_center rounded={px(6.0)}
                    text_size={px(12.0)} cursor_pointer />
            }
                .when(primary, |b| {
                    b.bg(color(&t.accent)).text_color(gpui::white())
                })
                .when(!primary, |b| {
                    b.border_1()
                        .border_color(color(&t.border_secondary))
                        .text_color(color(&t.text_secondary))
                        .hover(move |style| style.bg(hover))
                })
                .child(label)
        };
        let card =
            rsx! {
                <div id={"filter-panel"} absolute top={px(l.title_bar_height + 4.0)}
                    right={px(l.content_padding_x + 4.0)} w={px(s.panel_width - 20.0)}
                    max_w={relative(0.94)} flex flex_col px={px(14.0)} pb={px(10.0)}
                    bg={self.popover_surface()} border_1
                    border_color={color(&t.border_secondary)} rounded={px(s.card_radius)}
                    shadow_lg />
            }
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(title(tr("粘贴行为", "Paste behavior")))
                .child(
                    rsx! {
                        <div id={"filter-sort"} flex items_center justify_between cursor_pointer
                            text_size={px(13.0)}>
                            {tr("粘贴后置顶", "Pin after pasting")}
                        </div>
                    }
                        .child(
                            rsx! {
                                <div w={px(s.switch_width)} h={px(s.switch_height)} flex_none
                                    rounded_full p={px(2.0)} flex />
                            }
                                .when(sort_on, |track| track.justify_end())
                                .bg(color(if sort_on {
                                    &t.accent
                                } else {
                                    &t.border_secondary
                                }))
                                .child(div().size(px(knob)).rounded_full().bg(gpui::white())),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_advanced(|a| a.sort_type = if sort_on { 0 } else { 1 }, cx)
                        })),
                )
                .child(title(tr("按使用状态筛选", "Filter by usage")))
                .child(div().flex().flex_wrap().gap(px(4.0)).children(usage.map(
                    |(value, label)| {
                        self.chip(
                            SharedString::from(format!("usage-{value}")),
                            label,
                            advanced.usage_filter == value,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_advanced(|a| a.usage_filter = value, cx)
                        }))
                    },
                )))
                .child(title(tr("按创建时间筛选", "Filter by creation date")))
                .child(div().flex().flex_wrap().gap(px(4.0)).children(created.map(
                    |(value, label)| {
                        self.chip(
                            SharedString::from(format!("created-{value}")),
                            label,
                            advanced.create_time_filter == value,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_advanced(|a| a.create_time_filter = value, cx)
                        }))
                    },
                )))
                .when(advanced.create_time_filter == -1, |card| {
                    card.child(
                        rsx! {
                            <div pt={px(6.0)} flex items_center gap={px(6.0)}>
                                {self.boxed_field(&fields[0], window, cx)}
                                <div text_size={px(12.0)}>
                                    {tr("至", "to")}
                                </div>
                                {self.boxed_field(&fields[1], window, cx)}
                            </div>
                        },
                    )
                })
                .when(advanced.create_time_filter == -2, |card| {
                    card.child(
                        rsx! {
                            <div pt={px(6.0)} flex>
                                {self.boxed_field(&fields[2], window, cx)}
                            </div>
                        },
                    )
                })
                // Date fields need native keyboard activation in the no-focus window.
                .capture_any_mouse_down(cx.listener(
                    |this, event: &gpui::MouseDownEvent, window, cx| {
                        if event.button == MouseButton::Left
                            && matches!(this.advanced.create_time_filter, -1 | -2)
                        {
                            this.engage_keyboard(window, cx);
                        }
                    },
                ))
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .pt(px(12.0))
                        .child(button("filter-reset", tr("重置", "Reset"), false).on_click(
                            cx.listener(|this, _, window, cx| {
                                for field in this
                                    .filter_panel
                                    .iter()
                                    .flat_map(|(fields, _)| fields.clone())
                                {
                                    field.update(cx, |input, cx| {
                                        input.set_text(String::new(), window, cx)
                                    });
                                }
                                this.set_advanced(|a| *a = AdvancedFilter::default(), cx);
                            }),
                        ))
                        .child(div().flex_1())
                        .child(button("filter-done", tr("完成", "Done"), true).on_click(
                            cx.listener(|this, _, window, cx| this.close_filter_panel(window, cx)),
                        )),
                );
        div()
            .id("filter-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_filter_panel(window, cx)),
            )
            .child(card)
            .into_any_element()
    }

    fn icon(&self, key: &str, size: f32, color: Hsla) -> crate::crisp_svg::CrispSvg {
        svg()
            .path(self.tokens.icons.get(key).expect("shared icon").clone())
            .size(px(size))
            .flex_none()
            .text_color(color)
    }

    /// Borderless square icon button for the title bar and footer. Title-bar
    /// buttons exclude themselves from the native caption drag region.
    /// Design fixtures only: choose the list appearance slot without a host.
    pub fn set_fixture_appearance(&mut self, slot: &str) {
        if self.model.host().is_none() {
            self.configuration = json!({ "clipListAppearanceSlot": slot });
        }
    }

    #[cfg(debug_assertions)]
    pub fn seed_history_return_top_visual(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        if !matches!(self.model, UiModel::Fixture(_)) || index >= self.page.rows.len() { return false; }
        self.list.scroll_to(gpui::ListOffset { item_ix: index, offset_in_item: px(0.0) });
        self.sync_history_top_button(cx);
        cx.notify();
        true
    }

    #[cfg(debug_assertions)]
    pub fn return_history_to_top_visual(&mut self, cx: &mut Context<Self>) -> bool {
        if !matches!(self.model, UiModel::Fixture(_)) { return false; }
        self.return_history_to_top(cx);
        true
    }

    #[cfg(debug_assertions)]
    pub fn seed_context_menu_layout_visual(&mut self, configuration: Value, cx: &mut Context<Self>) -> bool {
        if !matches!(self.model, UiModel::Fixture(_)) { return false; }
        self.configuration = configuration;
        self.snapshot = true;
        self.set_context_menu_layout_visual(false, cx)
    }

    #[cfg(debug_assertions)]
    pub fn set_context_menu_layout_visual(&mut self, open: bool, cx: &mut Context<Self>) -> bool {
        if !matches!(self.model, UiModel::Fixture(_)) { return false; }
        // Exercise the production visibility branch without a real menu lease.
        self.menu_lease = open.then_some((0, Vec::new()));
        // Force new measurements so cached row heights cannot hide a regression.
        self.list.reset(self.page.rows.len());
        cx.notify();
        true
    }

    fn tool_button(
        &self,
        id: &str,
        icon: &str,
        tint: Hsla,
        caption: bool,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let l = &self.tokens.layout;
        let hover = color(&t.hover);
        div()
            .id(SharedString::from(format!("tool-{id}")))
            .relative()
            .when(caption, |element| element.child(caption_exclusion()))
            .size(px(if caption { self.tokens.titlebar_search.button_size } else { l.icon_button_size }))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(l.chip_radius))
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(self.icon(icon, l.chrome_icon_size, tint))
    }

    fn select_category(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.more_menu = false;
        self.category = key;
        self.focus.focus(window);
        self.restore_no_focus(window);
        self.refresh(cx);
    }

    fn close_more_menu(&mut self, cx: &mut Context<Self>) {
        if self.more_menu {
            self.more_menu = false;
            self.sync_overlay_guard();
            cx.notify();
        }
    }

    /// 「⋯」: categories that did not fit the row, then the category management
    /// entries that used to sit inside the strip.
    fn more_menu_view(
        &self,
        overflow_tabs: &[usize],
        overflow_processes: Vec<crate::process_filters::ProcessTab>,
        process_handler: crate::process_filters::ProcessTabHandler,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let l = &tokens.layout;
        let item = |id: SharedString, checked: bool| {
            let hover = color(&t.hover);
            rsx! {
                <div id={id} h={px(28.0)} px={px(8.0)} flex items_center gap={px(8.0)}
                    rounded={px(5.0)} text_size={px(tokens.typography.category_font_size)} />
            }
                .text_color(color(if checked { &t.text } else { &t.text_secondary }))
                .when(checked, |element| element.bg(color(&t.hover)))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
        };
        let mut card = rsx! {
            <div id={"category-more-menu"} absolute
                top={px(l.title_bar_height + l.content_padding_y + l.toolbar_height)}
                right={px(l.content_padding_x + 2.0)} min_w={px(156.0)} flex flex_col p={px(4.0)}
                bg={self.popover_surface()} border_1 border_color={color(&t.border)}
                rounded={px(8.0)} shadow_lg occlude />
        }
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for &index in overflow_tabs {
            let tab = &self.tabs[index];
            let key = tab.key.clone();
            card = card.child(
                item(SharedString::from(format!("more-tab-{index}")), self.category == tab.key)
                    .child(self.icon(tab.icon, l.icon_size, color(&t.text_secondary)))
                    .child(tab.label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_category(key.clone(), window, cx)
                    })),
            );
        }
        for tab in overflow_processes.iter() {
            let id = tab.id.clone();
            let handler = process_handler.clone();
            card = card.child(
                item(SharedString::from(format!("more-process-{id}")), tab.active)
                    .child(
                        rsx! {
                            <div size={px(l.icon_size)} flex items_center justify_center>
                                <div size={px(8.0)} rounded_full border_1
                                    border_color={color(&t.border_secondary)} />
                            </div>
                        },
                    )
                    .child(tab.label.clone())
                    .on_click(move |_, window, cx| handler(id.clone(), window, cx))
                    .on_mouse_down(MouseButton::Right, {
                        let id = tab.id.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.more_menu = false;
                            this.edit_process_preset(id.clone(), window, cx)
                        })
                    }),
            );
        }
        if !overflow_tabs.is_empty() || !overflow_processes.is_empty() {
            card = card.child(rsx! {
                <div h={px(1.0)} mx={px(2.0)} my={px(4.0)} bg={color(&t.border)} />
            });
        }
        card = card
            .child(
                item("more-add-process".into(), false)
                    .child(self.icon("plus", l.icon_size, color(&t.text_secondary)))
                    .child(tr("添加进程筛选…", "Add process filter…"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_more_menu(cx);
                        this.open_process_editor(window, cx)
                    })),
            )
            .child(
                item("more-customize-tabs".into(), false)
                    .child(self.icon("sliders", l.icon_size, color(&t.text_secondary)))
                    .child(tr("自定义分类…", "Customize categories…"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_more_menu(cx);
                        this.open_tag_editor(window, cx)
                    })),
            );
        div()
            .id("category-more-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_more_menu(cx)),
            )
            .child(card)
            .into_any_element()
    }

    fn row(
        &self,
        index: usize,
        row: PreviewRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.stats_enabled {
            let mut stats = self.stats.lock().unwrap();
            stats.rows_built += 1;
            stats.row_ids.insert(row.item.id.clone());
        }
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let l = &tokens.layout;
        let settings = AppearanceSettings::from_config(&self.configuration);
        let appearance = settings.resolved();
        let metrics = settings.row_metrics();
        let selected = self.selected_ids.contains(&row.item.id);
        let id = row.item.id.clone();
        #[cfg(windows)]
        let hover_binding =
            self.history_hover_binding(index, &id, self.page_generation, self.native_session);
        #[cfg(not(windows))]
        let hover_binding: Option<hover_binding::HoverBinding> = None;
        let metadata = RowMetadata::new(
            &row.item.source_process_name,
            &row.item.create_time.to_rfc3339(),
        );
        let group = SharedString::from(format!("history-row-{id}"));
        let minimal = appearance.layout == RowLayout::Minimal;
        // Minimal rows cap thumbnails (tokens minimalImageMaxHeight, DESIGN 2026-10-14).
        let image_max_height = if minimal {
            (settings.image_max_height_px as f32).min(l.minimal_image_max_height)
        } else {
            settings.image_max_height_px as f32
        };
        let row_width =
            f32::from(window.viewport_size().width) - 2.0 * l.content_padding_x;
        // Digit/pin marks and hover actions overlay minimal row content.
        // Only the custom process/time metadata can reserve body width.
        let reserve = if minimal {
            metadata.minimal_reserve(&appearance)
        } else {
            0.0
        };
        #[cfg(debug_assertions)]
        let qa_row_observer = {
            let mode = if minimal {
                ContentMode::Minimal
            } else {
                ContentMode::Standard
            };
            let ready = Self::content_projection(&row, mode)
                .map(|projection| {
                    let (resource_mode, resource_foreground) = Self::content_resource_style(&row, mode, &t.text);
                    self.content_cache
                        .get(self.page_generation, &id, &projection, resource_mode, resource_foreground)
                        .is_some()
                })
                .unwrap_or(true);
            self.content_qa_row_observer(
                &id,
                &row.item.kind,
                ready,
                "",
                row.item
                    .step_presentation
                    .as_ref()
                    .and_then(|value| value["stepCount"].as_u64())
                    .unwrap_or(0) as usize,
            )
        };
        let mut content = rsx! {
            <div relative flex_1 min_w_0 flex flex_col gap={px(metrics.content_gap)}
                text_color={color(&t.text)}
                font_weight={FontWeight(tokens.typography.text_weight as f32)} />
        };
        let mut image_information = appearance::RowImageInformation::default();
        if row.item.kind == "ImageClipItem" || row.thumbnail.is_some() {
            let mode = if minimal { ContentMode::Minimal } else { ContentMode::Standard };
            let (resource_mode, resource_foreground) = Self::content_resource_style(&row, mode, &t.text);
            let projection = Self::content_projection(&row, mode);
            let load_status = projection.as_ref().map(|projection| self.content_cache.load_status(
                self.page_generation, &id, projection, resource_mode, resource_foreground,
            )).unwrap_or(native_content::cache::ContentLoadStatus::Missing);
            let prepared = projection.as_ref().and_then(|projection| {
                self.content_cache
                    .get(self.page_generation, &id, projection, resource_mode, resource_foreground)
                    .and_then(|resources| resources.images.get(crate::history_image_resources::PRIMARY_IMAGE_RESOURCE))
                    .and_then(|image| image.clone())
            });
            let (natural_width, natural_height) = row.thumbnail.as_ref()
                .map(|thumbnail| (thumbnail.width, thumbnail.height))
                .or_else(|| prepared.as_ref().map(|image| (image.width as u32, image.height as u32)))
                .unwrap_or((320, 180));
            image_information = appearance::RowImageInformation {
                preview: true,
                width: Some(natural_width),
                height: Some(natural_height),
                size_bytes: None,
            };
            let available_width = f32::from(window.viewport_size().width)
                - 2.0 * l.content_padding_x
                - 2.0 * metrics.padding_x
                - reserve;
            let layout = crate::raster_layout::natural_image_layout(
                natural_width as f32, natural_height as f32, available_width,
                image_max_height, window.scale_factor(),
            );
            // Real history uses only prepared, budgeted images. Embedded design
            // fixtures have no Host or filesystem loader and remain embedded.
            let source = prepared.as_ref().map(native_content::ContentImage::source)
                .or_else(|| row.thumbnail.as_ref().filter(|thumbnail| {
                    self.model.host().is_none() && !std::path::Path::new(&thumbnail.asset).is_absolute()
                }).map(|thumbnail| gpui::ImageSource::Resource(gpui::Resource::Embedded(
                    thumbnail.asset.clone().into(),
                ))));
            let image = if let Some(source) = source.clone() { img(source)
                .w(px(layout.width_dip))
                .h(px(layout.height_dip))
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(px(l.image_radius * crate::raster_layout::image_dpi_compensation(window.scale_factor())))
                .with_fallback(|| {
                    div()
                        .child(tr("图片加载失败", "Image failed to load"))
                        .into_any_element()
                })
                .into_any_element()
            } else {
                use native_content::cache::ContentLoadStatus;
                // A first load is usually a few frames: keep the reserved box
                // blank (as a browser <img> does) so rows scrolling into view
                // do not flash a text label before their raster.
                let message = match load_status {
                    ContentLoadStatus::RetryScheduled => Some(tr("图片暂时不可用，正在重试", "Image temporarily unavailable; retrying")),
                    ContentLoadStatus::Unavailable | ContentLoadStatus::RetryExhausted => Some(tr("图片不可用", "Image unavailable")),
                    ContentLoadStatus::MemoryLimited => Some(tr("图片预览等待可用内存", "Image preview waiting for memory")),
                    ContentLoadStatus::Missing | ContentLoadStatus::Loading | ContentLoadStatus::Ready => None,
                };
                div().w(px(layout.width_dip)).h(px(layout.height_dip))
                    .children(message)
                    .into_any_element()
            };
            let image = if let Some(prepared) = &prepared {
                native_content::render_content_image(prepared, image)
            } else { image };
            #[cfg(debug_assertions)]
            let image = qa_content_harness::wrap_content_qa_primary_image(
                image,
                qa_row_observer.clone(),
            );
            content = content.child(match source {
                Some(source) => hover_binding::wrap_hover_thumbnail(
                    image,
                    hover_binding.clone(),
                    crate::hover::HoverThumbnailTarget::ItemImage,
                    source,
                ),
                None => hover_binding::wrap_hover_target(
                    image, hover_binding.clone(), crate::hover::HoverThumbnailTarget::ItemImage,
                ),
            });
        } else {
            let mode = if minimal {
                ContentMode::Minimal
            } else {
                ContentMode::Standard
            };
            if let Some(projection) = Self::content_projection(&row, mode) {
                let empty = ContentResources::default();
                let resources = self
                    .content_cache
                    .get(self.page_generation, &id, &projection, mode, &t.text)
                    .unwrap_or(&empty);
                image_information.preview = match &projection {
                    ContentProjection::Files(files) => native_content::history_file_image_preview(files, resources).is_some(),
                    ContentProjection::LocalPath(path) => path.is_image && !resources.paths.get(&path.path)
                        .is_some_and(|info| info.exists == Some(false)),
                    _ => false,
                };
                let owner = cx.weak_entity();
                let generation = self.page_generation;
                let session = self.native_session;
                #[cfg(debug_assertions)]
                let observer = qa_row_observer
                    .as_ref()
                    .map(|observer| observer.content_observer());
                #[cfg(not(debug_assertions))]
                let observer = None;
                let source_indices =
                    native_content::model::project_mixed_source_indices(&row.item.im_rich_segments);
                let thumbnail_wrapper: Option<native_content::ThumbnailWrapper> =
                    hover_binding.as_ref().map(|binding| {
                        let binding = binding.clone();
                        Rc::new(move |original_index, image, child| {
                            hover_binding::wrap_hover_thumbnail(
                                child,
                                Some(binding.clone()),
                                crate::hover::HoverThumbnailTarget::ImImage(original_index),
                                gpui::ImageSource::Image(image),
                            )
                        }) as native_content::ThumbnailWrapper
                    });
                let history_thumbnail_wrapper: Option<native_content::HistoryThumbnailWrapper> =
                    hover_binding.as_ref().map(|binding| {
                        let binding = binding.clone();
                        Rc::new(move |target, image, child| {
                            let target = match target {
                                native_content::HistoryThumbnailTarget::FileImage(index) =>
                                    crate::hover::HoverThumbnailTarget::FileImage(index),
                                native_content::HistoryThumbnailTarget::FileMedia(index) =>
                                    crate::hover::HoverThumbnailTarget::FileMedia(index),
                                native_content::HistoryThumbnailTarget::TextImage =>
                                    crate::hover::HoverThumbnailTarget::TextImage,
                            };
                            match image {
                                Some(image) => hover_binding::wrap_hover_thumbnail(
                                    child,
                                    Some(binding.clone()),
                                    target,
                                    gpui::ImageSource::Image(image),
                                ),
                                None => hover_binding::wrap_hover_target(
                                    child, Some(binding.clone()), target,
                                ),
                            }
                        }) as native_content::HistoryThumbnailWrapper
                    });
                let file_row = row.item.kind == "FileClipItem";
                content = content.child(native_content::render_history_content_with_raster_scale(
                    &id,
                    &projection,
                    resources,
                    ContentMetrics {
                        width: (row_width - 2.0 * metrics.padding_x - reserve).max(1.0),
                        font_size: tokens.typography.font_size,
                        line_height: metrics.text_line_height,
                        max_lines: metrics.text_max_lines,
                    },
                    image_max_height,
                    window.scale_factor(),
                    &tokens,
                    self.dark,
                    Rc::new(move |request, _, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if file_row {
                                if generation != this.page_generation || generation != this.generation
                                    || session != this.native_session
                                    || !this.page.rows.get(index).is_some_and(|row| row.item.id == request.item_id)
                                {
                                    return;
                                }
                                if let native_content::links::OpenTarget::ShellPath(path) = request.target {
                                    this.open_file(request.item_id, path, cx);
                                }
                            } else {
                                this.open_content(request, index, generation, session, cx);
                            }
                        });
                    }),
                    observer,
                    &source_indices,
                    thumbnail_wrapper,
                    history_thumbnail_wrapper,
                ));
            }
            if let Some(summary) =
                native_content::model::history_secondary_projection(&row.item, mode)
            {
                let owner = cx.weak_entity();
                let generation = self.page_generation;
                let session = self.native_session;
                content = content.child(native_content::render_secondary_content(
                    &id,
                    &summary,
                    (row_width - 2.0 * metrics.padding_x - reserve).max(1.0),
                    settings.density == crate::appearance::Density::Relaxed,
                    &tokens,
                    self.dark,
                    Rc::new(move |request, _, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            this.open_content(request, index, generation, session, cx)
                        });
                    }),
                ));
            }
        }
        content = content.child(appearance::render_row_information(
            &appearance::project_row_information(&row.item, appearance.layout, image_information),
            (row_width - 2.0 * metrics.padding_x - reserve).max(1.0),
            &tokens, self.dark,
        ));
        let view = cx.weak_entity();
        let menu_view = view.clone();
        let action_view = view.clone();
        let generation = self.page_generation;
        let drag = crate::favorite_drag::ContentDrag::history(
            if selected && self.selected_ids.len() > 1 {
                self.selection()
            } else {
                vec![id.clone()]
            },
            String::new(),self.page_generation,self.native_session,row.item.title.clone());
        let drag_owner = view.clone();
        let hover_owner = view.clone();
        let hover_id = id.clone();
        let hover_session = self.native_session;
        let click_id = id.clone();
        let menu_id = id.clone();
        let actions = render_hover_actions(
            &id,
            row.item.is_top,
            self.configuration["clipListExecuteMode"]
                .as_i64()
                .unwrap_or(1) as i32,
            self.model.host().is_some(),
            &settings,
            &tokens,
            self.dark,
            group.clone(),
            row_width,
            self.snapshot,
            self.menu_lease.is_some(),
            Rc::new(move |invocation, _, cx| {
                let _ = action_view
                    .update(cx, |this, cx| this.hover_action(invocation, generation, cx));
            }),
        );
        let process_icon = self
            .source_icons
            .get(&row.item.source_process_name)
            .and_then(Option::as_ref)
            .map(|icon| source_icon_element(icon, &tokens.theme(self.dark).surface));
        let trail_group = group.clone();
        let content = if minimal {
            content.pr(px(reserve))
        } else {
            content
        };
        let content = content.into_any_element();
        #[cfg(debug_assertions)]
        let content = qa_content_harness::wrap_content_qa_content(content, qa_row_observer.clone());
        let row_content = if minimal {
            rsx! {
                <div relative min_w_0>
                    {content}
                    {render_minimal_trail(
                    index,
                    row.item.is_top,
                    &metadata,
                    process_icon,
                    actions,
                    &settings,
                    &tokens,
                    self.dark,
                    trail_group,
                )}
                </div>
            }
        } else {
            div()
                .min_w_0()
                .child(render_row_header(
                    index,
                    &metadata,
                    process_icon,
                    actions,
                    &settings,
                    &tokens,
                    self.dark,
                ))
                .child(content)
        };
        let element = row_chrome_with_wallpaper(
            SharedString::from(id),
            group,
            &settings,
            &tokens,
            self.dark,
            selected,
            self.selected_id == row.item.id,
            row.item.is_top,
            // A divider touching the next row's selected fill is hidden too.
            index + 1 == self.page.rows.len()
                || self.page.rows.get(index + 1).is_some_and(|next| {
                    self.selected_ids.contains(&next.item.id)
                }),
            self.snapshot,
            self.window_background.snapshot().is_some(),
        )
        .cursor_pointer()
        .when(self.model.host().is_some() && cfg!(windows), |row| {
            row.on_drag(drag, move |drag: &crate::favorite_drag::ContentDrag, _, window, cx| {
                let _ = drag_owner.update(cx, |this, cx| this.start_history_drag(drag, window, cx));
                cx.new(|_| drag.clone())
            })
        })
        .on_mouse_down(MouseButton::Right, move |_, _, cx| {
            cx.stop_propagation();
            let _ = menu_view.update(cx, |this, cx| {
                if this.generation == generation
                    && this
                        .page
                        .rows
                        .get(index)
                        .is_some_and(|row| row.item.id == menu_id)
                {
                    this.open_row_menu(index, cx);
                }
            });
        })
        .on_click(move |event, window, cx| {
            let _ = view.update(cx, |this, cx| {
                if this.native_dragging
                    || this
                        .native_suppress_click
                        .is_some_and(|until| Instant::now() < until)
                {
                    return;
                }
                this.cancel_native_hover();
                if this.generation != generation
                    || !this
                        .page
                        .rows
                        .get(index)
                        .is_some_and(|row| row.item.id == click_id)
                {
                    return;
                }
                let modifiers = event.modifiers();
                this.click_select(index, modifiers.control, modifiers.shift);
                this.focus.focus(window);
                if this.editor_followed_selection.is_some() && this.editor_is_open(cx) {
                    this.sync_editor_selection(cx);
                    // The separate editor protects global hook routing. Give
                    // this explicitly clicked list real focus for arrow keys.
                    cx.defer_in(window, |this, window, cx| {
                        if this.exit_pending || !this.editor_is_open(cx) {
                            return;
                        }
                        // Isolated headless windows deliberately never take
                        // foreground focus, including list/editor switching.
                        if cliphost_core::is_headless() {
                            return;
                        }
                        if let Some(host) = this.model.host() {
                            if let Err(error) = NativeWindow::request_search_focus(window, &host) {
                                this.error = Some(error);
                                cx.notify();
                            }
                        }
                    });
                    cx.notify();
                    return;
                }
                match history_click_route(this.lan_companion_is_open(cx), event) {
                    HistoryClickRoute::Lan => {
                        // Capture the right pane's current destination in this
                        // click, before queuing any asynchronous observer.
                        let target = this.lan_companion
                            .and_then(|handle| handle.read(cx).ok())
                            .ok_or_else(|| tr("互传窗口已关闭。", "LAN transfer is closed.").to_string())
                            .and_then(|lan| lan.history_click_target(cx));
                        match target {
                            Ok(target) => this.send_history_to_lan_target(vec![click_id.clone()], 1, target, cx),
                            Err(error) => this.show_notice(error, cx),
                        }
                        this.restore_no_focus(window);
                        cx.notify();
                        return;
                    }
                    HistoryClickRoute::SelectionOnly => {
                        this.restore_no_focus(window);
                        cx.notify();
                        return;
                    }
                    HistoryClickRoute::List => {}
                }
                this.restore_no_focus(window);
                let mode = this.configuration["clipListExecuteMode"]
                    .as_i64()
                    .unwrap_or(1);
                if modifiers.control || modifiers.shift {
                    cx.notify();
                    return;
                }
                let count = event.click_count();
                if count == 1 && mode == 1 || count == 2 && matches!(mode, 2 | 3) {
                    this.execute_selected("paste", cx);
                } else if count == 1 && mode == 2 {
                    this.execute_selected("copy", cx);
                }
                cx.notify();
            });
        })
        .child(row_content)
        .children(appearance::render_row_number(
            index, self.keyboard_paste_active, self.keyboard_paste_amount,
            self.keyboard_paste_reason.as_deref().unwrap_or_default(), &tokens, self.dark,
        ))
        .into_any_element();
        let element = hover_binding::wrap_hover_row(
            element,
            hover_binding,
            Rc::new(move |event, window, cx| {
                let _ = hover_owner.update(cx, |this, cx| {
                    this.hover_history(
                        index,
                        &hover_id,
                        generation,
                        hover_session,
                        event,
                        window,
                        cx,
                    )
                });
            }),
        );
        #[cfg(debug_assertions)]
        let element = qa_content_harness::wrap_content_qa_row(element, qa_row_observer);
        element
    }

    pub fn report(&self, window: &Window, cx: &gpui::App) -> Value {
        let (isolated, keyboard) = match &self.model {
            UiModel::Live(service) => {
                let host = service.state();
                let state = host.hide_snapshot();
                (
                    service.isolated(),
                    json!({"hostHwnd":host.host_hwnd(), "visible":state.visible,
                    "hasFakeFocus":state.has_fake_focus, "noFocusSessionActive":state.no_focus_session_active,
                    "keyboardEngaged":cliphost_core::hide_policy::keyboard_engaged(&state)}),
                )
            }
            _ => (true, Value::Null),
        };
        let stats = self.stats.lock().unwrap();
        let bounds = self.list.viewport_bounds();
        let row_bounds: Vec<Value> = (0..self.page.rows.len().min(7))
            .filter_map(|index| {
                self.list.bounds_for_item(index).map(|bounds| {
                    json!({"index":index,
                "x":f32::from(bounds.origin.x), "y":f32::from(bounds.origin.y),
                "width":f32::from(bounds.size.width),"height":f32::from(bounds.size.height)})
                })
            })
            .collect();
        let mut report = json!({ "mode": if matches!(self.model, UiModel::Live(_)) {"live"} else {"fixture"}, "theme": if self.dark {"dark"} else {"light"},
            "language": i18n::displayed_language().wire_name(), "searchPlaceholder": "",
            "categoryLabels": self.tabs.iter().map(|tab| &tab.label).collect::<Vec<_>>(),
            "category": self.category, "query": self.query, "total": self.page.total,
            "returnedRows": self.page.rows.len(), "viewRenderCount": stats.view_renders,
            "rowsBuilt": stats.rows_built, "uniqueRowsBuilt": stats.row_ids.len(),
            "firstViewRenderMs": stats.first_view_ms, "lastQueryMs": self.last_query_ms,
            "width": f32::from(window.viewport_size().width), "height": f32::from(window.viewport_size().height),
            "scaleFactor": window.scale_factor(), "noFocus": self.no_focus,
            "snapshot":self.snapshot,"listViewport":{"x":f32::from(bounds.origin.x),"y":f32::from(bounds.origin.y),
                "width":f32::from(bounds.size.width),"height":f32::from(bounds.size.height)},"rowBounds":row_bounds,
            "nativeHostStarted": matches!(self.model, UiModel::Live(_)), "isolated": isolated,
            "productionHostStarted": matches!(self.model, UiModel::Live(_)) && !isolated,
            "keyboard": keyboard, "windowState": self.window_state,
            "historyReturnTop":{"buttonVisible":self.history_away_from_top,
                "itemIndex":self.list.logical_scroll_top().item_ix,
                "offset":f32::from(self.list.logical_scroll_top().offset_in_item),
                "selectedIds":self.selected_ids,"activeId":self.selected_id},
            "input": self.search.read(cx).diagnostics(), "error": self.error,
            "selectedIds": self.selected_ids, "notice": self.notice,
            "listAppearance":{"slot":AppearanceSettings::from_config(&self.configuration).slot.as_str(),
                "density":AppearanceSettings::from_config(&self.configuration).density.as_str(),
                "scope":"clipboard list; favorites use their existing header layout"},
            "processFilters":{"loaded":self.process_loaded,"initialized":self.process_initialized,
                "activeId":self.process_selection.active_id,"spec":self.process_selection.spec(&self.process_presets),
                "presetCount":self.process_presets.len(),"editorOpen":self.process_editor.is_some(),
                "loadPending":self.process_loading,"scope":"shared core snapshot; old Web origins import when opened"},
            "menuLease": self.menu_lease.as_ref().map(|(lease, _)| *lease),
            "editor": self.editor_window.and_then(|editor| editor.read(cx).ok().map(|editor| editor.diagnostics(cx))),
            "lanCompanion": self.lan_companion.and_then(|lan| lan.read(cx).ok().map(LanCompanionWindow::diagnostics)),
            "settings": self.settings_window.and_then(|settings| settings.read(cx).ok().map(|settings| settings.diagnostics(cx))),
            "contentResources": {"loaded":self.content_cache.len(),"pending":self.content_cache.pending_len(),"retainedBytes":self.content_cache.retained_bytes()},
            "toast": self.toast.and_then(|toast| toast.read(cx).ok().map(CopyToastOverlay::diagnostics)) });
        report["keyboardPasteStatus"] = json!({
            "active": self.keyboard_paste_active,
            "inactiveReason": self.keyboard_paste_reason,
            "visible": self.keyboard_paste_visible,
            "presentation": "row-index", "shortcutCount": self.page.rows.len().min(10),
            "hintVisible": false, "popupShown": false, "popupHwnd": 0,
        });
        if let Some(host) = self.model.host() {
            report["clipboardKeyboardMode"] = json!(format!("{:?}", host.clipboard_keyboard_mode()));
        }
        drop(stats);
        #[cfg(debug_assertions)]
        let report = {
            let mut report = report;
            report["favoriteFeedbackPresent"] = json!(self.favorite_feedback.is_some());
            report["favoriteUndoAvailable"] = json!(self.favorite_save_undo.is_some());
            report["focus"] = json!({
                "primaryPage": if self.favorites_active { "favorites" } else { "clipboard" },
                "historyList": self.focus.is_focused(window),
                "historySearch": self.search.read(cx).focus_handle().is_focused(window),
                "favorites": self.favorites.as_ref().map(|view| view.read(cx).search_focus_report(window, cx)),
            });
            report["listSettings"] = self.list_settings_diagnostics(window, cx);
            report["filterPopover"] = self.filter_popover_diagnostics(window, cx);
            if self.qa_harness.is_some() {
                let qa = self.content_qa_report(window, cx);
                report["qa"] = qa["qa"].clone();
                report["qaHarness"] = qa["qaHarness"].clone();
            }
            report
        };
        report
    }
}

impl Render for ClipboardPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::content_cache_owners::register(cx, |this, cx| {
            if this.favorites_active || this.model.host().is_some_and(|host| {
                let state = host.hide_snapshot(); !state.visible || state.is_collapsed
            }) {
                this.content_cache.set_visible_items(std::iter::empty::<&str>());
            }
            let changed = this.content_cache.apply_budget_pressure();
            this.content_cache.release_evicted(cx);
            if changed { cx.notify(); }
        });
        self.sync_history_top_button(cx);
        if let Some(host)=self.model.host() {
            crate::favorite_drag::register_surface(window,host,"clipboard",cx);
        }
        self.refresh_theme(window, cx);
        self.sync_editor_selection(cx);
        self.sync_overlay_guard();
        self.sync_native_interactions(cx);
        self.sync_keyboard_paste_status(window, cx);
        let keyboard_motion_now = Instant::now();
        self.keyboard_paste_amount = self.keyboard_paste_motion.value((),
            if self.keyboard_paste_active { 1.0 } else { 0.0 },
            crate::motion::HOVER, crate::motion::EASE, keyboard_motion_now);
        if self.keyboard_paste_motion.any_active(keyboard_motion_now) {
            window.request_animation_frame();
        }
        let language = i18n::displayed_language();
        if self.language != language {
            self.language = language;
            self.search.update(cx, |input, cx| {
                input.set_placeholder("", cx);
            });
            if let Some((fields, _)) = &self.filter_panel {
                for (field, (zh, en)) in fields.iter().zip([
                    ("开始 YYYY-MM-DD", "Start YYYY-MM-DD"),
                    ("结束 YYYY-MM-DD", "End YYYY-MM-DD"),
                    ("日期 YYYY-MM-DD", "Date YYYY-MM-DD"),
                ]) {
                    field.update(cx, |input, cx| input.set_placeholder(tr(zh, en), cx));
                }
            }
            self.sync_tabs();
            // Menu geometry and labels are a snapshot of the opening language.
            self.close_menu(cx);
            self.notice = None;
            self.favorite_feedback = None;
            self.favorite_feedback_generation = self.favorite_feedback_generation.wrapping_add(1);
            self.favorite_save_undo = None;
            self.notice_task = None;
        }
        if self.stats_enabled {
            let mut stats = self.stats.lock().unwrap();
            stats.view_renders += 1;
            stats
                .first_view_ms
                .get_or_insert_with(|| self.started.elapsed().as_secs_f64() * 1_000.0);
        }
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let l = &tokens.layout;
        let viewport_width = f32::from(window.viewport_size().width);
        // Clipboard and favorites share the compact titlebar search.
        let titlebar_metrics =
            crate::titlebar_layout::history_titlebar_layout(viewport_width, &tokens);
        let wallpaper = self.window_background.snapshot();
        let wallpaper_layer = wallpaper.as_ref().map(|image| {
            crate::window_background::render_background_image(image, self.window_background.options())
        });
        let wallpaper_tint = wallpaper.as_ref().map(|_| {
            div().absolute().inset_0().bg(crate::window_glass::page_tint(
                self.tokens.theme(self.dark), self.window_background.options(),
            ))
        });
        let (titlebar_background, titlebar_border) = if wallpaper.is_some() {
            crate::window_glass::titlebar_colors(
                self.tokens.theme(self.dark), self.window_background.options(),
            )
        } else {
            (color(&t.surface), color(&t.border))
        };
        let paused = self.window_state["isMonitorPaused"].as_bool() == Some(true);
        let pinned_window = self.window_state["isPinned"].as_bool() == Some(true);
        let neutral = color(&t.neutral_active);
        // Search lives in the caption; categories use their own row (2026-10-15).
        // Only the window width decides what fits; focus and typing never
        // reflow the row (user feedback: morphing chips on focus felt odd).
        let search_focused = self.search.read(cx).focus_handle().is_focused(window);
        let process_tabs = preset_tabs(&self.process_presets, &self.process_selection);
        // 默认 (the plain "all" tab) and 置顶 stay icon-only; their names are tooltips.
        let default_tab = self.tabs.iter().position(|tab| tab.request_type == "all");
        let icon_only_tab =
            |index: usize| Some(index) == default_tab || self.tabs[index].is_pinned();
        let chip_width = |label: &str| {
            label_width(
                label,
                tokens.typography.category_font_size,
                &tokens.typography.font_family,
                window,
            ) + 2.0 * l.chip_padding_x
        };
        let mut strip_widths: Vec<f32> = (0..self.tabs.len())
            .map(|index| {
                if icon_only_tab(index) {
                    l.icon_button_size
                } else {
                    chip_width(&self.tabs[index].label)
                }
            })
            .collect();
        // Process presets keep their text and a one-pixel outline.
        strip_widths.extend(process_tabs.iter().map(|tab| chip_width(&tab.label) + 2.0));
        let mut strip_required: Vec<bool> =
            self.tabs.iter().map(|tab| tab.key == self.category).collect();
        strip_required.extend(process_tabs.iter().map(|tab| tab.active));
        let strip_available = viewport_width
            - 2.0 * l.content_padding_x
            - l.icon_button_size
            - l.toolbar_gap
            - 4.0;
        let strip_visible =
            visible_strip_entries(&strip_widths, &strip_required, strip_available, l.chip_gap);
        let tab_count = self.tabs.len();
        let weak = cx.weak_entity();
        let list = list(self.list.clone(), move |index, window, cx| {
            weak.update(cx, |this, cx| {
                let row = this.page.rows[index].clone();
                this.row(index, row, window, cx)
            })
            .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h_0()
        .w_full();
        let weak = cx.weak_entity();
        let scrollbar_session = self.native_session;
        let history_scrollbar = self.history_scrollbar.render(
            "history-scrollbar",
            ScrollbarTarget::List(self.list.clone()),
            self.native_session,
            ScrollbarStyle {
                ink: color(&t.text),
                metrics: tokens.scrollbar,
                always_visible: self.snapshot,
            },
            Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.favorites_active || this.native_session != scrollbar_session {
                        return;
                    }
                    this.cancel_native_hover();
                    // Scrollbar jumps can reach an unmeasured end without a
                    // wheel event, so preserve the existing lazy-load trigger.
                    this.history_idle.activity(Instant::now());
                    this.sync_history_top_button(cx);
                    if scrollbar::visible_end(&this.list).saturating_add(20) >= this.page.rows.len() {
                        this.load_more(cx);
                    }
                    cx.notify();
                });
            }),
        );
        let views = [
            ("clipboard", "剪贴板", "Clipboard"),
            ("favorites", "收藏", "Favorites"),
        ];
        let process_owner = cx.weak_entity();
        let process_handler: crate::process_filters::ProcessTabHandler =
            Rc::new(move |id, window, cx| {
                let _ = process_owner.update(cx, |this, cx| {
                    this.more_menu = false;
                    this.process_selection.toggle(&id, &this.process_presets);
                    this.focus.focus(window);
                    this.restore_no_focus(window);
                    this.refresh(cx);
                });
            });
        let filter_active = self.advanced.is_active() || self.filter_panel.is_some();
        let search_box = div()
            .relative()
            // GPUI also dispatches native caption presses as MouseDown before
            // Windows starts moving. Keep propagation/default intact for drag.
            .on_mouse_down_out(cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                if event.button == MouseButton::Left {
                    this.blur_search_from_pointer(window, cx);
                }
            }))
            // The input and filter handle their own presses. Padding belongs
            // to the search too; do not let the page's tracked focus take it.
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                this.focus_search(window, cx);
                cx.stop_propagation();
            }))
            .child(caption_exclusion())
            .flex()
            .items_center()
            .h(px(l.search_height))
            .flex_none()
            .w(px(titlebar_metrics.search_width))
            .rounded(px(l.search_radius))
            .bg(color(&t.secondary))
            .border_1()
            .border_color(if search_focused {
                color(&t.border_secondary)
            } else {
                gpui::transparent_black()
            })
            .pl(px(2.0))
            .pr(px(8.0))
            .gap(px(6.0))
            .child({
                let hover = color(&t.hover);
                rsx! {
                    <div id={"search-filter"} relative size={px(l.inline_icon_button_size)}
                        flex_none flex items_center justify_center rounded={px(l.chip_radius)}
                        cursor_pointer />
                }
                    .hover(move |style| style.bg(hover))
                    .child(self.icon(
                        "filter",
                        l.action_icon_size,
                        color(if filter_active { &t.text } else { &t.text_muted }),
                    ))
                    // Active filters show a dot instead of an accent tint.
                    .when(filter_active, |element| {
                        element.child(
                            rsx! {
                                <div absolute top={px(4.0)} right={px(4.0)} size={px(5.0)}
                                    rounded_full bg={color(&t.text)} />
                            },
                        )
                    })
                    .tooltip(preference_tooltip(tr("筛选", "Filter").into(), t.clone()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.more_menu = false;
                        this.toggle_filter_panel(window, cx)
                    }))
            })
            .child(
                rsx! {
                    <div flex_1 min_w_0>
                        {self.search.clone()}
                    </div>
                }
                    // Input selection stops bubbling. Native focus handoff must
                    // happen in capture so clicking the text also activates IME.
                    .capture_any_mouse_down(cx.listener(
                        |this, event: &gpui::MouseDownEvent, window, cx| {
                            if event.button == MouseButton::Left {
                                this.focus_search(window, cx);
                            }
                        },
                    )),
            );
        let titlebar_search = if let Some(favorites) = self.favorites.as_ref().filter(|_| self.favorites_active) {
            favorites.update(cx, |favorites, cx| favorites.titlebar_search(titlebar_metrics.search_width, window, cx))
        } else {
            search_box.into_any_element()
        };
        let titlebar = rsx! {
            <div relative flex items_center justify_between h={px(l.title_bar_height)}
                px={px(l.title_bar_padding_x)} gap={px(l.title_bar_gap)} flex_none
                bg={titlebar_background} border_b_1 border_color={titlebar_border} />
        }
            .child(
                gpui::canvas(
                    |bounds, window, _| platform::record_caption_frame(window, bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(
                rsx! {
                    <div flex items_center gap={px(l.chip_gap)} flex_none min_w_0 overflow_hidden />
                }
                    .children(views.into_iter().map(|(key, zh, en)| {
                        // Icon navigation keeps more caption space for dragging.
                        let active = (key == "clipboard") != self.favorites_active;
                        let hover = color(&t.hover);
                        rsx! {
                            <div id={key} relative>
                                {caption_exclusion()}
                            </div>
                        }
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let route = if key == "clipboard" {
                                    "/clipboard"
                                } else {
                                    "/favorites"
                                };
                                this.set_clipboard_route(route, window, cx)
                            }))
                            .on_drag_move(cx.listener(move |this,event:&gpui::DragMoveEvent<crate::favorite_drag::ContentDrag>,window,cx| {
                                if key!="favorites" || this.favorites_active || !event.bounds.contains(&event.event.position)
                                    || !event.dragged_item().downcast_ref::<crate::favorite_drag::ContentDrag>()
                                        .is_some_and(|drag|matches!(drag.source,crate::favorite_drag::DragSource::History{..})) {
                                    return;
                                }
                                if this.drag_tab_hover.is_none() {
                                    let began=Instant::now(); this.drag_tab_hover=Some(began);
                                    let owner=window.window_handle(); let bounds=event.bounds;
                                    cx.spawn(async move |this,cx| {
                                        Timer::after(Duration::from_millis(500)).await;
                                        let _=owner.update(cx,|_,window,cx| {
                                            let _=this.update(cx,|this,cx| {
                                                if this.drag_tab_hover==Some(began) {
                                                    this.drag_tab_hover=None;
                                                    if crate::favorite_drag::active(cx) && bounds.contains(&window.mouse_position()) {
                                                        this.set_clipboard_route("/favorites",window,cx);
                                                    }
                                                }
                                            });
                                        });
                                    }).detach();
                                }
                            }))
                            .size(px(tokens.titlebar_search.button_size))
                            .rounded(px(l.chip_radius))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(tokens.typography.category_font_size))
                            .line_height(relative(1.2))
                            .text_color(color(if active { &t.text } else { &t.text_secondary }))
                            .when(active, |element| element.bg(neutral))
                            .when(!active, |element| element.hover(move |style| style.bg(hover)))
                            .tooltip(preference_tooltip(tr(zh, en).into(), t.clone()))
                            .child(self.icon(if key == "clipboard" { "clipboard" } else { "star" }, l.icon_size, color(if active { &t.text } else { &t.text_secondary })))
                    })),
            )
            .child(titlebar_search)
            .child(div().flex_1().min_w(px(titlebar_metrics.drag_width)))
            .child(
                rsx! {
                    <div flex flex_none items_center gap={px(l.chrome_button_gap)} />
                }
                    .child(
                        self.tool_button(
                            "pin",
                            "pin",
                            color(if pinned_window { &t.text } else { &t.text_muted }),
                            true,
                            t,
                        )
                        .when(pinned_window, |e| e.bg(neutral))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_native_hover();
                            this.window_action("setPinned", "isPinned", cx)
                        })),
                    )
                    .child(
                        self.tool_button("collapse", "collapse", color(&t.text_muted), true, t)
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.window_action("setCollapsed", "isCollapsed", cx)
                            })),
                    )
                    .child(
                        self.tool_button("close", "close", color(&t.text_muted), true, t)
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, _| this.dismiss(window))),
                    ),
            );
        let mut strip_tabs = Vec::new();
        let mut overflow_tabs = Vec::new();
        for index in 0..tab_count {
            if strip_visible[index] {
                strip_tabs.push(index);
            } else {
                overflow_tabs.push(index);
            }
        }
        let mut strip_processes = Vec::new();
        let mut overflow_processes = Vec::new();
        for (index, _) in process_tabs.iter().enumerate() {
            if strip_visible[tab_count + index] {
                strip_processes.push(index);
            } else {
                overflow_processes.push(index);
            }
        }
        let categories = rsx! {
            <div id={"category-bar"} min_w_0 overflow_hidden flex items_center
                gap={px(l.chip_gap)} flex_none />
        }
            .children(strip_tabs.iter().map(|&index| {
                let tab = &self.tabs[index];
                let key = tab.key.clone();
                let active = self.category == tab.key;
                let icon_only = icon_only_tab(index);
                let hover = color(&t.hover);
                let tint = color(if active { &t.text } else { &t.text_secondary });
                rsx! {
                    <div id={("category", index)} h={px(l.chip_height)}
                        rounded={px(l.chip_radius)} flex items_center justify_center flex_none />
                }
                    .when(icon_only, |element| {
                        element
                            .w(px(l.icon_button_size))
                            .child(self.icon(tab.icon, l.icon_size, tint))
                    })
                    .when(!icon_only, |element| {
                        element.px(px(l.chip_padding_x)).child(tab.label.clone())
                    })
                    .text_size(px(tokens.typography.category_font_size))
                    .text_color(tint)
                    .when(active, |element| element.bg(neutral))
                    .when(!active, |element| element.hover(move |style| style.bg(hover)))
                    .tooltip(preference_tooltip(tab.label.clone().into(), t.clone()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_category(key.clone(), window, cx)
                    }))
            }))
            .children(strip_processes.iter().map(|&index| {
                let tab = &process_tabs[index];
                let edit_id = tab.id.clone();
                render_preset_tab(tab, &tokens, self.dark, process_handler.clone())
                    .tooltip(preference_tooltip(
                        format!("{}\n{}", tab.tooltip, tr("右键编辑或删除", "Right-click to edit or delete")),
                        t.clone(),
                    ))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, window, cx| {
                        this.edit_process_preset(edit_id.clone(), window, cx)
                    }))
            }));
        let more_button = {
            let hover = color(&t.hover);
            rsx! {
                <div id={"category-more"} size={px(l.icon_button_size)} flex_none flex
                    items_center justify_center rounded={px(l.chip_radius)} cursor_pointer />
            }
                .hover(move |style| style.bg(hover))
                .when(self.more_menu || self.filter_popover_open(), |element| element.bg(neutral))
                .child(self.icon(
                    "more",
                    l.icon_size,
                    color(if self.more_menu || self.filter_popover_open() { &t.text } else { &t.text_muted }),
                ))
                .tooltip(preference_tooltip(
                    tr("更多分类与管理", "More categories").into(),
                    t.clone(),
                ))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.more_menu = !this.more_menu;
                    this.sync_overlay_guard();
                    cx.notify();
                }))
        };
        let toolbar = rsx! {
            <div flex items_center gap={px(l.toolbar_gap)} h={px(l.toolbar_height)} px={px(2.0)}
                flex_none>
                {categories}
                <div flex_1 />
                {more_button}
            </div>
        };
        let more_menu = self.more_menu.then(|| {
            self.more_menu_view(
                &overflow_tabs,
                overflow_processes
                    .iter()
                    .map(|&index| process_tabs[index].clone())
                    .collect(),
                process_handler.clone(),
                cx,
            )
        });
        let favorite_feedback = self.favorite_feedback_view(window, &tokens, cx);
        let editor_open = self.editor_is_open(cx);
        let lan_open = self.lan_companion_is_open(cx);
        let footer = rsx! {
            <div h={px(l.footer_height)} flex_none flex gap={px(l.chrome_button_gap)}
                items_center pl={px(4.0)} border_t_1 border_color={color(&t.border)}
                text_size={px(tokens.typography.small_font_size)}
                text_color={color(&t.text_muted)} />
        }
            .child(
                self.tool_button("pause", if paused { "play" } else { "pause" },
                    color(if paused { &t.warning } else { &t.text_muted }), false, t)
                    .tooltip(preference_tooltip(if paused { tr("继续记录", "Resume capture") } else { tr("暂停记录", "Pause capture") }.into(), t.clone()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.window_action("setMonitorPaused", "isMonitorPaused", cx)
                    })),
            )
            .child(
                rsx! {
                    <div flex_1 min_w_0 flex items_center gap={px(l.chrome_button_gap)}
                        overflow_hidden whitespace_nowrap />
                }
                    .when(paused, |element| element.child(
                        rsx! {
                            <div font_weight={FontWeight::MEDIUM} text_color={color(&t.warning)}
                                flex_none>
                                {tr("已暂停记录 ·", "Capture paused ·")}
                            </div>
                        },
                    ))
                    .child(div().min_w_0().text_ellipsis().child(if let Some(notice) = self.notice.as_ref().filter(|_| self.favorite_feedback.is_none()) {
                        notice.clone()
                    } else if self.loading && !self.background_refresh {
                        tr("搜索中…", "Searching…").to_string()
                    } else if self.selected_ids.len() > 1 {
                        cliphost_core::trf!("{} 条 · 已选 {}", "{} items · {} selected", self.page.total, self.selected_ids.len())
                    } else {
                        cliphost_core::trf!("{} 条", "{} items", self.page.total)
                    })),
            )
            .child(
                self.tool_button("history-add", "plus", color(&t.text_muted), false, t)
                    .tooltip(preference_tooltip(tr("添加", "Add").into(), t.clone()))
                    .on_click(cx.listener(|this, _, _, cx| this.open_history_dialog(HistoryDialog::AddText, cx))),
            )
            .child(
                self.tool_button("history-clear", "delete", color(&t.text_muted), false, t)
                    .tooltip(preference_tooltip(tr("清空非置顶", "Clear unpinned history").into(), t.clone()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_history_dialog(HistoryDialog::Clear(Some(ClearScope::NonTop)), cx);
                    })),
            )
            .child(
                self.tool_button("history-edit", "panel-right", color(if editor_open { &t.text } else { &t.text_muted }), false, t)
                    .when(editor_open, |button| button.bg(neutral))
                    .tooltip(preference_tooltip(if editor_open { tr("关闭编辑", "Close editor") } else { tr("打开编辑", "Open editor") }.into(), t.clone()))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_editor(cx))),
            )
            .child(
                self.tool_button("super-menu", "grid", color(&t.text_muted), false, t)
                    .tooltip(preference_tooltip(tr("超级菜单", "Super menu").into(), t.clone()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cancel_native_hover();
                        this.close_menu(cx);
                        if let Some(host) = this.model.host() {
                            if let Err(error) = host.request_super_menu() { this.error = Some(error); }
                        }
                        cx.notify();
                    })),
            )
            .child(
                self.tool_button("lan-share", "desktop", color(if lan_open { &t.text } else { &t.text_muted }), false, t)
                    .when(lan_open, |button| button.bg(neutral))
                    .tooltip(preference_tooltip(tr("局域网互传", "LAN sharing").into(), t.clone()))
                    .on_click(cx.listener(|this, _, _, cx| this.open_settings_route("/main/lan", cx))),
            )
            .child(
                self.tool_button(
                    "list-settings",
                    "sliders",
                    color(if self.list_settings.is_some() { &t.text } else { &t.text_muted }),
                    false,
                    t,
                )
                .when(self.list_settings.is_some(), |e| e.bg(neutral))
                .tooltip(preference_tooltip(
                    tr("列表显示设置", "List display").into(),
                    t.clone(),
                ))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_list_settings(window, cx))),
            )
            .child(
                self.tool_button("settings", "gear", color(&t.text_muted), false, t)
                    .tooltip(preference_tooltip(tr("设置", "Settings").into(), t.clone()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| this.open_settings_route("/main/settings", cx))),
            );
        let list_settings = self
            .list_settings
            .clone()
            .map(|form| self.list_settings_panel(form, window, cx));
        let filter_panel = self
            .filter_panel
            .as_ref()
            .map(|(fields, _)| fields.clone())
            .map(|fields| self.filter_panel_view(fields, window, cx));
        let tag_editor = self
            .tag_editor
            .as_ref()
            .map(|(editor, _)| editor.clone())
            .map(|editor| self.tag_editor_view(editor, window, cx));
        let process_editor = if self.process_editor.is_some() || self.process_open_pending {
            Some(
                self.process_editor_view(
                    self.process_editor
                        .as_ref()
                        .map(|(editor, _)| editor.clone()),
                    window,
                    cx,
                ),
            )
        } else {
            None
        };
        let (history_banner, history_dialog) = self
            .history_actions
            .clone()
            .map(|actions| {
                actions.update(cx, |actions, cx| {
                    (actions.banner_view(cx), actions.dialog_view(cx))
                })
            })
            .unwrap_or_default();
        let element = div()
            .capture_any_mouse_down(cx.listener(|this, _, _, _| {
                if !this.favorites_active {
                    this.history_idle.activity(Instant::now());
                }
            }))
            .capture_key_down(cx.listener(|this,event:&gpui::KeyDownEvent,window,cx| {
                if !this.favorites_active {
                    this.history_idle.activity(Instant::now());
                }
                if event.keystroke.key=="escape" && crate::favorite_drag::cancel_active(window,cx) {
                    this.native_suppress_click=Some(Instant::now()+Duration::from_millis(300));
                    window.prevent_default(); cx.stop_propagation();
                }
            }))
            .on_drag_move(cx.listener(|this,event:&gpui::DragMoveEvent<crate::favorite_drag::ContentDrag>,window,cx| {
                if crate::favorite_drag::route_drag_move(event,window,cx) {return;}
                let Some(source)=this.history_content_drag.clone().filter(|drag|drag.active()) else{return;};
                if !event.dragged_item().downcast_ref::<crate::favorite_drag::ContentDrag>()
                    .is_some_and(|drag|source.operation_id==drag.operation_id) {return;}
                let size=window.viewport_size(); let pos=event.event.position;
                let outside=pos.x<px(0.)||pos.y<px(0.)||pos.x>=size.width||pos.y>=size.height;
                if outside && !this.native_dragging {
                    #[cfg(windows)]
                    if let Some(native)=this.interactions.borrow_mut().as_mut() {
                        match native.start_content_drag(source) {
                            Ok(())=>this.native_dragging=true,Err(error)=>this.error=Some(error),
                        }
                    }
                }
            }))
            .key_context(if self.favorites_active {
                "ClipFavorites"
            } else if self.process_editor.is_some() || self.process_open_pending {
                "ClipProcessFilter"
            } else if self.history_dialog {
                "ClipHistoryDialog"
            } else if self.fifo_panel.is_some() {
                "ClipFifo"
            } else {
                "ClipPreview"
            })
            // Favorites owns its list focus. Tracking the history handle here
            // would steal it back when caption or page padding is clicked.
            .when(!self.favorites_active, |element| element.track_focus(&self.focus))
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.cancel_native_hover()))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key=="escape" && crate::favorite_drag::cancel_active(window,cx) {
                    this.native_suppress_click=Some(Instant::now()+Duration::from_millis(300));
                    cx.stop_propagation(); return;
                }
                this.cancel_native_hover();
                if this.process_editor.is_some() || this.process_open_pending {
                    if event.keystroke.key == "escape"
                        && !event.is_held
                        && event.keystroke.modifiers.number_of_modifiers() == 0
                    {
                        if let Some((editor, _)) = &this.process_editor {
                            editor.update(cx, |editor, cx| editor.escape(window, cx));
                        } else {
                            this.close_process_editor(window, cx);
                        }
                        cx.stop_propagation();
                    }
                    return;
                }
                if let Some(actions) = &this.history_actions {
                    if actions.update(cx, |actions, cx| actions.key_down(event, window, cx)) {
                        cx.stop_propagation();
                        return;
                    }
                }
                if this.history_dialog {
                    return;
                }
                if let Some((panel, _, _)) = &this.fifo_panel {
                    if panel.update(cx, |panel, cx| panel.key_down_in(event, window, cx)) {
                        cx.stop_propagation();
                        return;
                    }
                }
                if this.favorites_active {
                    return;
                }
                if this.more_menu {
                    if event.keystroke.key == "escape" && !event.is_held {
                        this.close_more_menu(cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.list_settings.is_some() {
                    if event.keystroke.key == "escape" && !event.is_held
                        && this.list_settings.as_ref().is_some_and(|form| form.read(cx).list_settings_can_close(cx)) {
                        this.close_list_settings(window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.filter_panel.is_some() || this.tag_editor.is_some() {
                    return;
                }
                let modifiers = &event.keystroke.modifiers;
                if this.focus.is_focused(window)
                    && modifiers.control && modifiers.number_of_modifiers() == 1
                    && event.keystroke.key == "a"
                    && !this.search.read(cx).is_composing()
                {
                    if !event.is_held {
                        this.navigate_key(crate::history_keyboard::Key::SelectAll, false, window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if (this.focus.is_focused(window) || this.search.read(cx).focus_handle().is_focused(window))
                    && !this.search.read(cx).is_composing()
                    && modifiers.number_of_modifiers() == u8::from(modifiers.shift)
                {
                    if event.keystroke.key == "tab" && !event.is_held {
                        this.set_clipboard_route(
                            crate::history_keyboard::primary_tab_route(false), window, cx,
                        );
                        cx.stop_propagation();
                        return;
                    }
                    if this.focus.is_focused(window)
                        && matches!(event.keystroke.key.as_str(), "left" | "right" | "space" | " ")
                    {
                        if let Some(key) = crate::history_keyboard::Key::from_name(&event.keystroke.key) {
                            this.navigate_key(key, modifiers.shift, window, cx);
                            cx.stop_propagation();
                            return;
                        }
                    }
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "escape")
                    && event.keystroke.modifiers.number_of_modifiers() == 0
                    && this.focus.is_focused(window)
                {
                    if !event.is_held && !this.search.read(cx).is_composing() {
                        if event.keystroke.key == "enter" {
                            this.execute_selected("paste", cx);
                        } else {
                            this.dismiss(window);
                        }
                    }
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, window, cx| {
                if let Some((panel, _, _)) = &this.fifo_panel {
                    if panel.update(cx, |panel, cx| panel.key_up_in(event, window, cx)) {
                        cx.stop_propagation();
                    }
                }
            }))
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &Next, window, cx| this.navigate(1, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &Previous, window, cx| this.navigate(-1, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &First, window, cx| this.navigate(-1, true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Last, window, cx| this.navigate(1, true, window, cx)))
            .on_action(cx.listener(|this, _: &RangeNext, window, cx| {
                this.navigate_key(crate::history_keyboard::Key::Down, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RangePrevious, window, cx| {
                this.navigate_key(crate::history_keyboard::Key::Up, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RangeFirst, window, cx| {
                this.navigate_key(crate::history_keyboard::Key::Home, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RangeLast, window, cx| {
                this.navigate_key(crate::history_keyboard::Key::End, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Close, window, cx| {
                if !this.search.read(cx).is_composing() {
                    this.dismiss(window);
                }
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.execute_selected("paste", cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.execute_selected("copy", cx)))
            .on_action(cx.listener(|this, _: &Delete, _, cx| this.execute_selected("delete", cx)))
            .on_action(cx.listener(|this, _: &Pin, _, cx| this.execute_selected("pin", cx)))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .font_family(tokens.typography.font_family.clone())
            .text_size(px(tokens.typography.font_size))
            .line_height(relative(tokens.typography.line_height))
            .text_color(color(&t.text))
            .bg(color(&self.tokens.theme(self.dark).background))
            .relative()
            .children(wallpaper_layer)
            .children(wallpaper_tint)
            .child(titlebar)
            .child(crate::favorite_drag::surface_events())
            .when(self.favorites_active, |element| {
                element.children(self.favorites.clone())
            })
            .when(!self.favorites_active, |element| {
                element.child(
                    rsx! {
                        <div flex_1 min_h_0 flex flex_col px={px(l.content_padding_x)}
                            py={px(l.content_padding_y)}>
                            {toolbar}
                        </div>
                    }
                        .child(
                            rsx! {
                                <div flex_1 min_h_0 relative flex flex_col
                                    bg={color(&t.background)} />
                            }
                                .when(self.native_drag_over.is_some(), |list| {
                                    list.border_2().border_color(color(&t.accent))
                                })
                                .pt(px(l.list_padding_top))
                                .pb(px(l.list_padding_bottom))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(|this, _, _, cx| {
                                        this.open_menu(
                                            vec![],
                                            vec![
                                                MenuAction::row(
                                                    "scroll-top",
                                                    tr("回到开头", "Scroll to top"),
                                                ),
                                                MenuAction::row(
                                                    "add-text",
                                                    tr("添加文本", "Add text"),
                                                ),
                                                MenuAction::row(
                                                    "clear-history",
                                                    tr("清空", "Clear history"),
                                                ),
                                            ],
                                            cx,
                                        )
                                    }),
                                )
                                .when(self.page.rows.is_empty(), |element| {
                                    element.child(
                                        rsx! {
                                            <div p_4 text_color={color(&t.text_muted)}>
                                                {tr("暂无剪贴板记录", "No clipboard history")}
                                            </div>
                                        },
                                    )
                                })
                                .child(list)
                                .when(self.history_away_from_top, |element| {
                                    let now = Instant::now();
                                    let hover = self.history_top_motion.value((),
                                        if self.history_top_hovered { 1.0 } else { 0.0 },
                                        crate::motion::HOVER, crate::motion::EASE, now);
                                    if self.history_top_motion.any_active(now) { window.request_animation_frame(); }
                                    let background = crate::motion::mix_hsla(self.popover_surface(), color(&t.secondary), hover);
                                    let button = rsx! {
                                        <div id={"history-scroll-top"} size={px(32.0)} flex_none
                                            flex items_center justify_center rounded_full border_1
                                            border_color={color(&t.border)} bg={background}
                                            text_color={color(&t.text)} shadow_md cursor_pointer occlude>
                                            {self.icon("arrow-up", 16.0, color(&t.text))}
                                        </div>
                                    }
                                    .on_hover(cx.listener(|this, hovered, _, cx| {
                                        this.history_top_hovered = *hovered;
                                        cx.notify();
                                    }))
                                    .tooltip(preference_tooltip(tr("回到顶部", "Back to top").into(), t.clone()))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.history_idle.activity(Instant::now());
                                        this.return_history_to_top(cx);
                                        cx.stop_propagation();
                                    }));
                                    element.child(
                                        rsx! {
                                            <div absolute left_0 right_0 bottom={px(10.0)} flex justify_center>
                                                {button}
                                            </div>
                                        }
                                    )
                                })
                                .child(history_scrollbar),
                        )
                        .when(self.error.is_some(), |element| {
                            element.child(
                                div()
                                    .text_color(color(&t.error))
                                    .child(self.error.clone().unwrap_or_default()),
                            )
                        })
                        .children(history_banner)
                        .child(footer),
                )
            })
            .relative()
            .children(favorite_feedback)
            .children(list_settings)
            .children(filter_panel)
            .children(more_menu)
            .children(tag_editor)
            .children(self.fifo_panel.as_ref().map(|(panel, _, _)| panel.clone()))
            .children(history_dialog)
            .children(process_editor)
            .into_any_element();
        #[cfg(debug_assertions)]
        let element =
            qa_content_harness::wrap_content_qa(element, self.content_qa_paint_observer());
        element
    }
}

/// Preserve the successful prefix when a later selected item fails to delete.
fn confirmed_deletions(
    ids: &[String],
    mut run: impl FnMut(&str) -> Result<Value, String>,
) -> (HashSet<String>, Option<String>) {
    let mut removed = HashSet::new();
    for id in ids {
        match run(id) {
            Ok(_) => { removed.insert(id.clone()); }
            Err(error) => return (removed, Some(error)),
        }
    }
    (removed, None)
}

fn each_id(
    ids: &[String],
    mut run: impl FnMut(&str) -> Result<Value, String>,
) -> Result<Value, String> {
    for id in ids {
        run(id)?;
    }
    Ok(json!({ "ok": true }))
}

/// (active id, ordered selected ids, anchor index). A plain click selects one
/// row, Ctrl toggles a row in selection order and Shift extends from the
/// anchor in list order; the selection never becomes empty through toggling.
type SelectionState = (String, Vec<String>, Option<usize>);

#[derive(Debug, PartialEq, Eq)]
enum HistoryClickRoute {
    List,
    SelectionOnly,
    Lan,
}

fn history_click_route(lan_open: bool, event: &gpui::ClickEvent) -> HistoryClickRoute {
    if !lan_open { return HistoryClickRoute::List; }
    match event {
        gpui::ClickEvent::Mouse(event)
            if event.down.button == MouseButton::Left && event.up.button == MouseButton::Left
                && event.down.click_count == 1
                && !event.down.modifiers.modified() && !event.up.modifiers.modified() => HistoryClickRoute::Lan,
        _ => HistoryClickRoute::SelectionOnly,
    }
}

fn click_selection(
    rows: &[&str],
    (active, selected, anchor): &mut SelectionState,
    index: usize,
    toggle: bool,
    range: bool,
) {
    let Some(id) = rows.get(index).map(|id| id.to_string()) else {
        return;
    };
    if range {
        let from = anchor
            .filter(|anchor| *anchor < rows.len())
            .unwrap_or(index);
        *selected = rows[from.min(index)..=from.max(index)]
            .iter()
            .map(|id| id.to_string())
            .collect();
        *active = id;
    } else if toggle {
        match selected.iter().position(|current| *current == id) {
            Some(position) if selected.len() > 1 => {
                selected.remove(position);
                if *active == id {
                    *active = selected.last().cloned().unwrap_or_default();
                }
            }
            Some(_) => {}
            None => {
                selected.push(id.clone());
                *active = id;
            }
        }
        *anchor = Some(index);
    } else {
        *active = id.clone();
        *selected = vec![id];
        *anchor = Some(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_resource_keys_ignore_theme_layout_and_row_presentation() {
        let mut record = cliphost_core::model::ClipRecord::new_text("fixture", "fixture");
        record.kind = cliphost_core::model::ClipKind::ImageClipItem;
        let mut row = PreviewRow {
            item: cliphost_core::dto::clip_to_dto(&record, None),
            preview_text: String::new(),
            thumbnail: None,
        };
        let projection = ClipboardPreview::content_projection(&row, ContentMode::Standard);
        let style = ClipboardPreview::content_resource_style(&row, ContentMode::Standard, "#ffffff");
        assert_eq!(style, (ContentMode::Standard, ""));
        row.item.title.push_str(" changed");
        row.item.is_top = !row.item.is_top;
        row.item.highlighted_text = "search hit".into();
        assert_eq!(ClipboardPreview::content_projection(&row, ContentMode::Minimal), projection);
        assert_eq!(ClipboardPreview::content_resource_style(&row, ContentMode::Minimal, "#111111"), style);
    }

    #[test]
    fn lan_history_clicks_replace_list_execution_only_for_the_first_plain_left_click() {
        let click = |count, button, down, up| gpui::ClickEvent::Mouse(gpui::MouseClickEvent {
            down: gpui::MouseDownEvent { button, click_count: count, modifiers: down, ..Default::default() },
            up: gpui::MouseUpEvent { button, modifiers: up, ..Default::default() },
        });
        let plain = click(1, MouseButton::Left, Default::default(), Default::default());
        assert_eq!(history_click_route(true, &plain), HistoryClickRoute::Lan);
        assert_eq!(history_click_route(false, &plain), HistoryClickRoute::List);
        for count in [2, 3] {
            let repeat = click(count, MouseButton::Left, Default::default(), Default::default());
            assert_eq!(history_click_route(true, &repeat), HistoryClickRoute::SelectionOnly);
            assert_eq!(history_click_route(false, &repeat), HistoryClickRoute::List);
        }
        for modifiers in [
            gpui::Modifiers { control: true, ..Default::default() },
            gpui::Modifiers { shift: true, ..Default::default() },
            gpui::Modifiers { alt: true, ..Default::default() },
            gpui::Modifiers { platform: true, ..Default::default() },
        ] {
            for (down, up) in [(modifiers, modifiers), (modifiers, Default::default()), (Default::default(), modifiers)] {
                assert_eq!(history_click_route(true, &click(1, MouseButton::Left, down, up)), HistoryClickRoute::SelectionOnly);
            }
        }
        for button in [MouseButton::Right, MouseButton::Middle] {
            assert_eq!(history_click_route(true, &click(1, button, Default::default(), Default::default())), HistoryClickRoute::SelectionOnly);
        }
        assert_eq!(history_click_route(true, &gpui::ClickEvent::default()), HistoryClickRoute::SelectionOnly);
    }

    #[test]
    fn deletion_failure_keeps_the_failed_and_unattempted_rows() {
        let ids = vec!["deleted".into(), "failed".into(), "unattempted".into()];
        let mut attempted = Vec::new();
        let (removed, error) = confirmed_deletions(&ids, |id| {
            attempted.push(id.to_owned());
            if id == "failed" { Err("write failed".into()) } else { Ok(json!({"ok": true})) }
        });
        assert_eq!(attempted, ["deleted", "failed"]);
        assert_eq!(removed, HashSet::from(["deleted".into()]));
        assert_eq!(error.as_deref(), Some("write failed"));
        let (removed, error) = confirmed_deletions(&ids, |_| Ok(json!({"ok": true})));
        assert_eq!(removed.len(), 3);
        assert!(error.is_none());
    }

    #[test]
    fn data_routes_use_the_settings_window_while_help_and_home_stay_independent() {
        for route in ["/main/data", "/main/data?source=menu", "/main/settings/dataLocation",
            "/settings/dataLocation/", "dataLocation", "setting dataLocation"] {
            let route = normalize_main_settings_route(route.into());
            assert_eq!(route, "/main/settings/dataLocation");
            assert_eq!(settings_page_from_route(&route).as_deref(), Some("dataLocation"));
        }
        // The former management routes open pages of the unified main window.
        assert_eq!(settings_page_from_route("/main/docs#usage").as_deref(), Some("help"));
        assert_eq!(settings_page_from_route("/main/home").as_deref(), Some("home"));
        assert_eq!(settings_page_from_route("/main").as_deref(), Some("home"));
        for route in ["/main/settings/lan", "/main/settings/deviceSync", "/main/settings/devices"] {
            assert_eq!(settings_page_from_route(route).as_deref(), Some("devices"));
        }
        // `/main/lan` keeps opening the companion beside the clipboard.
        assert_eq!(settings_page_from_route("/main/lan"), None);
        assert_eq!(normalize_main_settings_route("/main/settings/explorer".into()), "/main/settings/user");
    }

    #[test]
    fn click_selection_supports_ctrl_order_and_shift_ranges() {
        let rows = ["a", "b", "c", "d", "e"];
        let mut state = SelectionState::default();
        click_selection(&rows, &mut state, 1, false, false);
        assert_eq!(state, ("b".into(), vec!["b".into()], Some(1)));
        // Ctrl keeps the order in which rows were picked.
        click_selection(&rows, &mut state, 4, true, false);
        click_selection(&rows, &mut state, 0, true, false);
        assert_eq!(state.1, ["b", "e", "a"]);
        assert_eq!(state.0, "a");
        // Ctrl on a selected row removes it; the last row cannot be removed.
        click_selection(&rows, &mut state, 0, true, false);
        assert_eq!(
            (state.0.as_str(), state.1.clone()),
            ("e", vec!["b".into(), "e".into()])
        );
        click_selection(&rows, &mut state, 1, true, false);
        click_selection(&rows, &mut state, 4, true, false);
        assert_eq!(state.1, ["e"]);
        // Shift extends from the anchor (last plain/Ctrl click) in list order.
        click_selection(&rows, &mut state, 2, false, false);
        click_selection(&rows, &mut state, 0, false, true);
        assert_eq!(state.1, ["a", "b", "c"]);
        click_selection(&rows, &mut state, 3, false, true);
        assert_eq!(state.1, ["c", "d"]);
        assert_eq!((state.0.as_str(), state.2), ("d", Some(2)));
        // Out-of-range clicks never change state.
        let before = state.clone();
        click_selection(&rows, &mut state, 9, false, false);
        assert_eq!(state, before);
    }
}

/// Mirrors the local-image extensions shown by the Web list; no filesystem probe.
pub(crate) fn native_image_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif" | "apng"
            )
        })
}

fn fifo_anchor(
    window: &Window,
    expected_hwnd: isize,
    screen: Option<(i32, i32)>,
) -> Result<(f32, f32), String> {
    #[cfg(windows)]
    if let Some((x, y)) = screen {
        use windows_compat::Win32::{
            Foundation::{HWND, POINT},
            Graphics::Gdi::ScreenToClient,
        };
        let actual = platform::hwnd(window)?.0 as isize;
        if actual == 0 || actual != expected_hwnd {
            return Err("FIFO owner HWND was replaced.".into());
        }
        let mut point = POINT { x, y };
        if !unsafe { ScreenToClient(HWND(actual as *mut std::ffi::c_void), &mut point) }.as_bool() {
            return Err("Unable to locate FIFO menu anchor.".into());
        }
        let scale = window.scale_factor();
        if !scale.is_finite() || scale <= 0.0 {
            return Err("Invalid FIFO owner scale.".into());
        }
        return Ok((point.x as f32 / scale, point.y as f32 / scale));
    }
    let point = window.mouse_position();
    let _ = (expected_hwnd, screen);
    Ok((f32::from(point.x), f32::from(point.y)))
}

/// Which category-strip entries fit in `available` logical pixels. Required
/// entries (the active category and active process presets) always stay; the
/// remaining entries fill in display order and the rest go to the 「⋯」 menu.
pub(crate) fn visible_strip_entries(
    widths: &[f32],
    required: &[bool],
    available: f32,
    gap: f32,
) -> Vec<bool> {
    let mut visible: Vec<bool> = (0..widths.len())
        .map(|index| required.get(index).copied().unwrap_or(false))
        .collect();
    let mut used: f32 = widths
        .iter()
        .zip(&visible)
        .filter(|(_, shown)| **shown)
        .map(|(width, _)| width + gap)
        .sum();
    for (index, width) in widths.iter().enumerate() {
        if visible[index] {
            continue;
        }
        if used + width > available {
            // Later entries stay in the menu so the strip keeps its order.
            break;
        }
        visible[index] = true;
        used += width + gap;
    }
    visible
}

#[cfg(test)]
mod strip_tests {
    use super::visible_strip_entries;

    #[test]
    fn entries_fill_in_order_until_the_first_that_does_not_fit() {
        let widths = [28.0, 28.0, 40.0, 40.0, 40.0];
        let visible = visible_strip_entries(&widths, &[false; 5], 140.0, 2.0);
        assert_eq!(visible, [true, true, true, false, false]);
    }

    #[test]
    fn the_active_entry_stays_even_when_it_would_overflow() {
        let widths = [28.0, 28.0, 40.0, 40.0, 40.0];
        let required = [false, false, false, false, true];
        let visible = visible_strip_entries(&widths, &required, 140.0, 2.0);
        assert_eq!(visible, [true, true, false, false, true]);
    }

    #[test]
    fn nothing_fits_but_required_entries() {
        let visible = visible_strip_entries(&[30.0, 30.0], &[false, true], 0.0, 2.0);
        assert_eq!(visible, [false, true]);
    }
}
