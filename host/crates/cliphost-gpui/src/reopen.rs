//! GPUI Reopen. The quick window and the Explorer management page share the
//! authoritative core snapshot. UI selections always carry record revisions.

use std::{collections::HashSet, rc::Rc, sync::Arc};

use chrono::{DateTime, Local};
use cliphost_core::{host::HostState, i18n::tr};
use gpui::{
    div, list, prelude::*, px, size, AnyElement, App, Bounds, Context, Entity,
    FocusHandle, KeyDownEvent, ListAlignment, ListState, MouseButton, SharedString,
    Stateful, Subscription, Task, Window, WindowBounds, WindowHandle, WindowOptions,
};
use gpui_rsx::rsx;
use serde_json::{json, Value};

use crate::{
    crisp_svg::svg,
    input::{InputColors, SearchEvent, SearchInput},
    platform,
    preview::{DesignTokens, ThemeTokens},
    scrollbar::{ScrollbarState, ScrollbarStyle, ScrollbarTarget},
    view::color,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab { History, Exclusions }

#[derive(Clone)]
enum Row {
    Label(&'static str),
    History(Value),
    Opened(Value),
    Exclusion(Value),
}

#[derive(Clone)]
enum Confirm {
    Clear { entries: Vec<Value> },
    Exclude { path: String, entries: Vec<Value>, children: bool, remove: bool },
}

fn path_of(value: &Value) -> &str { value["path"].as_str().unwrap_or("") }

fn path_key(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn is_shell(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.starts_with("shell:") || path.starts_with("::{")
}

fn matches_path(path: &str, rule: &str, children: bool) -> bool {
    let path = path_key(path);
    let rule = path_key(rule);
    path == rule || (children && !is_shell(&rule) && path.starts_with(&(rule + "\\")))
}

fn title(value: &Value) -> String {
    if let Some(title) = value["title"].as_str().filter(|title| !title.is_empty()) {
        return title.into();
    }
    let path = path_of(value).trim_end_matches(['\\', '/']);
    path.rsplit(['\\', '/']).next().filter(|name| !name.is_empty()).unwrap_or(path).into()
}

fn matches_query(value: &Value, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty() || path_of(value).to_lowercase().contains(&query) || title(value).to_lowercase().contains(&query)
}

fn entry_key(value: &Value) -> String {
    format!("{}\n{}", path_of(value), value["recordedAt"].as_i64().unwrap_or(0))
}

fn record_time(value: &Value) -> String {
    value["recordedAt"].as_i64()
        .filter(|timestamp| *timestamp > 0)
        .and_then(DateTime::from_timestamp_millis)
        .map(|time| time.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| tr("时间未知", "Unknown time").into())
}

/// This entity is also embedded in SettingsWindow. Loading and mutations run on
/// background workers; a page/show epoch retires their presentation rights.
pub struct ReopenWindow {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    quick: bool,
    active: bool,
    leave_locked: bool,
    had_activation: bool,
    initialized: bool,
    display: u64,
    target_hwnd: i64,
    focus: FocusHandle,
    search: Entity<SearchInput>,
    limit_input: Entity<SearchInput>,
    menu_input: Entity<SearchInput>,
    _subscriptions: Vec<Subscription>,
    query: String,
    tab: Tab,
    snapshot: Value,
    projection: Vec<Row>,
    selected: HashSet<String>,
    cursor: Option<usize>,
    list: ListState,
    scrollbar: ScrollbarState,
    settings_open: bool,
    more: bool,
    context: Option<Value>,
    confirm: Option<Confirm>,
    dialog_selection: usize,
    context_selection: usize,
    undo: Option<Value>,
    feedback: Option<String>,
    error: Option<String>,
    failed_request: Option<Value>,
    epoch: u64,
    loading: bool,
    refresh_pending: bool,
    busy: bool,
    queued_settings: Option<Value>,
    load: Option<Task<()>>,
    mutation: Option<Task<()>>,
    close_waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

impl ReopenWindow {
    pub fn new_manager(host: Arc<HostState>, tokens: DesignTokens, dark: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(host, tokens, dark, false, window, cx)
    }

    fn new(host: Arc<HostState>, tokens: DesignTokens, dark: bool, quick: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| SearchInput::new(window, cx));
        let limit_input = cx.new(|cx| SearchInput::new(window, cx));
        let menu_input = cx.new(|cx| SearchInput::new(window, cx));
        let mut subscriptions = vec![cx.subscribe_in(&search, window, |this: &mut Self, _, event, window, cx| match event {
            SearchEvent::Changed(query) => {
                this.query.clone_from(query);
                this.selected.clear();
                this.context = None;
                this.reproject(false, cx);
            }
            SearchEvent::Submit => this.open_cursor(window, cx),
            SearchEvent::Escape => this.escape(window, cx),
        })];
        for (input, history) in [(&limit_input, true), (&menu_input, false)] {
            subscriptions.push(cx.subscribe_in(input, window, move |this: &mut Self, _, event, window, cx| {
                if matches!(event, SearchEvent::Submit) { this.commit_limit(history, window, cx); }
            }));
            let focus = input.read(cx).focus_handle();
            subscriptions.push(cx.on_blur(&focus, window, move |this: &mut Self, window, cx| this.commit_limit(history, window, cx)));
        }
        let mut this = Self {
            host, tokens, dark, quick, active: true, leave_locked: false,
            had_activation: false, initialized: false, display: 0, target_hwnd: 0,
            focus: cx.focus_handle(), search, limit_input, menu_input,
            _subscriptions: subscriptions, query: String::new(), tab: Tab::History,
            snapshot: json!({"entries":[],"opened":[],"exclusions":[],"recording":true,"historyLimit":200,"menuLimit":10,"showMenu":false}),
            projection: Vec::new(), selected: HashSet::new(), cursor: None,
            list: ListState::new(0, ListAlignment::Top, px(100.0)), scrollbar: ScrollbarState::default(),
            settings_open: false, more: false, context: None, confirm: None, dialog_selection: 0, context_selection: 0, undo: None,
            feedback: None, error: None, failed_request: None,
            epoch: 0, loading: false, refresh_pending: false, busy: false, queued_settings: None, load: None, mutation: None, close_waiters: Vec::new(),
        };
        this.configure_inputs(cx);
        this.reload(cx);
        this
    }

    pub fn show(existing: &mut Option<WindowHandle<Self>>, host: Arc<HostState>, tokens: DesignTokens, dark: bool, cx: &mut App) -> Result<(), String> {
        let state = host.reopen_native_snapshot();
        if state["visible"].as_bool() != Some(true) { return Ok(()); }
        let target = state["targetHwnd"].as_i64().unwrap_or(0);
        let handle = if let Some(handle) = existing { *handle } else {
            let build_host = host.clone();
            let incoming = tokens.clone();
            let bounds = Bounds::centered(None, size(px(420.0), px(560.0)), cx);
            let handle = cx.open_window(WindowOptions {
                titlebar: None, show: false, focus: false,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(320.0), px(380.0))),
                ..Default::default()
            }, move |window, cx| {
                window.set_window_title(tr("重开", "Reopen"));
                let entity = cx.new(|cx| Self::new(build_host, incoming, dark, true, window, cx));
                let weak = entity.downgrade();
                window.on_window_should_close(cx, move |window, cx| {
                    let _ = weak.update(cx, |this, cx| this.dismiss(window, cx));
                    false
                });
                entity
            }).map_err(|error| error.to_string())?;
            *existing = Some(handle);
            handle
        };
        handle.update(cx, |this, window, cx| {
            for waiter in std::mem::take(&mut this.close_waiters) {
                let _ = waiter.send(Err(tr("新的重开显示已取消旧的关闭请求", "A new Reopen display cancelled the previous close request").into()));
            }
            // Every Show owns fresh presentation rights even when a previously
            // accepted database write or external action is still finishing.
            this.epoch = this.epoch.wrapping_add(1);
            this.load = None;
            this.loading = false;
            this.set_design_tokens(tokens, dark, cx);
            this.initialize_native_window(window, None, false)?;
            #[cfg(windows)]
            host.reopen_set_hwnd(platform::hwnd(window)?.0 as isize);
            let generation = host.reopen_native_snapshot()["generation"].as_u64().unwrap_or(0);
            this.display = generation;
            this.target_hwnd = target;
            this.active = true;
            this.leave_locked = false;
            this.had_activation = false;
            this.confirm = None;
            this.context = None;
            let owner = host.clone();
            let _show = platform::guard_condition(move || {
                let state = owner.reopen_native_snapshot();
                state["visible"].as_bool() == Some(true) && state["generation"].as_u64() == Some(generation)
            });
            platform::set_no_activate(window, false)?;
            platform::show(window, true, true)?;
            this.search.read(cx).focus_handle().focus(window);
            this.reload(cx);
            Ok::<_, String>(())
        }).map_err(|error| error.to_string())??;
        Ok(())
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| this.suspend(window, cx));
    }

    pub fn initialize_native_window(&mut self, window: &Window, dimensions: Option<(f64, f64)>, no_activate: bool) -> Result<(), String> {
        if !self.initialized {
            let dimensions = dimensions.unwrap_or((420.0, 560.0));
            if !dimensions.0.is_finite() || !dimensions.1.is_finite() || dimensions.0 <= 0.0 || dimensions.1 <= 0.0 {
                return Err("Invalid Reopen initial client size".into());
            }
            let _configuration = platform::guard_condition(|| true);
            platform::configure_host(window)?;
            platform::initialize_ceabox_geometry(window, dimensions)?;
            self.initialized = true;
        }
        platform::set_no_activate(window, no_activate)
    }

    /// Explicit isolated visual entry only; the caller creates a fresh .tmp
    /// core and keeps process-wide headless mode enabled until process exit.
    #[cfg(debug_assertions)]
    pub fn new_fixture(host: Arc<HostState>, tokens: DesignTokens, dark: bool, quick: bool, query: &str, settings_open: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        assert!(cliphost_core::clipboard::is_headless(), "Reopen visual fixtures require headless core effects");
        if quick { host.reopen_show(); }
        let mut this = Self::new(host.clone(), tokens, dark, quick, window, cx);
        if quick {
            #[cfg(windows)]
            host.reopen_set_hwnd(platform::hwnd(window).map(|hwnd| hwnd.0 as isize).unwrap_or(0));
            this.display = host.reopen_native_snapshot()["generation"].as_u64().unwrap_or(0);
        }
        this.settings_open = settings_open && !quick;
        this.search.update(cx, |input, cx| input.set_text(query.to_string(), window, cx));
        this.limit_input.update(cx, |input, cx| input.set_text("200".into(), window, cx));
        this.menu_input.update(cx, |input, cx| input.set_text("10".into(), window, cx));
        this.query = query.into();
        this.reload(cx);
        this
    }

    pub fn save_and_close(handle: WindowHandle<Self>, cx: &mut App) -> crate::settings::SettingsCloseResult {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        let result = handle.update(cx, |this, window, cx| {
            this.leave_locked = true;
            if this.busy { this.close_waiters.push(sender.take().unwrap()); }
            else if let Some(error) = this.persistence_error() {
                this.leave_locked = false;
                let _ = sender.take().unwrap().send(Err(error));
            } else {
                this.dismiss(window, cx);
                let _ = sender.take().unwrap().send(Ok(()));
            }
        });
        if result.is_err() { if let Some(sender) = sender { let _ = sender.send(Ok(())); } }
        receiver
    }

    fn settle_close_waiters(&mut self, result: Result<(), String>) {
        if self.close_waiters.is_empty() { return; }
        if result.is_ok() { self.host.reopen_hide(); }
        else { self.leave_locked = false; }
        for waiter in std::mem::take(&mut self.close_waiters) { let _ = waiter.send(result.clone()); }
    }

    pub fn pending_writes(&self) -> bool { self.busy || self.queued_settings.is_some() }
    pub fn persistence_error(&self) -> Option<String> { self.failed_request.as_ref().and(self.error.clone()) }

    pub fn prepare_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if self.is_composing(cx) { return Err(tr("请先完成输入法组合", "Finish composing text first").into()); }
        if let Some(error) = self.persistence_error() { return Err(error); }
        if self.settings_open {
            let history = self.limit_input.read(cx).text().trim().parse::<u64>().map_err(|_| tr("请输入有效的历史上限", "Enter a valid history limit"))?.clamp(1, 2000);
            let menu = self.menu_input.read(cx).text().trim().parse::<u64>().map_err(|_| tr("请输入有效的菜单数量", "Enter a valid menu count"))?.clamp(1, history.min(100));
            if self.snapshot["historyLimit"].as_u64() != Some(history) || self.snapshot["menuLimit"].as_u64() != Some(menu) {
                self.mutate(json!({"action":"settings","historyLimit":history,"menuLimit":menu}), cx);
            }
            self.limit_input.update(cx, |input, cx| input.set_text(history.to_string(), window, cx));
            self.menu_input.update(cx, |input, cx| input.set_text(menu.to_string(), window, cx));
        }
        Ok(())
    }

    pub fn maintain(handle: WindowHandle<Self>, dark: bool, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| {
            let state = this.host.reopen_native_snapshot();
            if state["visible"].as_bool() != Some(true) || state["generation"].as_u64() != Some(this.display) {
                if this.active { this.suspend(window, cx); }
                return;
            }
            if this.dark != dark { this.set_design_tokens(this.tokens.clone(), dark, cx); }
            if window.is_window_active() { this.had_activation = true; }
            else if this.had_activation && !this.busy && this.confirm.is_none() { this.dismiss(window, cx); }
        });
    }

    pub fn refresh(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, _, cx| { if this.active { this.reload(cx); } });
    }

    pub fn refresh_page(&mut self, cx: &mut Context<Self>) { self.reload(cx); }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active { return; }
        self.active = active;
        self.epoch = self.epoch.wrapping_add(1);
        self.load = None;
        self.loading = false;
        self.context = None;
        self.confirm = None;
        self.more = false;
        self.selected.clear();
        if active && !self.busy { self.reload(cx); }
        cx.notify();
    }

    pub fn set_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.leave_locked = locked;
        cx.notify();
    }

    pub fn is_composing(&self, cx: &App) -> bool {
        self.search.read(cx).is_composing() || self.limit_input.read(cx).is_composing() || self.menu_input.read(cx).is_composing()
    }

    pub fn focus_handles(&self, cx: &App) -> Vec<FocusHandle> {
        let mut handles = vec![self.search.read(cx).focus_handle(), self.focus.clone()];
        if self.settings_open { handles.extend([self.limit_input.read(cx).focus_handle(), self.menu_input.read(cx).focus_handle()]); }
        handles
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
            background: color(&t.search_background), foreground: color(&t.text),
            placeholder: color(&t.text_muted), border: gpui::transparent_black(),
            selection: color(&t.selected), caret: color(&t.accent),
        };
        for input in [&self.search, &self.limit_input, &self.menu_input] {
            input.update(cx, |input, cx| { input.set_colors(colors, cx); input.set_metrics(30.0, 12.0, 0.0, cx); });
        }
        self.search.update(cx, |input, cx| input.set_placeholder(tr("搜索目录名称或路径", "Search folder names or paths"), cx));
    }

    fn current(&self) -> bool {
        if !self.active { return false; }
        if !self.quick { return true; }
        let state = self.host.reopen_native_snapshot();
        state["visible"].as_bool() == Some(true) && state["generation"].as_u64() == Some(self.display)
    }

    fn suspend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_active(false, cx);
        self.scrollbar.cancel();
        let _ = platform::hide(window);
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.host.reopen_hide();
        self.suspend(window, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.active { return; }
        if self.busy { self.refresh_pending = true; return; }
        self.refresh_pending = false;
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        self.loading = true;
        let host = self.host.clone();
        self.load = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { host.reopen_history_snapshot() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.epoch != epoch || !this.current() { return; }
                this.loading = false;
                match result {
                    Ok(snapshot) => { this.snapshot = snapshot; if this.failed_request.is_none() { this.error = None; } this.reproject(true, cx); }
                    Err(error) => this.error = Some(error),
                }
                if this.refresh_pending && this.current() { this.reload(cx); }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn entries(&self) -> Vec<Value> { self.snapshot["entries"].as_array().cloned().unwrap_or_default() }

    fn reproject(&mut self, preserve: bool, cx: &mut Context<Self>) {
        let old = self.projection.iter().map(row_key).collect::<Vec<_>>();
        self.projection = if self.quick {
            let opened = self.snapshot["opened"].as_array().cloned().unwrap_or_default().into_iter()
                .filter(|row| matches_query(row, &self.query)).collect::<Vec<_>>();
            let recent = self.entries().into_iter().filter(|row| row["opened"].as_bool() != Some(true) && matches_query(row, &self.query))
                .take(self.snapshot["menuLimit"].as_u64().unwrap_or(10).clamp(1, 100) as usize).collect::<Vec<_>>();
            let mut projection = Vec::new();
            if !opened.is_empty() { projection.push(Row::Label(tr("当前已打开", "Currently open"))); projection.extend(opened.into_iter().map(Row::Opened)); }
            projection.push(Row::Label(tr("最近记录", "Recently recorded")));
            projection.extend(recent.into_iter().map(Row::History));
            projection
        } else if self.tab == Tab::History {
            self.entries().into_iter().filter(|row| matches_query(row, &self.query)).map(Row::History).collect()
        } else {
            self.snapshot["exclusions"].as_array().cloned().unwrap_or_default().into_iter()
                .filter(|row| matches_query(row, &self.query)).map(Row::Exclusion).collect()
        };
        let keys = self.projection.iter().map(row_key).collect::<Vec<_>>();
        if preserve { crate::list_reconcile::reconcile(&self.list, &old, &keys); }
        else { self.list.reset(keys.len()); self.list.scroll_to(gpui::ListOffset { item_ix: 0, offset_in_item: px(0.0) }); }
        let current = keys.iter().cloned().collect::<HashSet<_>>();
        self.selected.retain(|key| current.contains(key));
        self.cursor = self.cursor.filter(|index| *index < keys.len());
        self.scrollbar.invalidate_measurements();
        cx.notify();
    }

    fn mutate(&mut self, request: Value, cx: &mut Context<Self>) {
        if self.leave_locked || !self.current() { return; }
        if self.busy {
            if request["action"] == "settings" {
                let pending = self.queued_settings.get_or_insert_with(|| json!({"action":"settings"}));
                if let (Some(pending), Some(incoming)) = (pending.as_object_mut(), request.as_object()) {
                    pending.extend(incoming.iter().map(|(key, value)| (key.clone(), value.clone())));
                }
                cx.notify();
            }
            return;
        }
        self.start_mutation(request, cx);
    }

    fn start_mutation(&mut self, request: Value, cx: &mut Context<Self>) {
        self.busy = true;
        self.loading = false;
        self.load = None;
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let host = self.host.clone();
        let retry = request.clone();
        let action = request["action"].as_str().unwrap_or("").to_string();
        if self.failed_request.as_ref().is_none_or(|request| request["action"] == "settings") { self.error = None; }
        self.mutation = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { host.reopen_history_apply(&request) }).await;
            let _ = this.update(cx, |this, cx| {
                // Mutations are accepted once and may finish after navigation.
                // Their persistence does not depend on presentation ownership.
                this.busy = false;
                if let Err(error) = &result {
                    this.error = Some(error.clone());
                    this.failed_request = Some(retry.clone());
                }
                if let Some(mut next) = this.queued_settings.take() {
                    if result.is_err() && action == "settings" {
                        let mut combined = retry.clone();
                        if let (Some(combined), Some(incoming)) = (combined.as_object_mut(), next.as_object()) {
                            combined.extend(incoming.iter().map(|(key, value)| (key.clone(), value.clone())));
                        }
                        next = combined;
                    }
                    this.start_mutation(next, cx);
                    return;
                }
                let close_result = result.as_ref().map(|_| ()).map_err(Clone::clone).and_then(|_| {
                    if action == "settings" && this.failed_request.as_ref().is_some_and(|request| request["action"] != "settings") {
                        Err(this.error.clone().unwrap_or_else(|| tr("仍有未完成的历史修改", "A history change still needs attention").into()))
                    } else { Ok(()) }
                });
                this.settle_close_waiters(close_result);
                if this.epoch != epoch || !this.current() {
                    if this.active { this.reload(cx); }
                    return;
                }
                match result {
                    Ok(reply) => {
                        let retain_failure = action == "settings" && this.failed_request.as_ref().is_some_and(|request| request["action"] != "settings");
                        this.snapshot = reply["snapshot"].clone();
                        this.undo = reply.get("undo").filter(|undo| !undo.is_null()).cloned();
                        if !retain_failure {
                            this.selected.clear(); this.context = None; this.confirm = None; this.more = false;
                            this.failed_request = None;
                            this.error = None;
                        }
                        let changed = reply["changed"].as_u64().unwrap_or(0);
                        let skipped = reply["skipped"].as_u64().unwrap_or(0);
                        if !retain_failure { this.feedback = Some(if skipped > 0 {
                            cliphost_core::trf!("已处理 {} 项，{} 项已有变化并保留", "Processed {} items; kept {} changed items", changed, skipped)
                        } else if action == "exclude" {
                            tr("已设为不再记录", "Recording exclusion added").into()
                        } else if action == "unexclude" {
                            tr("已解除规则，未来可重新记录", "Exclusion removed; future recording is allowed").into()
                        } else if action == "settings" {
                            tr("记录设置已保存", "Recording settings saved").into()
                        } else if action == "restore" {
                            cliphost_core::trf!("已恢复 {} 条记录", "Restored {} records", changed)
                        } else { cliphost_core::trf!("已移除 {} 条记录", "Removed {} records", changed) }); }
                        this.reproject(true, cx);
                    }
                    Err(error) => { this.error = Some(error); this.failed_request = Some(retry); }
                }
                if this.refresh_pending && this.current() { this.reload(cx); }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn selected_entries(&self) -> Vec<Value> {
        self.projection.iter().filter_map(|row| match row { Row::History(entry) if self.selected.contains(&entry_key(entry)) => Some(entry.clone()), _ => None }).collect()
    }

    fn commit_limit(&mut self, _history: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_open || self.limit_input.read(cx).is_composing() || self.menu_input.read(cx).is_composing() { return; }
        let history = self.limit_input.read(cx).text().trim().parse::<u64>();
        let menu = self.menu_input.read(cx).text().trim().parse::<u64>();
        let (Ok(history), Ok(menu)) = (history, menu) else {
            self.error = Some(tr("请输入有效的记录数量", "Enter a valid record count").into());
            cx.notify(); return;
        };
        let history = history.clamp(1, 2000);
        let menu = menu.clamp(1, history.min(100));
        if self.snapshot["historyLimit"].as_u64() != Some(history) || self.snapshot["menuLimit"].as_u64() != Some(menu) || self.queued_settings.is_some() {
            self.mutate(json!({"action":"settings","historyLimit":history,"menuLimit":menu}), cx);
        }
        self.limit_input.update(cx, |input, cx| input.set_text(history.to_string(), window, cx));
        self.menu_input.update(cx, |input, cx| input.set_text(menu.to_string(), window, cx));
    }

    fn open_entry(&mut self, row: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.leave_locked || !self.current() { return; }
        let path = path_of(&row).to_string();
        let folder = row["hwnd"].as_i64().unwrap_or(0);
        self.perform_path(path, "open", folder, window, cx);
    }

    fn perform_path(&mut self, path: String, operation: &'static str, folder_hwnd: i64, _window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || !self.current() { return; }
        let request = if self.quick && operation == "open" {
            let Some(request) = self.host.reopen_accept_open() else { return; }; Some(request)
        } else { None };
        let target = self.target_hwnd;
        self.context = None;
        self.busy = true;
        let host = self.host.clone();
        let epoch = self.epoch;
        let display = self.display;
        self.mutation = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                match operation {
                    "copy" => host.reopen_copy_path(&path),
                    "add" => host.file_menu_add(&path).map(|_| ()),
                    _ => if let Some(request) = &request { host.reopen_execute_path(&path, request, folder_hwnd) }
                        else if folder_hwnd != 0 { host.reopen_opened_path(&path, folder_hwnd, target) }
                        else { host.reopen_open_path(&path, target) },
                }
            }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if let Some(next) = this.queued_settings.take() { this.start_mutation(next, cx); return; }
                this.settle_close_waiters(result.clone().and_then(|_| this.persistence_error().map_or(Ok(()), Err)));
                if !this.current() || this.epoch != epoch || (this.quick && this.display != display) {
                    if this.active { this.reload(cx); }
                    return;
                }
                match result {
                    Ok(()) => {
                        if this.failed_request.is_none() { this.error = None; }
                        this.feedback = Some(match operation {
                            "copy" => tr("已复制路径", "Path copied"),
                            "add" => tr("已添加到文件菜单", "Added to File menu"),
                            _ => tr("已打开", "Opened"),
                        }.into());
                        if this.quick && operation == "open" { this.host.reopen_hide(); }
                    }
                    Err(error) => this.error = Some(error),
                }
                if this.refresh_pending && this.current() { this.reload(cx); }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.cursor.and_then(|index| self.projection.get(index)).or_else(|| self.projection.iter().find(|row| matches!(row, Row::History(_) | Row::Opened(_))));
        if let Some(Row::History(row) | Row::Opened(row)) = row.cloned() { self.open_entry(row, window, cx); }
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.take().is_some() || self.context.take().is_some() || std::mem::take(&mut self.more) { cx.notify(); return; }
        if !self.selected.is_empty() { self.selected.clear(); cx.notify(); return; }
        if self.quick { self.dismiss(window, cx); }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) { return; }
        let key = event.keystroke.key.as_str();
        if key == "escape" { self.escape(window, cx); window.prevent_default(); cx.stop_propagation(); return; }
        if let Some(confirm) = self.confirm.clone() {
            let controls = match &confirm { Confirm::Clear { .. } => 2, Confirm::Exclude { path, .. } if is_shell(path) => 3, _ => 4 };
            if key == "tab" {
                self.dialog_selection = (self.dialog_selection + if event.keystroke.modifiers.shift { controls - 1 } else { 1 }) % controls;
                self.focus.focus(window);
            } else if matches!(key, "enter" | "space") {
                match self.dialog_selection {
                    0 => self.confirm = None,
                    1 => self.confirm_apply(cx),
                    2 => if let Some(Confirm::Exclude { path, children, remove, .. }) = &mut self.confirm { if is_shell(path) { *remove = !*remove; } else { *children = !*children; } },
                    _ => if let Some(Confirm::Exclude { remove, .. }) = &mut self.confirm { *remove = !*remove; },
                }
            }
            window.prevent_default(); cx.stop_propagation(); cx.notify(); return;
        }
        if let Some(row) = self.context.clone() {
            if matches!(key, "up" | "down" | "tab") {
                let backwards = key == "up" || (key == "tab" && event.keystroke.modifiers.shift);
                self.context_selection = (self.context_selection + if backwards { 4 } else { 1 }) % 5;
            } else if matches!(key, "enter" | "space") {
                let path = path_of(&row).to_string();
                match self.context_selection {
                    0 => self.open_entry(row, window, cx),
                    1 => self.mutate(json!({"action":"remove","entries":[row]}), cx),
                    2 => { self.context = None; self.dialog_selection = 0; self.confirm = Some(Confirm::Exclude { path, entries: self.entries(), children: false, remove: false }); },
                    3 => self.perform_path(path, "copy", 0, window, cx),
                    _ => self.perform_path(path, "add", 0, window, cx),
                }
            }
            window.prevent_default(); cx.stop_propagation(); cx.notify(); return;
        }
        let text_focused = [&self.search, &self.limit_input, &self.menu_input].iter().any(|input| input.read(cx).focus_handle().is_focused(window));
        if matches!(key, "up" | "down") {
            let selectable = self.projection.iter().enumerate().filter(|(_, row)| !matches!(row, Row::Label(_))).map(|(index, _)| index).collect::<Vec<_>>();
            if let Some(first) = selectable.first().copied() {
                let current = self.cursor.and_then(|cursor| selectable.iter().position(|index| *index == cursor));
                let next = if key == "down" { current.map_or(0, |index| (index + 1).min(selectable.len() - 1)) } else { current.unwrap_or(0).saturating_sub(1) };
                self.cursor = Some(selectable.get(next).copied().unwrap_or(first));
                self.list.scroll_to_reveal_item(self.cursor.unwrap());
                window.prevent_default(); cx.stop_propagation(); cx.notify();
            }
        } else if !text_focused && key == "enter" { self.open_cursor(window, cx); cx.stop_propagation(); }
        else if !text_focused && key == "space" && !self.quick {
            if let Some(Row::History(row)) = self.cursor.and_then(|index| self.projection.get(index)) {
                let key = entry_key(row);
                if !self.selected.remove(&key) { self.selected.insert(key); }
                cx.notify(); cx.stop_propagation();
            }
        } else if key == "f10" && event.keystroke.modifiers.shift {
            if let Some(Row::History(row)) = self.cursor.and_then(|index| self.projection.get(index)) {
                self.context = Some(row.clone()); cx.notify(); cx.stop_propagation();
            }
        }
    }

    pub fn has_overlay(&self) -> bool { self.confirm.is_some() || self.context.is_some() }

    pub fn overlay_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) { self.key_down(event, window, cx); }

    fn confirm_apply(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.clone() else { return; };
        let request = match confirm {
            Confirm::Clear { entries } => json!({"action":"remove","entries":entries}),
            Confirm::Exclude { path, entries, children, remove } => json!({"action":"exclude","path":path,"includeChildren":children,"removeExisting":remove,"entries":entries}),
        };
        self.mutate(request, cx);
    }

    fn icon(&self, name: &str, size: f32) -> crate::crisp_svg::CrispSvg {
        svg().path(self.tokens.icons.get(name).cloned().unwrap_or_default()).size(px(size))
    }

    fn button(&self, id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        div().id(id.into()).min_h(px(28.0)).px(px(9.0)).py(px(4.0))
            .flex_none().flex().items_center().justify_center().gap(px(6.0))
            .rounded(px(5.0)).border_1().border_color(color(&t.border))
            .bg(color(&t.surface)).text_size(px(12.0)).text_color(color(&t.text_secondary))
            .cursor_pointer().hover(|style| style.bg(color(&t.hover)))
            .child(label.into())
    }

    fn checkbox(&self, checked: bool, t: &ThemeTokens) -> gpui::Div {
        div().size(px(14.0)).flex_none().rounded(px(3.0)).border_1().border_color(color(&t.border))
            .bg(if checked { color(&t.accent) } else { gpui::transparent_black() })
            .flex().items_center().justify_center()
            .when(checked, |element| element.child(self.icon("check", 11.0).text_color(color(&t.background))))
    }

    fn render_row(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.projection.get(index).cloned() else { return div().into_any_element(); };
        let t = self.tokens.theme(self.dark).clone();
        if let Row::Label(label) = row {
            return div().h(px(28.0)).px(px(12.0)).pt(px(8.0)).text_size(px(11.0)).text_color(color(&t.text_muted)).child(label).into_any_element();
        }
        let (value, opened, rule) = match row { Row::History(row) => (row, false, false), Row::Opened(row) => (row, true, false), Row::Exclusion(row) => (row, false, true), Row::Label(_) => unreachable!() };
        let key = entry_key(&value);
        let selected = self.selected.contains(&key);
        let name = if rule { path_of(&value).to_string() } else { title(&value) };
        let path = path_of(&value).to_string();
        let mut element = rsx! { <div id={SharedString::from(format!("reopen-row-{index}"))} min_h={px(if rule { 66.0 } else { 58.0 })}
            w_full px={px(10.0)} py={px(7.0)} flex items_center gap={px(10.0)} rounded={px(5.0)}
            bg={if selected || self.cursor == Some(index) { color(&t.selected) } else { gpui::transparent_black() }} /> }
            .hover(|style| style.bg(color(&t.hover)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| { this.cursor = Some(index); this.focus.focus(window); cx.notify(); }));
        if !self.quick && !rule {
            let entry_key = key.clone();
            element = element.child(div().id(SharedString::from(format!("reopen-check-{index}"))).p(px(3.0)).cursor_pointer().child(self.checkbox(selected, &t))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy || this.leave_locked { return; }
                    if !this.selected.remove(&entry_key) { this.selected.insert(entry_key.clone()); }
                    cx.stop_propagation(); cx.notify();
                })));
        }
        element = element.child(self.icon(if opened || value["opened"].as_bool() == Some(true) { "folder-open" } else { "folder-closed" }, 18.0).text_color(color(&t.text_muted)));
        let open_value = value.clone();
        let mut text = div().id(SharedString::from(format!("reopen-main-{index}"))).flex_1().min_w_0().flex().flex_col().gap(px(3.0)).cursor_pointer()
            .child(div().w_full().min_w_0().h(px(18.0)).flex_none().flex().items_center().gap(px(7.0)).child(div().flex_1().min_w_0().line_clamp(1).text_size(px(12.0)).child(name))
                .when(!rule && !opened && value["opened"].as_bool() == Some(true), |element| element.child(div().flex_none().text_size(px(10.0)).text_color(color(&t.text_muted)).child(tr("当前已打开", "Open now")))))
            .child(div().w_full().min_w_0().h(px(16.0)).flex_none().line_clamp(1).text_size(px(11.0)).text_color(color(&t.text_muted)).child(if rule {
                if value["includeChildren"].as_bool() == Some(true) { tr("此目录及其子目录", "This folder and its subfolders").into() } else { tr("仅此目录", "This folder only").into() }
            } else { path.clone() }));
        text = text.tooltip(crate::settings::settings_help(path, t.clone())).on_click(cx.listener(move |this, _, window, cx| {
            if !rule { this.open_entry(open_value.clone(), window, cx); }
        }));
        element = element.child(text);
        if rule {
            let rule = value.clone();
            element = element.child(self.button(SharedString::from(format!("reopen-rule-remove-{index}")), tr("解除", "Remove"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mutate(json!({"action":"unexclude","path":rule["path"],"revision":rule["revision"]}), cx);
                })));
        } else if opened {
            element = element.child(div().flex_none().text_size(px(11.0)).text_color(color(&t.text_muted)).child(tr("切换到", "Switch to")));
        } else {
            if !self.quick { element = element.child(div().flex_none().text_size(px(11.0)).text_color(color(&t.text_muted)).child(record_time(&value))); }
            let context_value = value.clone();
            let right_value = value.clone();
            element = element.child(div().id(SharedString::from(format!("reopen-more-{index}"))).w(px(26.0)).h(px(28.0)).flex_none().flex().items_center().justify_center().cursor_pointer().child(self.icon("more", 15.0).text_color(color(&t.text_muted)))
                .on_click(cx.listener(move |this, _, _, cx| { this.context = Some(context_value.clone()); this.context_selection = 0; cx.stop_propagation(); cx.notify(); })))
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, cx| { this.context = Some(right_value.clone()); this.context_selection = 0; cx.stop_propagation(); cx.notify(); }));
        }
        element.into_any_element()
    }

    fn render_context(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.context.clone() else { return div().into_any_element(); };
        let t = self.tokens.theme(self.dark).clone();
        let path = path_of(&row).to_string();
        let open_row = row.clone();
        let remove_row = row.clone();
        let exclude_row = row.clone();
        let copy_path = path.clone();
        let add_path = path;
        let card = rsx! { <div absolute right={px(18.0)} top={px(118.0)} w={px(246.0)} max_w_full p={px(6.0)} flex flex_col gap={px(2.0)}
            rounded={px(7.0)} border_1 border_color={color(&t.border)} bg={color(&t.surface)} shadow_lg /> }
            .child(div().px(px(8.0)).py(px(5.0)).text_size(px(11.0)).truncate().text_color(color(&t.text_muted)).child(title(&row)))
            .child(self.button("reopen-context-open", tr("打开", "Open")).on_click(cx.listener(move |this, _, window, cx| this.open_entry(open_row.clone(), window, cx))))
            .child(self.button("reopen-context-remove", tr("从重开历史移除", "Remove from Reopen history")).on_click(cx.listener(move |this, _, _, cx| this.mutate(json!({"action":"remove","entries":[remove_row.clone()]}), cx))))
            .child(self.button("reopen-context-exclude", tr("不再记录此目录…", "Stop recording this folder…")).on_click(cx.listener(move |this, _, _, cx| {
                this.context = None;
                this.dialog_selection = 0;
                this.confirm = Some(Confirm::Exclude { path: path_of(&exclude_row).into(), entries: this.entries(), children: false, remove: false });
                cx.notify();
            })))
            .child(self.button("reopen-context-copy", tr("复制路径", "Copy path")).on_click(cx.listener(move |this, _, window, cx| this.perform_path(copy_path.clone(), "copy", 0, window, cx))))
            .child(self.button("reopen-context-add", tr("添加到文件菜单", "Add to File menu")).on_click(cx.listener(move |this, _, window, cx| this.perform_path(add_path.clone(), "add", 0, window, cx))));
        div().absolute().inset_0().child(div().id("reopen-context-dismiss").absolute().inset_0().on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| { this.context = None; cx.notify(); }))).child(card).into_any_element()
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(confirm) = self.confirm.clone() else { return div().into_any_element(); };
        let t = self.tokens.theme(self.dark).clone();
        let mut body = div().w_full().min_w_0().flex().flex_col().gap(px(12.0));
        match &confirm {
            Confirm::Clear { entries } => {
                body = body.child(div().text_size(px(16.0)).child(tr("清空全部重开历史？", "Clear all Reopen history?")))
                    .child(div().text_size(px(12.0)).child(cliphost_core::trf!("将移除全部 {} 条历史，包括当前搜索结果之外的记录。", "Remove all {} records, including those outside the current search.", entries.len())))
                    .child(div().text_size(px(12.0)).text_color(color(&t.text_muted)).child(tr("不会删除实际文件夹，不影响排除规则，也不会暂停以后记录。", "Folders and exclusions are unchanged. Future recording is not paused.")));
            }
            Confirm::Exclude { path, entries, children, remove } => {
                let count = if *remove { entries.iter().filter(|entry| matches_path(path_of(entry), path, *children)).count() } else { 0 };
                body = body.child(div().text_size(px(16.0)).child(tr("不再记录此目录", "Stop recording this folder")))
                    .child(div().p(px(9.0)).rounded(px(5.0)).bg(color(&t.background)).text_size(px(12.0)).child(path.clone()));
                if !is_shell(path) {
                    body = body.child(div().id("reopen-exclude-children").flex().items_center().gap(px(8.0)).cursor_pointer().child(self.checkbox(*children, &t)).child(tr("同时包含所有子目录", "Also include all subfolders"))
                        .on_click(cx.listener(|this, _, _, cx| { if let Some(Confirm::Exclude { children, .. }) = &mut this.confirm { *children = !*children; } cx.notify(); })));
                }
                body = body.child(div().id("reopen-exclude-existing").flex().items_center().gap(px(8.0)).cursor_pointer().child(self.checkbox(*remove, &t)).child(tr("同时移除现有的匹配历史记录", "Also remove existing matching history"))
                    .on_click(cx.listener(|this, _, _, cx| { if let Some(Confirm::Exclude { remove, .. }) = &mut this.confirm { *remove = !*remove; } cx.notify(); })))
                    .child(div().text_size(px(11.0)).text_color(color(&t.text_muted)).child(cliphost_core::trf!("将移除 {} 条现有记录；实际文件夹不受影响。", "Remove {} existing records; folders are unchanged.", count)));
            }
        }
        body = body.child(div().flex().items_center().justify_end().gap(px(8.0))
            .child(self.button("reopen-confirm-cancel", tr("取消", "Cancel")).border_color(color(if self.dialog_selection == 0 { &t.accent } else { &t.border })).on_click(cx.listener(|this, _, _, cx| { this.confirm = None; cx.notify(); })))
            .child(self.button("reopen-confirm-accept", match confirm { Confirm::Clear { .. } => tr("清空历史", "Clear history"), _ => tr("确定", "Confirm") })
                .bg(color(&t.accent)).text_color(color(&t.background)).on_click(cx.listener(|this, _, _, cx| {
                    this.confirm_apply(cx);
                }))));
        div().absolute().inset_0().p(px(18.0)).flex().items_center().justify_center().bg(gpui::rgba(0x00000050))
            .child(div().id("reopen-confirm-scroll").w(px(410.0)).max_w_full().max_h_full().overflow_y_scroll().p(px(20.0)).rounded(px(9.0)).border_1().border_color(color(&t.border)).bg(color(&t.surface)).shadow_lg().child(body))
            .into_any_element()
    }

    pub fn diagnostics(&self, cx: &App) -> Value {
        json!({"quick":self.quick,"active":self.active,"busy":self.busy,"loading":self.loading,
            "historyCount":self.entries().len(),"rows":self.projection.len(),"selected":self.selected.len(),
            "query":self.query,"error":self.error,"display":self.display,"input":self.search.read(cx).diagnostics()})
    }
}

fn row_key(row: &Row) -> String {
    match row { Row::Label(label) => format!("label:{label}"), Row::History(row) => entry_key(row), Row::Opened(row) => format!("opened:{}:{}", path_of(row), row["hwnd"]), Row::Exclusion(row) => format!("excluded:{}:{}", path_of(row), row["revision"]) }
}

impl Render for ReopenWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        let recording = self.snapshot["recording"].as_bool().unwrap_or(true);
        let count = self.snapshot["entries"].as_array().map_or(0, Vec::len);
        let exclusions = self.snapshot["exclusions"].as_array().map_or(0, Vec::len);
        let mut header = rsx! { <div flex_none px={px(18.0)} pt={px(15.0)} pb={px(10.0)} flex items_center gap={px(10.0)} /> }
            .child(self.icon("folder-open", 17.0).text_color(color(&t.accent)))
            .child(div().id("reopen-caption-drag").flex_1().min_w_0().flex().flex_col().gap(px(3.0)).child(div().text_size(px(if self.quick { 15.0 } else { 18.0 })).child(tr("重开", "Reopen")))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.quick { let _ = platform::begin_owned_move_at_client(window, event.position); cx.stop_propagation(); }
                }))
                .when(!self.quick, |element| element.child(div().text_size(px(11.0)).text_color(color(&t.text_muted)).child(tr("整理最近使用的文件夹", "Organize recently used folders")))));
        if self.quick {
            header = header.child(self.button("reopen-close", "×").on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))));
        } else {
            header = header.child(self.button("reopen-recording", if recording { tr("记录中", "Recording") } else { tr("已暂停记录", "Recording paused") })
                .text_color(color(if recording { &t.accent } else { &t.text_muted }))
                .on_click(cx.listener(move |this, _, _, cx| this.mutate(json!({"action":"settings","recording":!recording}), cx))))
                .child(self.button("reopen-more", "⋯").on_click(cx.listener(|this, _, _, cx| { this.more = !this.more; this.context = None; cx.notify(); })));
        }
        let search = rsx! { <div flex_none mx={px(18.0)} mb={px(10.0)} px={px(9.0)} h={px(34.0)} flex items_center gap={px(8.0)} rounded={px(6.0)} border_1 border_color={color(&t.border)} bg={color(&t.search_background)} /> }
            .child(self.icon("search", 15.0).text_color(color(&t.text_muted)))
            .child(div().flex_1().min_w_0().child(self.search.clone()))
            .when(!self.query.is_empty(), |element| element.child(self.button("reopen-clear-search", "×").border_0().bg(gpui::transparent_black()).on_click(cx.listener(|this, _, window, cx| {
                this.search.update(cx, |input, cx| input.set_text(String::new(), window, cx));
                this.query.clear(); this.selected.clear(); this.reproject(false, cx);
            }))));
        let mut body = div().relative().size_full().flex().flex_col().overflow_hidden().track_focus(&self.focus)
            .font_family(self.tokens.typography.font_family.clone()).text_size(px(12.0)).text_color(color(&t.text)).bg(color(&t.background))
            .capture_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .child(header).child(search);
        if !self.quick {
            let mut tabs = div().flex_none().h(px(36.0)).px(px(18.0)).flex().items_center().gap(px(14.0)).border_b_1().border_color(color(&t.border));
            for (tab, label) in [(Tab::History, format!("{} {count}", tr("历史记录", "History"))), (Tab::Exclusions, format!("{} {exclusions}", tr("不再记录", "Excluded")))] {
                tabs = tabs.child(self.button(if tab == Tab::History { "reopen-history-tab" } else { "reopen-exclusions-tab" }, label)
                    .border_0().bg(gpui::transparent_black()).text_color(color(if self.tab == tab { &t.accent } else { &t.text_muted }))
                    .on_click(cx.listener(move |this, _, _, cx| { this.tab = tab; this.selected.clear(); this.context = None; this.reproject(false, cx); })));
            }
            body = body.child(tabs);
            if !recording { body = body.child(div().flex_none().px(px(18.0)).py(px(7.0)).text_size(px(11.0)).text_color(color(&t.text_muted)).child(tr("已暂停新增，已有记录仍可使用和管理", "New recording is paused. Existing history remains available."))); }
            if self.tab == Tab::History {
                let visible_count = self.projection.len();
                let selected = self.selected.len();
                let all = selected > 0 && selected == visible_count;
                let mut selection = div().flex_none().h(px(36.0)).px(px(18.0)).flex().items_center().gap(px(9.0))
                    .child(div().id("reopen-select-all").flex().items_center().gap(px(9.0)).cursor_pointer().child(self.checkbox(all, &t))
                        .child(div().text_size(px(11.0)).text_color(color(&t.text_muted)).child(if selected == 0 { cliphost_core::trf!("当前结果 {} 条", "{} results", visible_count) } else { cliphost_core::trf!("已选 {} 条", "{} selected", selected) }))
                        .on_click(cx.listener(move |this, _, _, cx| { if this.busy { return; } this.selected = if all { HashSet::new() } else { this.projection.iter().filter_map(|row| if let Row::History(row) = row { Some(entry_key(row)) } else { None }).collect() }; cx.notify(); })));
                selection = selection.child(div().flex_1());
                selection = if selected > 0 { selection.child(self.button("reopen-remove-selected", tr("移除记录", "Remove records")).on_click(cx.listener(|this, _, _, cx| this.mutate(json!({"action":"remove","entries":this.selected_entries()}), cx)))) }
                    else { selection.child(div().text_size(px(11.0)).text_color(color(&t.text_muted)).child(tr("最近记录", "Last recorded"))) };
                body = body.child(selection);
            } else { body = body.child(div().flex_none().px(px(18.0)).py(px(9.0)).text_size(px(11.0)).text_color(color(&t.text_muted)).child(tr("解除后允许未来重新记录，不恢复已移除的历史", "Removing a rule allows future records; removed history is not restored."))); }
        }
        let weak = cx.weak_entity();
        let virtual_list = list(self.list.clone(), move |index, window, cx| weak.update(cx, |this, cx| this.render_row(index, window, cx)).unwrap_or_else(|_| div().into_any_element())).w_full().flex_1().min_h_0();
        let mut results = div().relative().flex_1().min_h_0().flex().flex_col().px(px(8.0)).pb(px(5.0));
        let has_items = self.projection.iter().any(|row| !matches!(row, Row::Label(_)));
        if has_items {
            let weak = cx.weak_entity();
            results = results.child(virtual_list).child(self.scrollbar.render("reopen-scrollbar", ScrollbarTarget::List(self.list.clone()), self.epoch,
                ScrollbarStyle { ink: color(&t.text_muted), metrics: self.tokens.scrollbar, always_visible: false },
                Rc::new(move |_, cx| { let _ = weak.update(cx, |_, cx| cx.notify()); })));
        } else {
            results = results.child(div().flex_1().min_h_0().px(px(18.0)).flex().flex_col().items_center().justify_center().gap(px(9.0)).text_color(color(&t.text_muted))
                .child(self.icon(if self.query.is_empty() { "folder-open" } else { "search" }, 28.0))
                .child(if self.loading { tr("正在读取…", "Loading…") } else if self.query.is_empty() { tr("暂无记录", "No records yet") } else { tr("没有匹配的记录", "No matching records") })
                .child(div().text_size(px(11.0)).child(if recording { tr("使用过的目录会在这里记录", "Recently used folders appear here") } else { tr("记录已暂停，已有记录仍可管理", "Recording is paused; existing history can still be managed") })));
        }
        body = body.child(results);
        if let Some(error) = self.error.clone() {
            let retry = self.failed_request.clone();
            let cancellable = self.failed_request.as_ref().is_some_and(|request| request["action"] != "settings");
            body = body.child(div().flex_none().px(px(14.0)).py(px(7.0)).flex().items_center().gap(px(8.0)).bg(color(&t.surface))
                .child(div().flex_1().min_w_0().text_size(px(11.0)).text_color(color(&t.error)).child(error))
                .child(self.button("reopen-retry", tr("重试", "Retry")).on_click(cx.listener(move |this, _, _, cx| { if let Some(request) = retry.clone() { this.mutate(request, cx); } else { this.reload(cx); } })))
                .when(cancellable, |row| row.child(self.button("reopen-cancel-failed", tr("取消本次操作", "Cancel this operation")).on_click(cx.listener(|this, _, _, cx| {
                    this.failed_request = None; this.error = None; this.confirm = None; this.context = None; cx.notify();
                })))));
        }
        if let Some(feedback) = self.feedback.clone() {
            let mut feedback_row = div().flex_none().mx(px(12.0)).mb(px(6.0)).px(px(9.0)).py(px(5.0)).flex().items_center().gap(px(6.0)).rounded(px(5.0)).border_1().border_color(color(&t.border)).bg(color(&t.surface)).text_size(px(11.0))
                .child(div().flex_1().min_w_0().child(feedback));
            if let Some(undo) = self.undo.clone() { feedback_row = feedback_row.child(self.button("reopen-undo", tr("撤销", "Undo")).border_0().text_color(color(&t.accent)).on_click(cx.listener(move |this, _, _, cx| this.mutate(json!({"action":"restore","undo":undo.clone()}), cx)))); }
            feedback_row = feedback_row.child(self.button("reopen-feedback-close", "×").border_0().on_click(cx.listener(|this, _, _, cx| { this.feedback = None; this.undo = None; cx.notify(); })));
            body = body.child(feedback_row);
        }
        let footer = if self.quick {
            div().flex_none().px(px(12.0)).py(px(7.0)).border_t_1().border_color(color(&t.border)).child(self.button("reopen-manage", tr("管理重开历史…", "Manage Reopen history…")).w_full().justify_start().border_0()
                .on_click(cx.listener(|this, _, window, cx| { this.dismiss(window, cx); this.host.open_settings_page("explorer"); })))
        } else {
            div().flex_none().min_h(px(40.0)).px(px(12.0)).py(px(5.0)).border_t_1().border_color(color(&t.border)).flex().items_center().gap(px(5.0))
                .child(self.button("reopen-settings", tr("记录设置", "Recording settings")).border_0().bg(gpui::transparent_black()).on_click(cx.listener(|this, _, window, cx| {
                    if this.settings_open { this.commit_limit(true, window, cx); }
                    else {
                        this.limit_input.update(cx, |input, cx| input.set_text(this.snapshot["historyLimit"].as_u64().unwrap_or(200).to_string(), window, cx));
                        this.menu_input.update(cx, |input, cx| input.set_text(this.snapshot["menuLimit"].as_u64().unwrap_or(10).to_string(), window, cx));
                    }
                    this.settings_open = !this.settings_open; cx.notify();
                })))
                .child(self.button("reopen-file-menu-settings", tr("文件菜单设置", "File menu settings")).border_0().bg(gpui::transparent_black()).on_click(cx.listener(|this, _, _, _| this.host.open_settings_page("menu_file"))))
                .child(div().flex_1().min_w_0().text_size(px(10.0)).text_color(color(&t.text_muted)).child(tr("移除记录不会删除实际文件夹", "Removing records does not delete folders")))
        };
        body = body.child(footer);
        if self.settings_open && !self.quick {
            let show_menu = self.snapshot["showMenu"].as_bool().unwrap_or(false);
            let controls = div().flex_none().px(px(18.0)).py(px(10.0)).flex().flex_col().gap(px(9.0)).border_t_1().border_color(color(&t.border)).bg(color(&t.surface))
                .child(div().flex().flex_wrap().items_center().gap(px(10.0)).text_size(px(11.0))
                    .child(tr("历史上限", "History limit"))
                    .child(div().w(px(65.0)).h(px(30.0)).px(px(6.0)).rounded(px(4.0)).border_1().border_color(color(&t.border)).child(self.limit_input.clone()))
                    .child(tr("菜单显示数量", "Folders in menu"))
                    .child(div().w(px(65.0)).h(px(30.0)).px(px(6.0)).rounded(px(4.0)).border_1().border_color(color(&t.border)).child(self.menu_input.clone())))
                .child(div().flex().flex_wrap().items_center().gap(px(6.0)).text_size(px(11.0)).child(tr("运行独立重开动作时", "When running Reopen"))
                    .child(self.button("reopen-launch-direct", tr("直接重开最近目录", "Reopen latest directly")).text_color(color(if !show_menu { &t.accent } else { &t.text_muted })).on_click(cx.listener(|this, _, _, cx| this.mutate(json!({"action":"settings","showMenu":false}), cx))))
                    .child(self.button("reopen-launch-window", tr("显示快捷重开窗", "Show quick window")).text_color(color(if show_menu { &t.accent } else { &t.text_muted })).on_click(cx.listener(|this, _, _, cx| this.mutate(json!({"action":"settings","showMenu":true}), cx)))))
                .child(div().text_size(px(10.0)).text_color(color(&t.text_muted)).child(tr("按 Enter 或离开输入框保存；调小上限会在下次新增时整理。", "Press Enter or leave a field to save. Lower limits apply on the next record.")));
            body = body.child(controls);
        }
        if self.more {
            body = body.child(div().absolute().right(px(16.0)).top(px(52.0)).w(px(196.0)).p(px(6.0)).rounded(px(6.0)).border_1().border_color(color(&t.border)).bg(color(&t.surface)).shadow_lg()
                .child(self.button("reopen-clear-all", tr("清空全部历史…", "Clear all history…")).w_full().border_0().on_click(cx.listener(|this, _, _, cx| {
                    if !this.entries().is_empty() { this.confirm = Some(Confirm::Clear { entries: this.entries() }); this.dialog_selection = 0; }
                    this.more = false; cx.notify();
                }))));
        }
        if self.context.is_some() { body = body.child(self.render_context(cx)); }
        if self.confirm.is_some() { body = body.child(self.render_confirm(cx)); }
        body.when(self.busy || self.leave_locked, |body| body.child(div().absolute().inset_0().bg(gpui::rgba(0x00000010))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_exclusion_preview_respects_boundaries_and_virtual_positions() {
        assert!(matches_path(r"C:\Work\Docs", r"c:\work", true));
        assert!(!matches_path(r"C:\Work-other", r"c:\work", true));
        assert!(!matches_path(r"C:\Work\Docs", r"c:\work", false));
        assert!(matches_path(r"C:\Work\", r"c:\work", false));
        assert!(!matches_path("shell:::a\\b", "shell:::a", true));
    }
}
