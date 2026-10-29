//! History destination cascade. Hover opens folder columns; only explicit
//! directory choices write. One shaped HWND owns the complete visible cascade.
use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use cliphost_core::{i18n::tr, window_place::overlay_work_area_from_point, HostState};
use gpui::{
    canvas, div, prelude::*, px, relative, AnimationExt, AnyElement, App, Context, Entity,
    FocusHandle, KeyDownEvent, MouseButton, ScrollHandle, SharedString, Subscription, Task, Timer,
    Window, WindowHandle, WindowOptions,
};
use serde_json::json;

use crate::{
    crisp_svg::svg,
    input::{InputColors, SearchEvent, SearchInput},
    motion, platform,
    preview::DesignTokens,
    smooth_wheel::SmoothWheelExt,
    view::color,
};

#[path = "favorite_save_model.rs"]
mod model;
use model::{ChoiceMode, Choices, Folder, Layout, LeaveDeadline, Path};

#[cfg(all(windows, debug_assertions))]
#[path = "favorite_save_fixture.rs"]
pub mod fixture;

#[derive(Clone, Debug)]
pub enum FavoriteSaveFinish {
    Saved {
        count: usize,
        path_label: String,
        undo_id: Option<String>,
    },
    Cancelled,
}
pub type FinishHandler = Rc<dyn Fn(u64, FavoriteSaveFinish, &mut App)>;
type ChoiceKey = (usize, ChoiceMode, Path);
type Rect = (f32, f32, f32, f32);
#[derive(Clone)]
struct Column {
    parent: Path,
    top: f32,
}

pub struct FavoriteSavePopup {
    host: HostState,
    tokens: DesignTokens,
    dark: bool,
    lease: u64,
    ids: Vec<String>,
    choices: Choices,
    layout: Layout,
    columns: Vec<Column>,
    query: String,
    active: Option<ChoiceKey>,
    hovered: Option<ChoiceKey>,
    error: Option<String>,
    loading: bool,
    saving: bool,
    finished: bool,
    input_requested: bool,
    ever_active: bool,
    opening_foreground: isize,
    leave: LeaveDeadline,
    branch_task: Option<Task<()>>,
    branch_from: Option<usize>,
    search: Entity<SearchInput>,
    focus: FocusHandle,
    _search_subscription: Subscription,
    _activation_subscription: Subscription,
    body_scroll: ScrollHandle,
    child_scroll: [ScrollHandle; 2],
    body_bounds: Rc<RefCell<HashMap<usize, Rect>>>,
    row_bounds: Rc<RefCell<HashMap<ChoiceKey, Rect>>>,
    card_regions: Rc<RefCell<HashMap<usize, Rect>>>,
    last_regions: Rc<RefCell<Vec<Rect>>>,
    hover_tweens: motion::Tweens<String>,
    on_finish: FinishHandler,
}

impl FavoriteSavePopup {
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: &HostState,
        owner_hwnd: isize,
        lease: u64,
        point: (i32, i32),
        trigger_rect: Option<(i32, i32, i32, i32)>,
        ids: Vec<String>,
        tokens: DesignTokens,
        dark: bool,
        on_finish: FinishHandler,
        cx: &mut App,
    ) -> Result<(), String> {
        if ids.is_empty() || !host.native_context_menu_is_current(lease) {
            return Err(tr("收藏菜单已关闭。", "The favorites menu has closed.").into());
        }
        if trigger_rect.is_some() && !host.set_native_context_menu_trigger(lease, trigger_rect) {
            return Err(tr(
                "无法定位收藏入口，请重试。",
                "Could not locate the favorite button. Please retry.",
            )
            .into());
        }
        let area = overlay_work_area_from_point(point.0, point.1).ok_or_else(|| {
            tr(
                "无法确定菜单所在屏幕。",
                "Could not determine the menu's monitor.",
            )
            .to_owned()
        })?;
        let work = (
            area.work.left.round() as i32,
            area.work.top.round() as i32,
            area.work.width.round() as i32,
            area.work.height.round() as i32,
        );
        let layout = Layout::place_for_trigger(point, work, area.dpi_scale_y, trigger_rect);
        let guarded_host = host.clone();
        let _guard = platform::guard_condition_inherited(move || {
            guarded_host.native_context_menu_is_current(lease)
        });
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
                        window.set_window_title(tr(
                            "ClipHost GPUI · 选择收藏夹",
                            "ClipHost GPUI · Choose favorite folder",
                        ));
                        cx.new(|cx| {
                            Self::new(
                                host.clone(),
                                tokens.clone(),
                                dark,
                                layout,
                                on_finish.clone(),
                                window,
                                cx,
                            )
                        })
                    },
                )
                .map_err(|error| error.to_string())?;
            *existing = Some(handle);
            handle
        };
        handle
            .update(cx, |this, window, cx| {
                this.host = host.clone();
                this.tokens = tokens;
                this.dark = dark;
                this.lease = lease;
                this.ids = ids;
                this.layout = layout;
                this.choices = Choices::default();
                this.columns = vec![Column {
                    parent: Vec::new(),
                    top: 8.0,
                }];
                this.query.clear();
                this.active = None;
                this.hovered = None;
                this.error = None;
                this.saving = false;
                this.finished = false;
                this.input_requested = false;
                this.ever_active = false;
                this.opening_foreground = cliphost_core::window_menu::foreground_hwnd();
                this.leave = LeaveDeadline::default();
                this.branch_task = None;
                this.branch_from = None;
                this.on_finish = on_finish;
                this.reset_scroll();
                this.hover_tweens.clear();
                this.card_regions.borrow_mut().clear();
                this.last_regions.borrow_mut().clear();
                this.search.update(cx, |input, cx| {
                    input.set_read_only(false, cx);
                    input.set_text(String::new(), window, cx);
                });
                this.configure_input(cx);
                window.focus(&this.focus);
                #[cfg(windows)]
                if !host.set_native_context_menu_hwnd(lease, platform::hwnd(window)?.0 as isize) {
                    return Err(tr("收藏菜单已关闭。", "The favorites menu has closed.").into());
                }
                this.load(window, cx);
                this.watch(window, cx);
                cx.notify();
                platform::set_geometry(window, Some((layout.rect.0, layout.rect.1)), None)?;
                platform::overlay_set_regions(
                    window,
                    vec![(
                        layout.root_x - 6.0,
                        2.0,
                        layout.root_width + 12.0,
                        layout.height + 12.0,
                    )],
                )?;
                platform::overlay_show(window, owner_hwnd, layout.rect)
            })
            .map_err(|error| error.to_string())?
    }

    fn new(
        host: HostState,
        tokens: DesignTokens,
        dark: bool,
        layout: Layout,
        on_finish: FinishHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| SearchInput::new(window, cx));
        let subscription = cx.subscribe_in(&search, window, |this, _, event, _, cx| match event {
            SearchEvent::Changed(query) => {
                if this.query != *query {
                    this.branch_task = None;
                    this.branch_from = None;
                    this.columns.truncate(1);
                    this.query = query.clone();
                    this.active = None;
                    this.hovered = None;
                    this.reset_scroll();
                    cx.notify();
                }
            }
            SearchEvent::Escape => {
                if !this.search.read(cx).is_composing() && this.current() {
                    this.host.begin_native_context_menu_escape_drain(this.lease);
                    this.cancel(cx);
                }
            }
            SearchEvent::Submit => {}
        });
        let activation = cx.observe_window_activation(window, |this, window, _| {
            // Record actual activation even if a quick Alt+Tab happens between
            // watch ticks. Deactivation alone never cancels an owner/star click.
            if this.current() && window.is_window_active() {
                this.ever_active = true;
            }
        });
        Self {
            host,
            tokens,
            dark,
            lease: 0,
            ids: Vec::new(),
            choices: Choices::default(),
            layout,
            columns: Vec::new(),
            query: String::new(),
            active: None,
            hovered: None,
            error: None,
            loading: false,
            saving: false,
            finished: false,
            input_requested: false,
            ever_active: false,
            opening_foreground: 0,
            leave: LeaveDeadline::default(),
            branch_task: None,
            branch_from: None,
            search,
            focus: cx.focus_handle(),
            _search_subscription: subscription,
            _activation_subscription: activation,
            body_scroll: ScrollHandle::new(),
            child_scroll: [ScrollHandle::new(), ScrollHandle::new()],
            body_bounds: Rc::default(),
            row_bounds: Rc::default(),
            card_regions: Rc::default(),
            last_regions: Rc::default(),
            hover_tweens: Default::default(),
            on_finish,
        }
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| {
            this.finished = true;
            this.branch_task = None;
            this.branch_from = None;
            if this.input_requested {
                this.host
                    .set_native_context_menu_text_input(this.lease, false);
                let _ = platform::set_no_activate(window, true);
            }
            this.input_requested = false;
            this.ever_active = false;
            this.lease = 0;
            window.blur();
            cx.notify();
            let _ = platform::hide(window);
        });
    }

    pub fn focus_search(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| this.request_input(window, cx));
    }

    fn current(&self) -> bool {
        self.lease != 0 && !self.finished && self.host.native_context_menu_is_current(self.lease)
    }

    fn configure_input(&mut self, cx: &mut Context<Self>) {
        let t = self.tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&t.secondary),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted).opacity(0.7),
            border: gpui::transparent_black(),
            selection: color(&t.neutral_active),
            caret: color(&t.text),
        };
        self.search.update(cx, |input, cx| {
            input.set_metrics(28.0, 12.0, 0.0, cx);
            input.set_colors(colors, cx);
            input.set_placeholder(tr("搜索文件夹或路径", "Search folders or paths"), cx);
            input.set_double_click_clears(false);
        });
    }

    fn request_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() || self.saving {
            return;
        }
        let lease = self.lease;
        self.active = None;
        cx.defer_in(window, move |this, window, cx| {
            if this.lease != lease || !this.current() {
                return;
            }
            if !this.host.request_native_context_menu_text_input(lease) {
                return;
            }
            let host = this.host.clone();
            let _guard = platform::guard_condition_inherited(move || {
                host.native_context_menu_is_current(lease)
                    && host.clipboard_keyboard_focus_allowed()
            });
            match platform::set_no_activate(window, false).and_then(|_| platform::activate(window))
            {
                Ok(()) => {
                    this.input_requested = true;
                    this.search.read(cx).focus_handle().focus(window);
                }
                Err(error) => {
                    this.host.set_native_context_menu_text_input(lease, false);
                    this.error = Some(error);
                }
            }
            cx.notify();
        });
    }

    fn reset_scroll(&mut self) {
        self.body_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        for scroll in &self.child_scroll {
            scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        }
        self.body_bounds.borrow_mut().clear();
        self.row_bounds.borrow_mut().clear();
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        let (host, lease) = (self.host.clone(), self.lease);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.favorite_save_choices()
                        .and_then(|value| Choices::parse(&value))
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.lease != lease || !this.current() {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(choices) => {
                        this.choices = choices;
                        for index in 0..this.columns.len() {
                            if !this.columns[index].parent.is_empty()
                                && this
                                    .choices
                                    .destination(&this.columns[index].parent)
                                    .is_none()
                            {
                                this.columns.truncate(index.max(1));
                                if index == 0 {
                                    this.columns[0].parent.clear();
                                }
                                break;
                            }
                        }
                        this.active = None;
                        this.reset_scroll();
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lease = self.lease;
        cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(50)).await;
            let keep = this
                .update_in(cx, |this, window, cx| {
                    if this.lease != lease || this.finished {
                        return false;
                    }
                    if !this.host.native_context_menu_is_current(lease) {
                        this.cancel(cx);
                        let _ = platform::hide(window);
                        return false;
                    }
                    if let Ok(Some(error)) = platform::take_error(window) {
                        this.host.set_native_context_menu_text_input(lease, false);
                        this.input_requested = false;
                        this.ever_active = false;
                        window.blur();
                        let _ = platform::set_no_activate(window, true);
                        this.error = Some(error);
                        cx.notify();
                    }
                    #[cfg(windows)]
                    if let Ok(hwnd) = platform::hwnd(window) {
                        let foreground = cliphost_core::window_menu::foreground_hwnd();
                        if foreground == hwnd.0 as isize {
                            this.ever_active = true;
                        } else if foreground != 0
                            && foreground != this.host.host_hwnd()
                            && (this.ever_active || foreground != this.opening_foreground)
                        {
                            // Owner/trigger clicks belong to the core outside-click
                            // lease and the owner's toggle. Initial no-activate state
                            // and queued activation must not be mistaken for blur.
                            this.cancel(cx);
                            return false;
                        }
                    }
                    let inside = platform::cursor_position().is_some_and(|(x, y)| {
                        this.host.native_context_menu_pointer_inside(lease, x, y)
                    });
                    // Native input permission survives keyboard navigation. Only
                    // the actual editor focus/IME protects a hover session.
                    let input = this.search.read(cx);
                    let explicit_input = model::text_input_holds_open(
                        window.is_window_active(),
                        input.focus_handle().is_focused(window),
                        input.is_composing(),
                    );
                    if this.leave.observe(
                        inside,
                        explicit_input,
                        this.saving,
                        Instant::now(),
                        motion::FAVORITE_SAVE_DISMISS,
                    ) {
                        this.cancel(cx);
                        return false;
                    }
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.finished || self.lease == 0 {
            return;
        }
        self.finished = true;
        self.branch_task = None;
        self.branch_from = None;
        let (finish, lease) = (self.on_finish.clone(), self.lease);
        cx.defer(move |cx| finish(lease, FavoriteSaveFinish::Cancelled, cx));
    }

    fn save(&mut self, folder: Folder, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() || self.saving || self.loading || self.search.read(cx).is_composing() {
            return;
        }
        self.saving = true;
        self.branch_task = None;
        self.branch_from = None;
        self.error = None;
        self.search
            .update(cx, |input, cx| input.set_read_only(true, cx));
        let (host, lease, finish, count) = (
            self.host.clone(),
            self.lease,
            self.on_finish.clone(),
            self.ids.len(),
        );
        let payload = json!({"ids":self.ids, "parentId":folder.id, "path":folder.path,
            "expectedRevision":self.choices.revision, "operationId":format!("favorite-save-{}", uuid::Uuid::new_v4())});
        let label = folder.label;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !host.native_context_menu_is_current(lease) {
                        return Err(
                            tr("收藏菜单已关闭。", "The favorites menu has closed.").to_owned()
                        );
                    }
                    host.favorite_save_history(&payload)
                })
                .await;
            match result {
                Ok(result) => {
                    let path_label = result["savedFolder"]["pathLabel"]
                        .as_str()
                        .unwrap_or(&label)
                        .to_owned();
                    let undo_id = result["undoId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned);
                    let _ = this.update_in(cx, |this, _, cx| {
                        if this.lease == lease {
                            this.finished = true;
                            this.saving = false;
                            cx.notify();
                        }
                        cx.defer(move |cx| {
                            finish(
                                lease,
                                FavoriteSaveFinish::Saved {
                                    count,
                                    path_label,
                                    undo_id,
                                },
                                cx,
                            )
                        });
                    });
                }
                Err(error) => {
                    let _ = this.update_in(cx, |this, _, cx| {
                        if this.lease != lease || !this.current() {
                            return;
                        }
                        this.saving = false;
                        this.error = Some(error);
                        this.search
                            .update(cx, |input, cx| input.set_read_only(false, cx));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn column_choices(&self, index: usize) -> Vec<model::Choice> {
        self.columns.get(index).map_or_else(Vec::new, |column| {
            self.choices
                .choices(&column.parent, if index == 0 { &self.query } else { "" })
        })
    }

    fn open_column(
        &mut self,
        folder: Folder,
        index: usize,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.current() || self.saving || self.loading || index >= self.columns.len() {
            return;
        }
        if !folder.has_children {
            self.columns.truncate(index + 1);
            cx.notify();
            return;
        }
        if index + 1 >= self.layout.columns && !explicit {
            return;
        }
        let next = (index + 1).min(self.layout.columns - 1);
        let count = self.choices.members(&folder.path).len();
        let height = (44.0 + count as f32 * 32.0).min(self.layout.height);
        let top = if next == 0 {
            8.0
        } else {
            self.row_bounds
                .borrow()
                .get(&(index, ChoiceMode::Browse, folder.path.clone()))
                .map_or(8.0, |rect| rect.1 - 7.0)
                .clamp(8.0, (self.layout.height + 8.0 - height).max(8.0))
        };
        self.columns.truncate(next);
        self.columns.push(Column {
            parent: folder.path,
            top,
        });
        self.card_regions
            .borrow_mut()
            .retain(|column, _| *column <= next);
        self.row_bounds.borrow_mut().retain(|key, _| key.0 < next);
        if next == 0 {
            self.body_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        } else {
            self.child_scroll[next - 1].set_offset(gpui::point(px(0.0), px(0.0)));
        }
        if explicit {
            self.hovered = None;
            self.active = self
                .column_choices(next)
                .first()
                .map(|choice| (next, choice.mode, choice.folder.path.clone()));
            self.focus.focus(window);
        }
        cx.notify();
    }

    fn hover_row(
        &mut self,
        key: ChoiceKey,
        folder: Folder,
        entered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.current() {
            return;
        }
        if !entered {
            if self.hovered.as_ref() == Some(&key) {
                self.hovered = None;
                cx.notify();
            }
            return;
        }
        self.hovered = Some(key.clone());
        self.branch_task = None;
        self.branch_from = None;
        if key.1 == ChoiceMode::Browse && !self.saving && !self.loading {
            let index = key.0;
            if self
                .columns
                .get(index + 1)
                .is_some_and(|column| column.parent == folder.path)
            {
                cx.notify();
                return;
            }
            let delay = if self.columns.len() > index + 1 {
                motion::FAVORITE_SAVE_SWITCH
            } else {
                motion::FAVORITE_SAVE_BRANCH
            };
            let lease = self.lease;
            self.branch_from = Some(index);
            self.branch_task = Some(cx.spawn_in(window, async move |this, cx| {
                Timer::after(delay).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.lease == lease && this.current() && this.hovered.as_ref() == Some(&key)
                    {
                        this.branch_from = None;
                        this.open_column(folder, index, false, window, cx);
                    }
                });
            }));
        }
        cx.notify();
    }

    fn enter_card(&mut self, index: usize) {
        // Cancel an ancestor's pending switch on entry, including diagonal
        // movement. Never erase the timer just created by this child's row.
        if self.branch_from.is_some_and(|from| from < index) {
            self.branch_task = None;
            self.branch_from = None;
        }
    }

    fn back(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() || self.saving || index >= self.columns.len() {
            return;
        }
        let old = self.columns[index].parent.clone();
        if old.is_empty() {
            return;
        }
        self.branch_task = None;
        self.branch_from = None;
        let preceding = index.checked_sub(1).map(|i| self.columns[i].parent.clone());
        if preceding
            .as_ref()
            .is_some_and(|path| old.len() == path.len() + 1 && old.starts_with(path))
        {
            self.columns.truncate(index);
            self.active = Some((index - 1, ChoiceMode::Browse, old));
        } else {
            self.columns.truncate(index + 1);
            self.columns[index].parent.pop();
            self.active = Some((index, ChoiceMode::Browse, old));
        }
        self.hovered = None;
        self.reset_scroll();
        self.focus.focus(window);
        cx.notify();
    }

    fn reveal_active(&self) {
        let Some(key) = &self.active else {
            return;
        };
        let Some(row) = self.row_bounds.borrow().get(key).copied() else {
            return;
        };
        let Some(body) = self.body_bounds.borrow().get(&key.0).copied() else {
            return;
        };
        let delta = if row.1 < body.1 {
            body.1 - row.1
        } else if row.1 + row.3 > body.1 + body.3 {
            body.1 + body.3 - row.1 - row.3
        } else {
            0.0
        };
        if delta != 0.0 {
            let scroll = if key.0 == 0 {
                &self.body_scroll
            } else {
                &self.child_scroll[key.0 - 1]
            };
            let offset = scroll.offset();
            scroll.set_offset(gpui::point(offset.x, offset.y + px(delta)));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current() || self.search.read(cx).is_composing() {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "tab" {
            self.cancel(cx);
            return;
        }
        if key == "escape" {
            if event.keystroke.modifiers.modified() {
                return;
            }
            self.host.begin_native_context_menu_escape_drain(self.lease);
            self.cancel(cx);
            cx.stop_propagation();
            return;
        }
        if self.saving || self.loading || event.keystroke.modifiers.modified() {
            return;
        }
        let input = self.search.read(cx).focus_handle().is_focused(window);
        if input && key != "down" && key != "up" {
            return;
        }
        let index = self
            .active
            .as_ref()
            .map_or(0, |key| key.0)
            .min(self.columns.len().saturating_sub(1));
        let choices = self.column_choices(index);
        let position = self.active.as_ref().and_then(|(_, mode, path)| {
            choices
                .iter()
                .position(|choice| choice.mode == *mode && choice.folder.path == *path)
        });
        match key {
            "down" | "up" | "home" | "end" => {
                if choices.is_empty() {
                    return;
                }
                let next = match key {
                    "home" => 0,
                    "end" => choices.len() - 1,
                    "up" => position
                        .unwrap_or(0)
                        .checked_sub(1)
                        .unwrap_or(choices.len() - 1),
                    _ => position.map_or(0, |i| (i + 1) % choices.len()),
                };
                self.active = Some((index, choices[next].mode, choices[next].folder.path.clone()));
                self.focus.focus(window);
                self.reveal_active();
                cx.notify();
            }
            "left" => self.back(index, window, cx),
            "right" => {
                if let Some(position) = position {
                    self.branch_task = None;
                    self.branch_from = None;
                    self.open_column(choices[position].folder.clone(), index, true, window, cx);
                }
            }
            "enter" | "space" if !event.is_held => {
                if let Some(position) = position {
                    self.save(choices[position].folder.clone(), window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn icon(&self, key: &str, size: f32) -> Option<crate::crisp_svg::CrispSvg> {
        self.tokens.icons.get(key).map(|path| {
            svg()
                .path(path.clone())
                .size(px(size))
                .flex_none()
                .text_color(color(&self.tokens.theme(self.dark).text_secondary))
        })
    }

    fn row(
        &mut self,
        folder: Folder,
        mode: ChoiceMode,
        index: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let lease = self.lease;
        let key = (index, mode, folder.path.clone());
        let id = format!(
            "{index}-{:?}-{}",
            mode,
            serde_json::to_string(&folder.path).unwrap_or_default()
        );
        let selected = self.active.as_ref() == Some(&key);
        let now = Instant::now();
        let progress = self.hover_tweens.value(
            id.clone(),
            if self.hovered.as_ref() == Some(&key) || selected {
                1.0
            } else {
                0.0
            },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if self.hover_tweens.any_active(now) {
            window.request_animation_frame();
        }
        let hover_key = key.clone();
        let hover_folder = folder.clone();
        let save_folder = folder.clone();
        let enter_folder = folder.clone();
        let bounds = self.row_bounds.clone();
        let quick = mode == ChoiceMode::QuickSave;
        let expanded = self
            .columns
            .get(index + 1)
            .is_some_and(|column| column.parent == folder.path);
        let mut row = rsx! {
            <div id={SharedString::from(format!("favorite-save-row-{id}"))} relative w_full flex
                items_start min_h={px(32.0)} gap={px(7.0)} px={px(7.0)} py={px(6.0)}
                rounded={px(5.0)} cursor_pointer
                bg={motion::mix_hsla(
                color(&t.surface),
                color(&t.hover),
                progress,
            )}
                text_color={color(&t.text)} />
        }
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                if this.lease == lease {
                    this.hover_row(
                        hover_key.clone(),
                        hover_folder.clone(),
                        *hovered,
                        window,
                        cx,
                    );
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                if this.lease == lease {
                    this.save(save_folder.clone(), window, cx);
                }
            }))
            .child(
                canvas(
                    move |rect, _, _| {
                        bounds.borrow_mut().insert(
                            key,
                            (
                                f32::from(rect.left()),
                                f32::from(rect.top()),
                                f32::from(rect.size.width),
                                f32::from(rect.size.height),
                            ),
                        );
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(div().pt(px(2.0)).children(self.icon(
                if quick {
                    "star"
                } else if expanded {
                    "folder-open"
                } else {
                    "folder-closed"
                },
                14.0,
            )))
            .child(
                rsx! {
                    <div flex_1 min_w_0 line_height={relative(1.45)}>
                        <div truncate>
                            {folder.title.clone()}
                        </div>
                    </div>
                }
                    .when(quick, |title| {
                        title.child(
                            div()
                                .mt(px(1.0))
                                .text_size(px(10.0))
                                .text_color(color(&t.text_muted))
                                .line_clamp(2)
                                .child(folder.label),
                        )
                    }),
            );
        if !quick && folder.has_children {
            row = row.child(
                rsx! {
                    <div id={SharedString::from(format!("favorite-save-enter-{id}"))}
                        size={px(22.0)} flex_none flex items_center justify_center
                        rounded={px(4.0)} />
                }
                    .hover(|style| style.bg(color(&t.tertiary)))
                    .children(self.icon("chevron-right", 14.0))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if this.lease != lease {
                            return;
                        }
                        this.branch_task = None;
                        this.branch_from = None;
                        this.open_column(enter_folder.clone(), index, true, window, cx);
                    })),
            );
        }
        row.into_any_element()
    }

    fn heading(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark).clone();
        let lease = self.lease;
        let parent = self.columns[index].parent.clone();
        let folder = self.choices.destination(&parent);
        let mut heading = rsx! {
            <div h={px(29.0)} flex_none flex items_center gap={px(3.0)} px={px(3.0)}
                text_size={px(11.0)} text_color={color(&t.text_secondary)} />
        };
        if !parent.is_empty() {
            heading = heading.child(
                rsx! {
                    <div id={SharedString::from(format!("favorite-save-back-{index}"))}
                        size={px(22.0)} flex_none flex items_center justify_center
                        rounded={px(4.0)} cursor_pointer />
                }
                    .hover(|style| style.bg(color(&t.hover)))
                    .children(self.icon("chevron-left", 14.0))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if this.lease == lease {
                            this.back(index, window, cx);
                        }
                    })),
            );
        }
        let title = folder.as_ref().map_or_else(
            || tr("全部收藏夹", "All folders").to_owned(),
            |folder| folder.title.clone(),
        );
        heading
            .child(
                rsx! {
                    <div id={SharedString::from(format!("favorite-save-heading-{index}"))} flex_1
                        min_w_0 truncate px={px(3.0)}>
                        {title}
                    </div>
                }
                    .when(folder.is_some(), |title| {
                        title
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if this.lease == lease {
                                    if let Some(folder) = folder.clone() {
                                        this.save(folder, window, cx);
                                    }
                                }
                            }))
                    }),
            )
            .into_any_element()
    }

    fn body_probe(&self, index: usize) -> AnyElement {
        let bodies = self.body_bounds.clone();
        canvas(
            move |bounds, _, _| {
                bodies.borrow_mut().insert(
                    index,
                    (
                        f32::from(bounds.left()),
                        f32::from(bounds.top()),
                        f32::from(bounds.size.width),
                        f32::from(bounds.size.height),
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

    fn region_probe(&self, index: usize) -> AnyElement {
        let regions = self.card_regions.clone();
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
}

impl Render for FavoriteSavePopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        self.card_regions.borrow_mut().clear();
        let mut root = div()
            .id("favorite-save-overlay")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .bg(gpui::transparent_black())
            .font_family(self.tokens.typography.font_family.clone())
            .text_size(px(12.0))
            .line_height(relative(1.4))
            .text_color(color(&t.text))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| this.cancel(cx)),
            )
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)));
        if self.lease == 0 || self.columns.is_empty() {
            return root;
        }
        let lease = self.lease;
        let mut card = rsx! {
            <div id={"favorite-save-root"} relative absolute left={px(self.layout.root_x)}
                top={px(8.0)} w={px(self.layout.root_width)} h={px(self.layout.height)}
                p={px(5.0)} flex flex_col rounded={px(8.0)} border_1
                border_color={color(&t.border_secondary)} bg={color(&t.surface)} shadow_lg
                occlude />
        }
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                if *hovered && this.lease == lease {
                    this.enter_card(0);
                }
            }))
            .child(self.region_probe(0))
            .child(
                rsx! {
                    <div h={px(29.0)} flex_none flex items_center gap={px(7.0)} px={px(5.0)} />
                }
                    .children(self.icon("star", 15.0))
                    .child(tr("收藏到", "Save to"))
                    .child(
                        rsx! {
                            <div text_size={px(11.0)} text_color={color(&t.text_muted)}>
                                {cliphost_core::trf!("{} 项", "{} items", self.ids.len())}
                            </div>
                        },
                    )
                    .child(div().flex_1())
                    .child(
                        rsx! {
                            <div id={"favorite-save-close"} size={px(23.0)} flex items_center
                                justify_center rounded={px(5.0)} cursor_pointer />
                        }
                            .hover(|style| style.bg(color(&t.hover)))
                            .children(self.icon("close", 14.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if this.lease == lease {
                                    this.cancel(cx);
                                }
                            })),
                    ),
            )
            .child(
                rsx! {
                    <div id={"favorite-save-search"} flex_none h={px(30.0)} mx={px(3.0)}
                        mt={px(3.0)} mb={px(4.0)} px={px(7.0)} flex items_center gap={px(6.0)}
                        rounded={px(5.0)} border_1 />
                }
                    .border_color(color(
                        if self.search.read(cx).focus_handle().is_focused(window) {
                            &t.accent
                        } else {
                            &t.border
                        },
                    ))
                    .bg(color(&t.secondary))
                    .capture_any_mouse_down(cx.listener(
                        move |this, event: &gpui::MouseDownEvent, window, cx| {
                            if event.button == MouseButton::Left && this.lease == lease {
                                this.request_input(window, cx);
                            }
                        },
                    ))
                    .children(self.icon("search", 14.0))
                    .child(rsx! {
                        <div flex_1 min_w_0>
                            {self.search.clone()}
                        </div>
                    })
                    .when(!self.query.is_empty(), |search| {
                        search.child(
                            rsx! {
                                <div id={"favorite-save-search-clear"} size={px(20.0)} flex
                                    items_center justify_center cursor_pointer />
                            }
                                .children(self.icon("close", 12.0))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    if this.lease != lease {
                                        return;
                                    }
                                    this.query.clear();
                                    this.active = None;
                                    this.hovered = None;
                                    this.branch_task = None;
                                    this.branch_from = None;
                                    this.columns.truncate(1);
                                    this.reset_scroll();
                                    this.search.update(cx, |input, cx| {
                                        input.set_text(String::new(), window, cx)
                                    });
                                    this.search.read(cx).focus_handle().focus(window);
                                    cx.notify();
                                })),
                        )
                    }),
            );
        let label = |text: &'static str| {
            rsx! {
                <div px={px(7.0)} pt={px(7.0)} pb={px(4.0)} text_size={px(10.0)}
                    text_color={color(&t.text_muted)}>
                    {text}
                </div>
            }
        };
        let parent = self.columns[0].parent.clone();
        let mut body = rsx! {
            <div id={"favorite-save-body"} relative flex_1 min_h_0 />
        }
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .smooth_scroll("favorite-save-body", cx)
            .child(self.body_probe(0));
        if self.loading {
            body = body.child(
                rsx! {
                    <div p={px(9.0)} text_color={color(&t.text_muted)}>
                        {tr("正在加载收藏夹…", "Loading folders…")}
                    </div>
                },
            );
        } else if !self.query.trim().is_empty() {
            let rows = self.choices.search(&self.query);
            body = body.child(label(tr(
                "搜索结果 · 点击收藏",
                "Search results · click to save",
            )));
            if rows.is_empty() {
                body = body.child(
                    rsx! {
                        <div p={px(9.0)} text_color={color(&t.text_muted)}>
                            {tr("未找到文件夹", "No matching folders")}
                        </div>
                    },
                );
            }
            for folder in rows {
                body = body.child(self.row(folder, ChoiceMode::QuickSave, 0, window, cx));
            }
        } else {
            if parent.is_empty() && !self.choices.recent.is_empty() {
                body = body.child(label(tr("最近使用", "Recent folders")));
                for folder in self.choices.recent.clone() {
                    body = body.child(self.row(folder, ChoiceMode::QuickSave, 0, window, cx));
                }
            }
            body = body.child(self.heading(0, cx));
            let rows = self.choices.members(&parent);
            if rows.is_empty() {
                body = body.child(
                    div()
                        .px(px(7.0))
                        .py(px(10.0))
                        .text_color(color(&t.text_muted))
                        .child(if parent.is_empty() {
                            tr(
                                "请先在收藏页创建文件夹",
                                "Create a folder in Favorites first",
                            )
                        } else {
                            tr("没有子文件夹", "No subfolders")
                        }),
                );
            }
            for folder in rows {
                body = body.child(self.row(folder, ChoiceMode::Browse, 0, window, cx));
            }
        }
        if self.choices.truncated {
            body = body.child(
                rsx! {
                    <div p={px(7.0)} text_size={px(10.0)} text_color={color(&t.text_muted)}>
                        {tr(
                        "目录过多，部分路径未显示。",
                        "Some folder paths are not shown.",
                    )}
                    </div>
                },
            );
        }
        if let Some(error) = self.error.clone() {
            body = body.child(
                rsx! {
                    <div p={px(7.0)} text_size={px(11.0)} text_color={color(&t.error)}>
                        {error}
                    </div>
                }
                    .child(
                        rsx! {
                            <div id={"favorite-save-retry"} cursor_pointer>
                                {tr("重新加载目录", "Reload folders")}
                            </div>
                        }
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if this.lease != lease {
                                    return;
                                }
                                this.branch_task = None;
                                this.branch_from = None;
                                this.error = None;
                                this.load(window, cx);
                                cx.notify();
                            })),
                    ),
            );
        }
        let target = self
            .hovered
            .as_ref()
            .or(self.active.as_ref())
            .map(|key| self.choices.label(&key.2))
            .filter(|label| !label.is_empty());
        card = card.child(body).child(
            rsx! {
                <div flex_none min_h={px(29.0)} mt={px(4.0)} px={px(7.0)} pt={px(6.0)}
                    pb={px(2.0)} border_t_1 border_color={color(&t.border)} text_size={px(10.0)}
                    line_height={relative(1.45)} text_color={color(&t.text_muted)} />
            }
                .child(if self.saving {
                    tr("正在收藏…", "Saving…").to_owned()
                } else {
                    target.unwrap_or_else(|| {
                        tr("点击文件夹即可收藏", "Click a folder to save").to_owned()
                    })
                })
                .line_clamp(2),
        );
        root = root.child(card.with_animation(
            SharedString::from(format!("favorite-save-enter-{}", self.lease)),
            motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
            |card, value| card.opacity(value),
        ));
        if self.query.trim().is_empty() {
            for index in 1..self.columns.len() {
                let column = self.columns[index].clone();
                let rows = self.choices.members(&column.parent);
                let height = (44.0 + rows.len() as f32 * 32.0)
                    .min(self.layout.height + 8.0 - column.top)
                    .max(40.0);
                let scroll = self.child_scroll[index - 1].clone();
                let mut body = rsx! {
                    <div id={SharedString::from(format!("favorite-save-body-{index}"))} relative
                        flex_1 min_h_0 />
                }
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .smooth_scroll(
                        SharedString::from(format!("favorite-save-body-{index}")),
                        cx,
                    )
                    .child(self.body_probe(index));
                for folder in rows {
                    body = body.child(self.row(folder, ChoiceMode::Browse, index, window, cx));
                }
                let card = rsx! {
                    <div id={SharedString::from(format!("favorite-save-column-{index}"))}
                        relative absolute left={px(self.layout.column_x(index))}
                        top={px(column.top)} w={px(210.0)} h={px(height)} p={px(5.0)} flex
                        flex_col rounded={px(8.0)} border_1
                        border_color={color(&t.border_secondary)} bg={color(&t.surface)}
                        shadow_lg occlude />
                }
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                        if *hovered && this.lease == lease {
                            this.enter_card(index);
                        }
                    }))
                    .child(self.region_probe(index))
                    .child(self.heading(index, cx))
                    .child(body);
                root = root.child(card.with_animation(
                    SharedString::from(format!(
                        "favorite-save-column-enter-{}-{}",
                        self.lease,
                        column.parent.join("/")
                    )),
                    motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                    |card, value| card.opacity(value),
                ));
            }
        }
        let regions = self.card_regions.clone();
        let previous = self.last_regions.clone();
        let (host, lease) = (self.host.clone(), self.lease);
        root.child(
            canvas(
                move |_, window, cx| {
                    let mut indexed: Vec<_> = regions
                        .borrow()
                        .iter()
                        .map(|(index, rect)| (*index, *rect))
                        .collect();
                    indexed.sort_by_key(|(index, _)| *index);
                    let rectangles: Vec<_> = indexed.into_iter().map(|(_, rect)| rect).collect();
                    if rectangles.is_empty() || *previous.borrow() == rectangles {
                        return;
                    }
                    *previous.borrow_mut() = rectangles.clone();
                    let handle = window.window_handle();
                    cx.defer(move |cx| {
                        if !host.native_context_menu_is_current(lease)
                            || *previous.borrow() != rectangles
                        {
                            return;
                        }
                        let guarded_host = host.clone();
                        let _guard = platform::guard_condition_inherited(move || {
                            guarded_host.native_context_menu_is_current(lease)
                        });
                        let _ = handle.update(cx, |_, window, _| {
                            platform::overlay_set_regions(window, rectangles)
                        });
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
    }
}
