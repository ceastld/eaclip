//! Data management, offline help and LAN pages. SettingsWindow embeds one
//! instance as the unified main window; the standalone window is retained.
//! All mutations use HostState;
//! selecting a data directory delegates confirmation/restart to dataLocation.
//! This module never probes a directory independently or opens another store.

use gpui_rsx::rsx;
use std::{sync::Arc, time::Duration};

use cliphost_core::{host::HostState, i18n::tr};
use gpui::{
    div, prelude::*, px, size, AnyElement, App, Bounds, ClipboardItem, Context, Entity,
    FontWeight, Image, ImageFormat, MouseButton, SharedString, Subscription, Task, Timer, Window,
    WindowBounds, WindowHandle, WindowOptions,
};
use crate::crisp_svg::svg;
use crate::smooth_wheel::SmoothWheelExt;
use serde_json::{json, Value};

use crate::{
    input::{InputColors, SearchEvent, SearchInput},
    preview::{DesignTokens, ThemeTokens},
    settings::SettingsForm,
    view::color,
    window_background::{PreparedBackground, WindowBackgroundOptions},
};

#[path = "management_lan.rs"]
mod lan;
#[cfg(debug_assertions)]
#[path = "management_lan_fixture.rs"]
pub mod lan_fixture;
pub use lan::{HistorySendFailure, SharedHistorySend};
pub(crate) use lan::{LanManagementEvent, LanManagementPage};
#[path = "management_help.rs"]
mod help;

/// Root gates new LAN actions before awaiting this on normal exit/restart.
pub async fn flush_management_lan_writes() -> Result<(), String> {
    lan::flush_writes().await
}

const USAGE: &str = include_str!("../../../../frontend/src/docs/clipboard-usage.mdx");
const COMMANDS: &str = include_str!("../../../../frontend/src/docs/commandsCatalogData.ts");
const ALIPAY: &[u8] = include_bytes!("../../../../packaging/about/alipay.png");
const WEPAY: &[u8] = include_bytes!("../../../../packaging/about/wepay.png");

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn close_main(host: &HostState) {
    if host.close_behavior() == "ExitApplication" {
        host.chrome_web_action(&json!({"kind": "mainHostCommand", "command": "exit"}));
    } else {
        host.dismiss_native_window("main");
    }
}

pub(crate) fn data_settings_route(route: &str) -> Option<&'static str> {
    let path = route.split(['?', '#']).next().unwrap_or(route).trim_end_matches('/');
    matches!(path, "/main/data" | "data" | "dataLocation" | "setting dataLocation"
        | "/main/settings/dataLocation" | "/settings/dataLocation")
        .then_some("/main/settings/dataLocation")
}

fn page_from_route(route: &str) -> &'static str {
    match route
        .split(['?', '#'])
        .next()
        .unwrap_or(route)
        .trim_end_matches('/')
    {
        path if data_settings_route(path).is_some() => "data",
        "/main/docs" | "docs" | "help" => "docs",
        "/main/lan" | "lan" => "lan",
        _ => "home",
    }
}

/// The existing frontend catalogue is data-only TypeScript. Quote object keys
/// outside strings to load its exact descriptions/examples without a second
/// copied catalogue or a production JavaScript runtime.
fn command_catalog() -> Result<Vec<Value>, String> {
    let source = COMMANDS
        .split_once("export const commandDocCatalogItems:")
        .and_then(|(_, source)| source.split_once("= ["))
        .map(|(_, source)| format!("[{source}"))
        .ok_or("Command catalogue declaration is missing.")?;
    let source = source.trim().trim_end_matches(';');
    let bytes = source.as_bytes();
    let mut output = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            output.push_str(&source[start..i.min(bytes.len())]);
        } else if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &source[start..i];
            let next = source[i..].trim_start();
            if next.starts_with(':') {
                output.push('"');
                output.push_str(word);
                output.push('"');
            } else {
                output.push_str(word);
            }
        } else {
            if bytes[i] != b','
                || !matches!(
                    source[i + 1..].trim_start().as_bytes().first(),
                    Some(b']' | b'}')
                )
            {
                output.push(bytes[i] as char);
            }
            i += 1;
        }
    }
    serde_json::from_str(&output).map_err(|error| error.to_string())
}

fn usage_lines() -> Vec<(u8, String)> {
    let mut importing = false;
    USAGE
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("import ") {
                importing = true;
            }
            if importing {
                if line.ends_with(';') {
                    importing = false;
                }
                return None;
            }
            if line.is_empty() || line.starts_with('<') || line.starts_with("| ---") {
                return None;
            }
            let heading = line.chars().take_while(|ch| *ch == '#').count() as u8;
            Some((
                heading,
                line.trim_start_matches('#')
                    .trim()
                    .replace("**", "")
                    .replace('`', ""),
            ))
        })
        .collect()
}

pub struct ManagementPage {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    page: String,
    embedded_data: bool,
    data_leave_locked: bool,
    data_save_error: Option<String>,
    data_clear_error: Option<String>,
    generation: u64,
    draft_revision: u64,
    paths: Value,
    info: Value,
    status: Value,
    pending_clear: Value,
    loading: bool,
    saving: bool,
    picking: bool,
    clear_confirm: Option<bool>,
    reset_confirm: bool,
    clearing: bool,
    message: Option<(bool, String)>,
    folder: Entity<SearchInput>,
    command_search: Entity<SearchInput>,
    query: String,
    catalogue: Result<Vec<Value>, String>,
    help: help::HelpState,
    system_form: Entity<SettingsForm>,
    lan: Entity<LanManagementPage>,
    alipay: Arc<Image>,
    wepay: Arc<Image>,
    _subscriptions: Vec<Subscription>,
    _state_watch: Task<()>,
}

impl ManagementPage {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        route: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t = tokens.theme(dark);
        let colors = InputColors {
            background: color(&t.search_background),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: color(&t.border_secondary),
            selection: color(&t.tab_active),
            caret: color(&t.accent),
        };
        let folder = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_metrics(32., 13., 8., cx);
            input.set_colors(colors, cx);
            input.set_placeholder(
                tr("留空使用内置默认", "Leave blank for the built-in location"),
                cx,
            );
            input.set_double_click_clears(false);
            input
        });
        let search = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_metrics(32., 13., 8., cx);
            input.set_colors(colors, cx);
            input.set_placeholder(
                tr(
                    "搜索命令、名称或说明…",
                    "Search commands, names or descriptions…",
                ),
                cx,
            );
            input.set_double_click_clears(false);
            input
        });
        let folder_sub = cx.subscribe_in(&folder, window, |this, _, event, _, cx| {
            if matches!(event, SearchEvent::Changed(_)) {
                this.draft_revision += 1;
                cx.notify();
            }
        });
        let search_sub = cx.subscribe_in(&search, window, |this, input, event, _, cx| {
            if matches!(event, SearchEvent::Changed(_)) {
                this.query = input.read(cx).text().to_lowercase();
                cx.notify();
            }
        });
        let form_host = host.clone();
        let form_tokens = tokens.clone();
        let system_form = cx.new(|cx| {
            let mut form = SettingsForm::new(form_host, form_tokens, dark, None, window, cx);
            form.load("homeGlobal", window, cx);
            form
        });
        let lan_host = host.clone();
        let lan_tokens = tokens.clone();
        let lan = cx.new(|cx| {
            LanManagementPage::new(
                lan_host,
                lan_tokens,
                dark,
                page_from_route(route) == "lan",
                window,
                cx,
            )
        });
        let lan_sub = cx.subscribe(&lan, |this, _, event, cx| {
            match event {
                LanManagementEvent::Error(error) => this.message = Some((true, error.clone())),
                LanManagementEvent::CloseRequested | LanManagementEvent::MoveRequested => {},
            }
            cx.notify();
        });
        let state_watch = cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if this
                .update_in(cx, |this, _, cx| {
                    let status = this.host.main_host_state_dto();
                    let pending = this.host.clear_pending();
                    if status != this.status || pending != this.pending_clear {
                        this.status = status;
                        this.pending_clear = pending;
                        cx.notify();
                    } else if !pending.is_null() {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        });
        let mut page = Self {
            host,
            tokens,
            dark,
            page: page_from_route(route).into(),
            embedded_data: false,
            data_leave_locked: false,
            data_save_error: None,
            data_clear_error: None,
            generation: 0,
            draft_revision: 0,
            paths: Value::Null,
            info: Value::Null,
            status: Value::Null,
            pending_clear: Value::Null,
            loading: false,
            saving: false,
            picking: false,
            clear_confirm: None,
            reset_confirm: false,
            clearing: false,
            message: None,
            folder,
            command_search: search,
            query: String::new(),
            catalogue: command_catalog(),
            help: help::HelpState::new(cx),
            system_form,
            lan,
            alipay: Arc::new(Image::from_bytes(ImageFormat::Png, ALIPAY.to_vec())),
            wepay: Arc::new(Image::from_bytes(ImageFormat::Png, WEPAY.to_vec())),
            _subscriptions: vec![folder_sub, search_sub, lan_sub],
            _state_watch: state_watch,
        };
        page.refresh(window, cx);
        page
    }

    /// Retain this entity in SettingsWindow. It owns the existing explicit-save
    /// data actions; the parent supplies layout, scrolling, search and wallpaper.
    pub fn new_embedded_data(
        host: Arc<HostState>, tokens: DesignTokens, dark: bool,
        window: &mut Window, cx: &mut Context<Self>,
    ) -> Self {
        let mut page = Self::new(host, tokens, dark, "dataLocation", window, cx);
        page.embedded_data = true;
        page.lan.update(cx, |lan, cx| lan.set_embedded(true, cx));
        page
    }

    pub fn is_composing(&self, cx: &App) -> bool { self.folder.read(cx).is_composing() }

    pub fn pending_data_writes(&self) -> bool { self.saving || self.clearing }

    pub fn data_persistence_error(&self) -> Option<String> {
        let errors: Vec<_> = [&self.data_save_error, &self.data_clear_error]
            .into_iter().flatten().map(String::as_str).collect();
        (!errors.is_empty()).then(|| errors.join("\n"))
    }

    pub fn projection_busy(&self, cx: &App) -> bool {
        self.is_composing(cx) || self.picking || self.pending_data_writes()
    }

    pub fn preflight_data_leave(&self, cx: &App) -> Result<(), String> {
        if self.is_composing(cx) {
            Err(tr("请先完成数据路径的输入法组合。", "Finish composing the data path first.").into())
        } else if self.picking {
            Err(tr("请先完成数据文件夹选择。", "Finish choosing the data folder before leaving.").into())
        } else { Ok(()) }
    }

    pub fn set_data_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if self.data_leave_locked != locked {
            self.data_leave_locked = locked;
            self.folder.update(cx, |input, cx| input.set_read_only(locked, cx));
            cx.notify();
        }
    }

    pub fn data_focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.folder.read(cx).focus_handle()
    }

    /// The unified main window gates new LAN actions during exit like the
    /// former management window did.
    pub(crate) fn set_lan_exit_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.lan.update(cx, |lan, cx| lan.set_exit_pending(pending, cx));
    }

    pub fn set_route(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        let next = page_from_route(route);
        if next == "data" && !self.embedded_data {
            self.host.show_web_route("/main/settings/dataLocation");
            return;
        }
        if self.page == next {
            return;
        }
        window.blur();
        if next == "docs" {
            self.help = help::HelpState::new(cx);
            self.query.clear();
            self.command_search
                .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        }
        self.page = next.into();
        self.lan
            .update(cx, |lan, cx| lan.set_active(next == "lan", window, cx));
        self.message = None;
        self.clear_confirm = None;
        self.reset_confirm = false;
        self.refresh(window, cx);
        cx.notify();
    }

    fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.set_design_tokens(self.tokens.clone(), dark, cx);
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        let changed = self.dark != dark || self.tokens.theme(dark) != tokens.theme(dark);
        self.tokens = tokens;
        self.dark = dark;
        if !changed {
            return;
        }
        let t = self.tokens.theme(dark);
        let colors = InputColors {
            background: color(&t.search_background),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: color(&t.border_secondary),
            selection: color(&t.tab_active),
            caret: color(&t.accent),
        };
        for input in [&self.folder, &self.command_search] {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        self.system_form.update(cx, |form, cx| {
            form.set_design_tokens(self.tokens.clone(), dark, cx)
        });
        self.lan.update(cx, |lan, cx| {
            lan.set_design_tokens(self.tokens.clone(), dark, cx)
        });
        cx.notify();
    }

    pub(crate) fn handle_lan_event(
        &mut self,
        name: &str,
        data: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lan
            .update(cx, |lan, cx| lan.on_event(name, data, window, cx));
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let revision = self.draft_revision;
        let host = self.host.clone();
        self.loading = true;
        cx.spawn_in(window, async move |this, cx| {
            let (paths, info) = cx
                .background_executor()
                .spawn(async move { (host.data_paths(), host.app_info()) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                if this.draft_revision == revision && !this.is_composing(cx) {
                    let path = string(&paths, "sidecarOverridePath");
                    this.folder
                        .update(cx, |input, cx| input.set_text(path, window, cx));
                }
                this.paths = paths;
                this.info = info;
                this.loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_leave_locked || self.is_composing(cx)
            || self.picking || self.saving || self.loading || self.paths["envOverrideActive"] == true {
            return;
        }
        let owner = match crate::platform::hwnd(window) {
            Ok(hwnd) => hwnd.0 as isize,
            Err(error) => {
                self.message = Some((true, error));
                cx.notify();
                return;
            }
        };
        let request = match self.host.capture_native_picker_window(owner) {
            Ok(request) => request,
            Err(error) => {
                self.message = Some((true, error));
                cx.notify();
                return;
            }
        };
        self.picking = true;
        self.message = None;
        let generation = self.generation;
        let revision = self.draft_revision;
        let typed = self.folder.read(cx).text().trim().to_string();
        let initial = if typed.is_empty() {
            string(&self.paths, "dataRoot")
        } else {
            typed
        };
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker_request = request.clone();
            let result = crate::native_dialog::run(move || {
                host.pick_folder_native_window_guarded(
                    &json!({"initialPath": initial}), &worker_request, || true,
                )
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.picking = false;
                if this.generation != generation
                    || !this.host.native_picker_window_is_current(&request)
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(result)
                        if result["cancelled"] != true && this.draft_revision == revision
                            && !this.is_composing(cx) =>
                    {
                        if let Some(path) = result["path"].as_str().filter(|path| !path.is_empty())
                        {
                            this.folder.update(cx, |input, cx| {
                                input.set_text(path.to_string(), window, cx)
                            });
                            this.draft_revision += 1;
                        }
                    }
                    Err(error) => this.message = Some((true, error)),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_location(&mut self, values: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_leave_locked || self.is_composing(cx)
            || self.saving || self.loading || self.picking || self.paths["envOverrideActive"] == true {
            return;
        }
        self.saving = true;
        self.data_save_error = None;
        self.message = None;
        self.reset_confirm = false;
        let generation = self.generation;
        let revision = self.draft_revision;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = host.settings_form_update(
                        &json!({"configId": "dataLocation", "values": values}),
                    );
                    let paths = result.as_ref().ok().map(|_| host.data_paths());
                    (result, paths)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                if this.generation != generation {
                    return;
                }
                match result {
                    (Ok(form), Some(paths)) => {
                        this.data_save_error = None;
                        let messages: Vec<_> = form["messages"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|message| message["text"].as_str())
                            .collect();
                        this.message = Some((false, messages.join("\n")));
                        if this.draft_revision == revision && !this.is_composing(cx) {
                            let path = string(&paths, "sidecarOverridePath");
                            this.folder
                                .update(cx, |input, cx| input.set_text(path, window, cx));
                        }
                        this.paths = paths;
                    }
                    (Err(error), _) => {
                        this.data_save_error = Some(error.clone());
                        this.message = Some((true, error));
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn clear(&mut self, undo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_leave_locked || self.is_composing(cx) || self.clearing {
            return;
        }
        if !undo && self.clear_confirm.is_none() {
            return;
        }
        self.clearing = true;
        self.data_clear_error = None;
        self.message = None;
        let non_top = self.clear_confirm.unwrap_or(true);
        let id = string(&self.pending_clear, "clearId");
        let host = self.host.clone();
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if undo {
                        host.clear_cancel(&json!({"clearId": id}))
                    } else {
                        host.clear_all(non_top)
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.clearing = false;
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(result) => {
                        this.data_clear_error = None;
                        this.clear_confirm = None;
                        this.pending_clear = this.host.clear_pending();
                        if undo {
                            this.message =
                                Some((false, tr("已撤销清空。", "Clear canceled.").into()));
                        } else if result["itemCount"] == 0 {
                            this.message = Some((
                                false,
                                tr("没有可清空的记录。", "No records to clear.").into(),
                            ));
                        }
                    }
                    Err(error) => {
                        this.data_clear_error = Some(error.clone());
                        this.message = Some((true, error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn command(&mut self, body: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_leave_locked { return; }
        let host = self.host.clone();
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.chrome_web_action(&body) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation {
                    return;
                }
                if result["ok"] == false {
                    this.message = Some((true, string(&result, "error")));
                }
                this.status = this.host.main_host_state_dto();
                cx.notify();
            });
        })
        .detach();
    }

    fn open_target(
        &mut self,
        target: String,
        uri: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.data_leave_locked || target.is_empty() {
            return;
        }
        let host = self.host.clone();
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if uri {
                        host.shell_open_uri(&json!({"uri": target}))
                    } else {
                        host.shell_open_path(&json!({"path": target}))
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation == generation {
                    if let Err(error) = result {
                        this.message = Some((true, error));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn copy_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_leave_locked { return; }
        let host = self.host.clone();
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.latest_log() })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(log) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(string(&log, "text")));
                        this.message = Some((
                            false,
                            tr(
                                "已复制最新日志到剪贴板",
                                "Latest log copied to the clipboard.",
                            )
                            .into(),
                        ));
                    }
                    Err(error) => this.message = Some((true, error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        enabled: bool,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let enabled = enabled && !self.data_leave_locked;
        let hover = color(&t.hover);
        let id: SharedString = id.into();
        let label: SharedString = label.into();
        rsx! {
            <div id={id} px={px(10.)} py={px(5.)} rounded={px(5.)} border_1
                border_color={color(&t.border_secondary)}>
                {label}
            </div>
        }
            .when(enabled, |button| {
                button.cursor_pointer().hover(move |style| style.bg(hover))
            })
            .when(!enabled, |button| button.opacity(0.4))
    }

    fn action_button(
        &self,
        id: &str,
        label: &'static str,
        command: &str,
        t: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let command = command.to_string();
        self.button(id.to_string(), label, true, t)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.command(
                    json!({"kind": "mainHostCommand", "command": command}),
                    window,
                    cx,
                );
            }))
            .into_any_element()
    }

    fn data_view(&self, t: &ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let locked = self.paths["envOverrideActive"] == true;
        let busy = self.data_leave_locked || self.loading || self.saving || self.picking;
        let editable = !locked && !busy;
        let current = string(&self.paths, "dataRoot");
        let legacy = string(&self.paths, "legacyDataRoot");
        let legacy_error = string(&self.paths, "legacyDataError");
        let active = self.paths["legacyDataRootActive"] == true;
        let mut page = rsx! {
            <div w_full min_w_0 flex_none flex flex_col gap={px(10.)}>
                <div font_weight={FontWeight::SEMIBOLD}>
                    {tr("数据位置", "Data location")}
                </div>
            </div>
        }
            .child(div().text_color(color(&t.text_muted)).child(if locked {
                tr("环境变量覆盖", "Environment override")
            } else if string(&self.paths, "sidecarOverridePath").is_empty() {
                tr("内置默认", "Built-in default")
            } else {
                tr("自定义", "Custom")
            }))
            .child(div().child(format!("{} {current}", tr("当前生效：", "Current: "))));
        if locked {
            page = page.child(tr(
                "环境变量已覆盖路径，无法在界面修改。",
                "The path is controlled by an environment variable.",
            ));
        } else {
            page = page.child(self.folder.clone()).child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(
                        self.button(
                            "pick-data",
                            if self.picking {
                                tr("选择中…", "Choosing…")
                            } else {
                                tr("浏览", "Browse")
                            },
                            editable,
                            t,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx))),
                    )
                    .child(
                        self.button(
                            "save-data",
                            if self.saving {
                                tr("保存中…", "Saving…")
                            } else {
                                tr("保存", "Save")
                            },
                            editable,
                            t,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if !this.folder.read(cx).is_composing() {
                                let path = this.folder.read(cx).text().trim().to_string();
                                this.save_location(json!({"dataFolder": path}), window, cx);
                            }
                        })),
                    )
                    .child(
                        self.button("reset-data", tr("恢复默认", "Restore default"), editable, t)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.data_leave_locked && !this.saving
                                    && !this.picking
                                    && this.paths["envOverrideActive"] != true
                                {
                                    this.reset_confirm = true;
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        page = page.child(div().text_color(color(&t.text_muted)).child(tr("保存后可能需重启，不会自动迁移已有数据。", "A restart may be required. Existing data is kept in its current folder.")))
            .child(div().flex().flex_wrap().gap(px(8.)).children([("dataRoot", tr("打开数据", "Open data")), ("logFolder", tr("日志", "Logs")), ("sidecarDirectory", tr("配置", "Configuration"))].into_iter().map(|(key, label)| {
                let path = string(&self.paths, key); let enabled = !path.is_empty();
                self.button(format!("open-{key}"), label, enabled, t).on_click(cx.listener(move |this, _, window, cx| this.open_target(path.clone(), false, window, cx)))
            })).child(self.action_button("latest-log", tr("最新日志", "Latest log"), "openLog", t, cx))
                .child(self.button("copy-log", tr("复制日志", "Copy log"), true, t).on_click(cx.listener(|this, _, window, cx| this.copy_log(window, cx)))))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(tr("旧版数据", "Legacy data")))
            .child(self.button("use-legacy-data", if active { tr("正在使用", "In use") } else { tr("使用旧数据目录", "Use legacy data directory") }, editable && !legacy.is_empty() && !active, t)
                .on_click(cx.listener(|this, _, window, cx| { if !string(&this.paths, "legacyDataRoot").is_empty() && this.paths["legacyDataRootActive"] != true { this.save_location(json!({"useLegacyDataRoot": true}), window, cx); } })))
            .child(div().text_color(color(if legacy_error.is_empty() { &t.text_secondary } else { &t.error })).child(if !legacy_error.is_empty() { legacy_error } else if legacy.is_empty() { tr("未检测到旧版数据库。", "No legacy database detected.").into() } else { legacy }))
            .child(div().text_color(color(&t.text_muted)).child(if active { tr("历史、收藏和图片直接使用此目录。", "History, favorites and images use this directory directly.") } else { tr("切换后重启，直接使用原有历史、收藏和图片。当前目录的数据会保留。", "Restart to use the existing history, favorites and images directly. The current directory is retained.") }));
        if self.reset_confirm {
            page = page.child(rsx! {
                <div p={px(10.)} border_1 border_color={color(&t.border)} flex flex_col
                    gap={px(8.)}>
                    {tr("恢复内置默认数据位置？保存后需重启，不会自动迁移已有数据。", "Restore the built-in data location? Restart is required; existing data stays in place.")}
                </div>
            }
                .child(div().flex().gap(px(8.))
                    .child(self.button("cancel-reset", tr("取消", "Cancel"), !busy, t).on_click(cx.listener(|this, _, _, cx| { this.reset_confirm = false; cx.notify(); })))
                    .child(self.button("confirm-reset", tr("恢复默认", "Restore default"), editable, t).on_click(cx.listener(|this, _, window, cx| this.save_location(json!({"dataFolder": ""}), window, cx))))));
        }
        page = page
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("剪贴板维护", "Clipboard maintenance")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .children(
                        [
                            (true, tr("清空非置顶", "Clear unpinned")),
                            (false, tr("清空全部", "Clear all")),
                        ]
                        .into_iter()
                        .map(|(non_top, label)| {
                            self.button(
                                format!("clear:{non_top}"),
                                label,
                                !self.clearing && self.pending_clear.is_null(),
                                t,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if !this.data_leave_locked && !this.clearing && this.pending_clear.is_null() {
                                        this.clear_confirm = Some(non_top);
                                        cx.notify();
                                    }
                                },
                            ))
                        }),
                    )
                    .child(self.action_button(
                        "restart-data",
                        tr("重启程序", "Restart"),
                        "restart",
                        t,
                        cx,
                    )),
            );
        if let Some(non_top) = self.clear_confirm {
            page = page.child(rsx! {
                <div p={px(10.)} border_1 border_color={color(&t.error)} flex flex_col
                    gap={px(8.)} />
            }
                .child(if non_top { tr("将立即隐藏所有非置顶记录；约 5 秒内可撤销，超时后永久删除。", "Hide all unpinned records now; undo within about 5 seconds before permanent deletion.") } else { tr("将立即隐藏全部记录（含置顶）；约 5 秒内可撤销，超时后永久删除。", "Hide all records including pinned records now; undo within about 5 seconds before permanent deletion.") })
                .child(div().flex().gap(px(8.))
                    .child(self.button("cancel-clear", tr("取消", "Cancel"), !self.clearing, t).on_click(cx.listener(|this, _, _, cx| { if !this.clearing { this.clear_confirm = None; cx.notify(); } })))
                    .child(self.button("confirm-clear", tr("确认清空", "Confirm clear"), !self.clearing, t).on_click(cx.listener(|this, _, window, cx| this.clear(false, window, cx))))));
        }
        if !self.pending_clear.is_null() {
            let deadline = self.pending_clear["expiresAtUnixMs"].as_i64().unwrap_or(0);
            let remaining =
                ((deadline - chrono::Utc::now().timestamp_millis()).max(0) + 999) / 1000;
            page = page.child(
                rsx! {
                    <div flex items_center gap={px(8.)}>
                        {format!(
                        "{} {} · {}s",
                        tr("待清空：", "Pending clear:"),
                        self.pending_clear["itemCount"],
                        remaining
                    )}
                    </div>
                }
                    .child(
                        self.button("undo-clear", tr("撤销", "Undo"), !self.clearing, t)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.clear(true, window, cx)),
                            ),
                    ),
            );
        }
        page.into_any_element()
    }

    fn home_view(&self, t: &ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let mut page = rsx! {
            <div flex flex_col gap={px(12.)}>
                <div font_weight={FontWeight::SEMIBOLD}>
                    {tr("首页", "Home")}
                </div>
                {tr("快捷打开剪贴板、超级面板与各类超级菜单", "Open clipboard history, panels and Super Menu categories")}
            </div>
        }
            .child(rsx! {
                <div flex flex_wrap gap={px(8.)}>
                    {self.action_button("home-clip", tr("剪贴板", "Clipboard"), "openClipboard", t, cx)}
                    {self.action_button("home-panel", tr("超级面板", "Panel"), "openPanel", t, cx)}
                </div>
            }
                .children([("super", tr("超级菜单", "Super Menu")), ("bookmarks", tr("书签", "Bookmarks")), ("file", tr("文件", "Files")), ("window", tr("窗口", "Windows")), ("text", tr("文本", "Text")), ("favor", tr("收藏", "Favorites")), ("clipboard", tr("剪贴板菜单", "Clipboard menu"))].into_iter().map(|(menu, label)| {
                    self.button(format!("home-menu:{menu}"), label, true, t).on_click(cx.listener(move |this, _, window, cx| this.command(json!({"kind": "mainHostCommand", "command": "openMenu", "menu": menu}), window, cx)))
                })))
            .child(div().child(format!("Quicker RPC: {} · Verb RPC: {}", if self.status["quickerRpcConnected"] == true { tr("已连接", "Connected") } else { tr("未连接", "Disconnected") }, if self.status["verbRpcListenerReady"] == true { tr("已就绪", "Ready") } else { tr("未就绪", "Not ready") })));
        let show_tray = self.status["showTrayIcon"] != false;
        let background = self.status["closeBehavior"] != "ExitApplication";
        page = page.child(div().font_weight(FontWeight::SEMIBOLD).child(tr("窗口行为", "Window behavior")))
            .child(self.button("toggle-tray", format!("{} {}", if show_tray { "☑" } else { "☐" }, tr("显示托盘图标", "Show tray icon")), true, t)
                .on_click(cx.listener(move |this, _, window, cx| this.command(json!({"kind": "mainHostCommand", "command": "setShowTrayIcon", "show": !show_tray}), window, cx))))
            .child(div().flex().flex_wrap().gap(px(8.)).children([("minimizeToTray", if show_tray { tr("缩小到托盘", "Minimize to tray") } else { tr("关闭后在后台运行", "Keep running in the background") }), ("exitApplication", tr("退出程序", "Exit application"))].into_iter().map(|(behavior, label)| {
                self.button(format!("close:{behavior}"), label, true, t).when(background == (behavior == "minimizeToTray"), |button| button.bg(color(&t.tab_active)))
                    .on_click(cx.listener(move |this, _, window, cx| this.command(json!({"kind": "mainHostCommand", "command": "setCloseBehavior", "behavior": behavior}), window, cx)))
            }))).child(rsx! {
                <div flex flex_wrap gap={px(8.)}>
                    {self.action_button("home-log", tr("查看日志", "View log"), "openLog", t, cx)}
                </div>
            }
                .child(self.button("home-copy-log", tr("复制日志", "Copy log"), true, t).on_click(cx.listener(|this, _, window, cx| this.copy_log(window, cx))))
                .child(self.action_button("home-restart", tr("重启", "Restart"), "restart", t, cx))
                .child(self.action_button("home-exit", tr("退出", "Exit"), "exit", t, cx)))
            .child(div().h(px(440.)).child(self.system_form.clone()));
        page.into_any_element()
    }

    fn docs_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        help::render(self, window, cx)
    }
}

impl Render for ManagementPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        let content = match self.page.as_str() {
            "data" => self.data_view(&t, cx),
            "docs" => self.docs_view(window, cx),
            "lan" => self.lan.clone().into_any_element(),
            _ => self.home_view(&t, cx),
        };
        // The LAN workspace owns its caption and navigation, matching the
        // confirmed design stage. Other management pages keep their chrome.
        if self.page == "lan" {
            return rsx! {
                <div size_full text_size={px(13.)} text_color={color(&t.text)}
                    bg={color(&t.background)}>
                    {content}
                </div>
            };
        }
        // Help fills the unified main window's content area beside its own
        // contents list; the window supplies caption, navigation and search.
        if self.embedded_data && self.page == "docs" {
            return rsx! {
                <div size_full flex flex_col text_size={px(13.)} text_color={color(&t.text)} />
            }
                .children(self.message.as_ref().map(|(error, message)| {
                    div().px(px(14.)).py(px(8.)).text_color(color(if *error { &t.error } else { &t.accent }))
                        .child(message.clone())
                }))
                .child(div().id("management-content").flex_1().min_h_0().child(content));
        }
        if self.embedded_data {
            let mut page = rsx! {
                <div w_full min_w_0 flex_none flex flex_col gap={px(10.)} text_size={px(13.)}
                    text_color={color(&t.text)} />
            };
            if let Some((error, message)) = &self.message {
                page = page.child(div().text_color(color(if *error { &t.error } else { &t.accent }))
                    .child(message.clone()));
            }
            return page.child(content);
        }
        let mut page = rsx! {
            <div size_full flex flex_col text_size={px(13.)} text_color={color(&t.text)}
                bg={color(&t.background)} />
        }
            .child(
                rsx! {
                    <div flex flex_wrap gap={px(8.)} p={px(12.)} border_b_1
                        border_color={color(&t.border)} />
                }
                    .children(
                        [
                            ("home", tr("首页", "Home")),
                            ("data", tr("数据管理", "Data management")),
                            ("docs", tr("帮助", "Help")),
                            ("lan", tr("局域网", "LAN")),
                        ]
                        .into_iter()
                        .map(|(id, label)| {
                            self.button(format!("management:{id}"), label, true, &t)
                                .when(self.page == id, |button| button.bg(color(&t.tab_active)))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_route(id, window, cx)
                                }))
                        }),
                    )
                    .child(
                        self.button("management-settings", tr("设置", "Settings"), true, &t)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let host = this.host.clone();
                                cx.spawn_in(window, async move |_, cx| {
                                    let _ = cx
                                        .background_executor()
                                        .spawn(async move { host.execute_verb("setting global") })
                                        .await;
                                })
                                .detach();
                            })),
                    ),
            );
        if let Some((error, message)) = self.message.as_ref().filter(|_| self.page != "lan") {
            page = page.child(
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .text_color(color(if *error { &t.error } else { &t.accent }))
                    .child(message.clone()),
            );
        }
        page.child(
            div()
                .id("management-content")
                .flex_1()
                .min_h_0()
                .when(self.page != "lan" && self.page != "docs", |content| {
                    content
                        .overflow_y_scroll()
                        .smooth_scroll("management-content", cx)
                        .p(px(14.))
                })
                .child(content),
        )
    }
}

pub struct ManagementWindow {
    page: Entity<ManagementPage>,
    host: Arc<HostState>,
    base_tokens: DesignTokens,
    tokens: DesignTokens,
    dark: bool,
    window_background: Option<Arc<PreparedBackground>>,
    window_background_options: WindowBackgroundOptions,
}

impl ManagementWindow {
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        route: &str,
        cx: &mut App,
    ) -> Result<(), String> {
        if let Some(route) = data_settings_route(route) {
            host.show_web_route(route);
            return Ok(());
        }
        if let Some(handle) = existing {
            if let Ok(result) = handle.update(cx, |this, window, cx| {
                this.set_design_tokens(tokens.clone(), dark, cx);
                this.page
                    .update(cx, |page, cx| page.set_route(route, window, cx));
                crate::platform::present_main(window, &this.host)
            }) {
                return result;
            }
        }
        let s = &tokens.settings;
        let is_lan = page_from_route(route) == "lan";
        let bounds = Bounds::centered(None, if is_lan { size(px(900.), px(640.)) }
            else { size(px(s.width), px(s.height)) }, cx);
        let minimum = if is_lan { size(px(560.), px(420.)) }
            else { size(px(s.min_width), px(s.min_height)) };
        let route = route.to_string();
        let close_host = host.clone();
        let handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    focus: false,
                    show: true,
                    window_min_size: Some(minimum),
                    ..Default::default()
                },
                move |window, cx| {
                    window.set_window_title(tr("ClipHost 管理", "ClipHost Management"));
                    #[cfg(windows)]
                    if let Ok(hwnd) = crate::platform::hwnd(window) {
                        host.set_main_hwnd(hwnd.0 as isize);
                    }
                    if let Err(error) = crate::platform::register_probe_window(window) {
                        log::warn!("management probe registration failed: {error}");
                    }
                    let view = cx.new(|cx| {
                        let page_host = host.clone();
                        let page_tokens = tokens.clone();
                        Self {
                            page: cx.new(|cx| {
                                ManagementPage::new(
                                    page_host,
                                    page_tokens,
                                    dark,
                                    &route,
                                    window,
                                    cx,
                                )
                            }),
                            host,
                            base_tokens: tokens.clone(),
                            tokens,
                            dark,
                            window_background: None,
                            window_background_options: WindowBackgroundOptions::default(),
                        }
                    });
                    let retire_pickers = view
                        .read(cx)
                        .page
                        .read(cx)
                        .lan
                        .read(cx)
                        .picker_close_guard();
                    window.on_window_should_close(cx, move |_, _| {
                        retire_pickers();
                        close_main(&close_host);
                        true
                    });
                    view
                },
            )
            .map_err(|error| error.to_string())?;
        *existing = Some(handle);
        handle
            .update(cx, |this, window, _| crate::platform::present_main(window, &this.host))
            .map_err(|error| error.to_string())?
    }

    pub fn close(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| {
            this.page.read(cx).lan.read(cx).retire_pickers();
            window.remove_window();
        });
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.set_design_tokens(self.base_tokens.clone(), dark, cx);
    }

    /// Accept the resolved base palette, never the already translucent tokens.
    /// Update colors on the existing page and its input/form entities only.
    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.base_tokens = tokens;
        let tokens = if self.window_background.is_some() {
            crate::window_glass::tokens(&self.base_tokens, &self.window_background_options)
        } else {
            self.base_tokens.clone()
        };
        let changed = self.dark != dark || self.tokens.theme(dark) != tokens.theme(dark);
        self.tokens = tokens;
        self.dark = dark;
        let background_enabled = self.window_background.is_some();
        let background_options = self.window_background_options.clone();
        let base_tokens = self.base_tokens.clone();
        self.page.update(cx, |page, cx| {
            page.set_design_tokens(self.tokens.clone(), dark, cx);
            // Apply the settings-specific card/control palettes last. The
            // generic page palette must not overwrite their .88/.68 alpha.
            page.system_form.update(cx, |form, cx| {
                form.set_window_background_theme(
                    background_enabled,
                    background_options,
                    base_tokens,
                    dark,
                    cx,
                )
            });
        });
        if changed {
            cx.notify();
        }
    }

    /// Share the root's prepared resource and lease; do not start another read.
    /// Removing the wallpaper restores the unmodified resolved base palette.
    pub fn set_window_background(
        &mut self,
        snapshot: Option<Arc<PreparedBackground>>,
        options: WindowBackgroundOptions,
        base_tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        let same_image = match (&self.window_background, &snapshot) {
            (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
            (None, None) => true,
            _ => false,
        };
        let changed = !same_image || self.window_background_options != options;
        self.window_background = snapshot;
        self.window_background_options = options;
        self.set_design_tokens(base_tokens, dark, cx);
        if changed {
            cx.notify();
        }
    }

    pub fn set_exit_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.page.update(cx, |page, cx| {
            page.lan
                .update(cx, |lan, cx| lan.set_exit_pending(pending, cx))
        });
    }

    /// Forward the existing service broadcast; this window does not drain the
    /// core event bus or create another peer/listener maintenance owner.
    pub fn handle_lan_event(
        &mut self,
        name: &str,
        data: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.page
            .update(cx, |page, cx| page.handle_lan_event(name, data, window, cx));
    }
}

impl Render for ManagementWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_lan = self.page.read(cx).page == "lan";
        let t = self.tokens.theme(self.dark);
        let base = self.base_tokens.theme(self.dark);
        let l = &self.tokens.layout;
        let wallpaper = self.window_background.as_ref();
        let wallpaper_layer = wallpaper.map(|image| {
            crate::window_background::render_background_image(image, &self.window_background_options)
        });
        let wallpaper_tint = wallpaper.map(|_| {
            div()
                .absolute()
                .inset_0()
                .bg(crate::window_glass::page_tint(
                    base,
                    &self.window_background_options,
                ))
        });
        let (titlebar_background, titlebar_border) = if wallpaper.is_some() {
            crate::window_glass::titlebar_colors(base, &self.window_background_options)
        } else {
            (color(&t.surface), color(&t.border))
        };
        let hover = color(if wallpaper.is_some() {
            &t.tertiary
        } else {
            &t.hover
        });
        if is_lan {
            return rsx! {
                <div size_full relative flex flex_col bg={color(&base.background)}
                    font_family={self.tokens.typography.font_family.clone()} />
            }
                .children(wallpaper_layer).children(wallpaper_tint)
                .child(rsx! {
                    <div relative size_full>
                        {self.page.clone()}
                    </div>
                });
        }
        rsx! {
            <div size_full relative flex flex_col bg={color(&base.background)}
                font_family={self.tokens.typography.font_family.clone()} />
        }
            .children(wallpaper_layer)
            .children(wallpaper_tint)
            .child(
                rsx! {
                    <div h={px(l.title_bar_height)} flex_none flex items_center justify_between
                        px={px(l.title_bar_padding_x + 4.)} bg={titlebar_background} border_b_1
                        border_color={titlebar_border} />
                }
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, window, _| {
                            if event.click_count == 1 {
                                if let Err(error) =
                                    crate::platform::begin_host_move(window, &this.host, "main")
                                {
                                    log::warn!("management caption drag failed: {error}");
                                }
                            }
                        }),
                    )
                    .child(
                        rsx! {
                            <div text_size={px(12.)} text_color={color(&t.text_secondary)}>
                                {tr("ClipHost 管理", "ClipHost Management")}
                            </div>
                        },
                    )
                    .child(
                        rsx! {
                            <div id={"management-close"} size={px(l.chrome_button_size)} flex
                                items_center justify_center rounded={px(4.)} cursor_pointer />
                        }
                            .hover(move |style| style.bg(hover))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.page.read(cx).lan.read(cx).retire_pickers();
                                close_main(&this.host);
                                window.remove_window();
                            }))
                            .child(
                                svg()
                                    .path(
                                        self.tokens.icons.get("close").cloned().unwrap_or_default(),
                                    )
                                    .size(px(l.chrome_icon_size))
                                    .text_color(color(&t.text_secondary)),
                            ),
                    ),
            )
            .child(rsx! {
                <div flex_1 min_h_0>
                    {self.page.clone()}
                </div>
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_catalog_uses_existing_examples_and_preserves_windows_paths() {
        let commands = command_catalog().unwrap();
        assert_eq!(commands.len(), 8);
        let file = commands
            .iter()
            .find(|item| item["command"] == "menu")
            .unwrap();
        assert!(file.to_string().contains("menu file -f C:"));
        assert_eq!(
            file["exampleSections"][0]["rows"][5]["line"],
            "menu file -f C:\\a.txt;C:\\b.txt"
        );
    }

    #[test]
    fn management_routes_keep_canonical_main_destinations() {
        assert_eq!(page_from_route("/main/data?x=1"), "data");
        assert_eq!(page_from_route("/main/docs#about"), "docs");
        assert_eq!(page_from_route("/main/home"), "home");
        assert_eq!(page_from_route("/main/lan?x=1"), "lan");
        assert_eq!(page_from_route("/main/sync"), "home");
        assert_eq!(page_from_route("/main/sync/remote"), "home");
    }

    #[test]
    fn all_data_aliases_resolve_to_the_settings_category() {
        for route in ["/main/data", "/main/data/?x=1", "data", "dataLocation",
            "setting dataLocation", "/main/settings/dataLocation", "/settings/dataLocation#path"] {
            assert_eq!(data_settings_route(route), Some("/main/settings/dataLocation"), "{route}");
            assert_eq!(page_from_route(route), "data");
        }
        for route in ["/main/docs", "/main/home", "/main/lan", "/main/settings/clip", "/main/database"] {
            assert_eq!(data_settings_route(route), None, "{route}");
        }
    }

    #[test]
    fn offline_usage_does_not_render_mdx_imports_as_product_text() {
        let lines = usage_lines();
        assert!(lines
            .iter()
            .any(|(heading, line)| *heading == 1 && line == "剪贴板使用说明"));
        assert!(!lines
            .iter()
            .any(|(_, line)| line.contains("import ") || line.contains("<Note")));
    }
}
