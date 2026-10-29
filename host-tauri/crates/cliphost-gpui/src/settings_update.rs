//! An application-owned update session survives closing or changing settings.
//! Network, package verification and helper startup never run in a GPUI render.

use super::*;
use cliphost_updater::{PreparedUpdate, UpdateConfig, UpdatePhase, UpdateService, UpdateSnapshot};
use std::path::PathBuf;

#[path = "update_schedule.rs"]
mod schedule;

static UPDATE_RUNTIME: OnceLock<Arc<UpdateRuntime>> = OnceLock::new();

/// Owned by NativeHost's runtime, independent of whether settings is open.
pub(crate) async fn run_automatic_update_checks(host: Arc<HostState>) {
    let updater = OnceLock::<Arc<UpdateRuntime>>::new();
    schedule::run(
        || {
            if host.health()["windowRuntimeReady"] != true {
                return false;
            }
            let runtime = updater.get_or_init(|| UpdateRuntime::shared(&host));
            // Initialization is asynchronous. Do not consume the startup check
            // while its service is absent; an existing manual check can coalesce.
            runtime.state.lock().unwrap().service.is_some()
        },
        || {
            if let Some(runtime) = updater.get() {
                runtime.start_automatic_check(host.clone());
            }
        },
        schedule::CHECK_INTERVAL,
    )
    .await;
}

struct UpdateRuntime {
    state: Mutex<UpdateRuntimeState>,
    executable: PathBuf,
    data_root: PathBuf,
    api_port: u16,
}

struct UpdateRuntimeState {
    service: Option<Arc<UpdateService>>,
    busy: bool,
    error: Option<String>,
    prepared: Option<Arc<PreparedUpdate>>,
    awaiting_exit: bool,
}

#[derive(Clone, Copy)]
enum UpdateAction {
    Check,
    Download,
    Install,
}

impl UpdateRuntime {
    fn shared(host: &HostState) -> Arc<Self> {
        UPDATE_RUNTIME
            .get_or_init(|| {
                let runtime = Arc::new(Self {
                    state: Mutex::new(UpdateRuntimeState {
                        service: None,
                        busy: false,
                        error: None,
                        prepared: None,
                        awaiting_exit: false,
                    }),
                    executable: std::env::current_exe().unwrap_or_default(),
                    data_root: host.data_root(),
                    api_port: host.api_port(),
                });
                runtime.start_initialization();
                runtime
            })
            .clone()
    }

    fn start_initialization(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        if state.busy || state.service.is_some() {
            return;
        }
        state.busy = true;
        state.error = None;
        drop(state);
        let runtime = self.clone();
        std::thread::spawn(move || {
            let result = UpdateService::new(UpdateConfig {
                current_version: env!("CARGO_PKG_VERSION").to_string(),
                executable: runtime.executable.clone(),
                data_root: runtime.data_root.clone(),
                api_port: runtime.api_port,
                allow_install: !cfg!(debug_assertions)
                    && !cliphost_core::NATIVE_ACCEPTANCE_BUILD
                    && !cliphost_core::is_headless()
                    && std::env::var("CLIPHOST_GPUI_ISOLATED").as_deref() != Ok("1"),
            });
            let mut state = runtime.state.lock().unwrap();
            state.busy = false;
            match result {
                Ok(service) => state.service = Some(Arc::new(service)),
                Err(error) => state.error = Some(error.to_string()),
            }
        });
    }

    fn snapshot(&self) -> Option<UpdateSnapshot> {
        let service = self.state.lock().unwrap().service.clone();
        service.map(|service| service.snapshot())
    }

    fn start(self: &Arc<Self>, action: UpdateAction, host: Arc<HostState>) {
        self.start_action(action, host, false);
    }

    fn start_automatic_check(self: &Arc<Self>, host: Arc<HostState>) {
        self.start_action(UpdateAction::Check, host, true);
    }

    fn start_action(self: &Arc<Self>, action: UpdateAction, host: Arc<HostState>, automatic: bool) {
        let mut state = self.state.lock().unwrap();
        if state.busy || state.awaiting_exit || (automatic && state.prepared.is_some()) {
            return;
        }
        let Some(service) = state.service.clone() else {
            drop(state);
            self.start_initialization();
            return;
        };
        let prepared = state.prepared.clone();
        if matches!(action, UpdateAction::Check) {
            state.prepared = None;
        }
        state.busy = true;
        state.error = None;
        drop(state);
        if automatic {
            tracing::info!(target: "cliphost_gpui::updates", "Starting automatic update check");
        }
        let runtime = self.clone();
        std::thread::spawn(move || {
            let result: Result<Option<Arc<PreparedUpdate>>, String> = match action {
                UpdateAction::Check => service
                    .check()
                    .map(|_| None)
                    .map_err(|error| error.to_string()),
                UpdateAction::Download => service
                    .download_and_prepare()
                    .map_err(|error| error.to_string())
                    .and_then(|prepared| {
                        let prepared = Arc::new(prepared);
                        // Keep the verified package for a retry if helper
                        // startup or an unsaved draft prevents restarting.
                        runtime.state.lock().unwrap().prepared = Some(prepared.clone());
                        service
                            .spawn_install_helper(&prepared)
                            .map(|_| Some(prepared))
                            .map_err(|error| error.to_string())
                    }),
                UpdateAction::Install => prepared
                    .ok_or_else(|| tr("请先下载更新。", "Download the update first.").to_string())
                    .and_then(|prepared| {
                        service
                            .spawn_install_helper(&prepared)
                            .map(|_| Some(prepared))
                            .map_err(|error| error.to_string())
                    }),
            };
            if automatic {
                match &result {
                    Ok(_) => tracing::info!(target: "cliphost_gpui::updates", "Automatic update check completed"),
                    Err(error) => tracing::warn!(target: "cliphost_gpui::updates", %error, "Automatic update check failed"),
                }
            }
            let mut state = runtime.state.lock().unwrap();
            state.busy = false;
            match result {
                Ok(prepared) => {
                    if let Some(prepared) = prepared {
                        state.prepared = Some(prepared);
                    }
                    if matches!(action, UpdateAction::Download | UpdateAction::Install) {
                        state.awaiting_exit = true;
                        drop(state);
                        // The root flushes every editor and settings queue. It
                        // authorizes the helper only after that barrier succeeds.
                        host.chrome_web_action(&json!({"kind":"mainHostCommand","command":"exit"}));
                    }
                }
                Err(error) => state.error = Some(error),
            }
        });
    }
}

/// Called on the background executor after the application's complete save
/// barrier succeeds. A normal exit has no pending helper and is a no-op.
pub(crate) fn authorize_update_exit() -> Result<(), String> {
    let Some(runtime) = UPDATE_RUNTIME.get() else {
        return Ok(());
    };
    let state = runtime.state.lock().unwrap();
    if !state.awaiting_exit {
        return Ok(());
    }
    let service = state
        .service
        .clone()
        .ok_or("Update service is unavailable")?;
    let prepared = state
        .prepared
        .clone()
        .ok_or("Prepared update is unavailable")?;
    drop(state);
    service
        .authorize_install_helper(&prepared)
        .map_err(|error| error.to_string())
}

/// Clears the pending install immediately; file I/O cancellation is performed
/// outside GPUI so a later unrelated exit can never authorize this helper.
pub(crate) fn update_exit_cancelled(error: &str) {
    let Some(runtime) = UPDATE_RUNTIME.get() else {
        return;
    };
    let mut state = runtime.state.lock().unwrap();
    if !state.awaiting_exit {
        return;
    }
    state.awaiting_exit = false;
    state.busy = true;
    state.error = Some(format!(
        "{}\n{error}",
        tr(
            "更新尚未安装，请处理未保存内容后重试。",
            "The update was not installed. Resolve unsaved changes and retry."
        )
    ));
    let pair = state.service.clone().zip(state.prepared.clone());
    drop(state);
    if let Some((service, prepared)) = pair {
        let runtime = runtime.clone();
        std::thread::spawn(move || {
            let result = service.cancel_install_helper(&prepared);
            let mut state = runtime.state.lock().unwrap();
            state.busy = false;
            if let Err(error) = result {
                state.error = Some(format!(
                    "{}\n{error}",
                    state.error.as_deref().unwrap_or_default()
                ));
            }
        });
    } else {
        runtime.state.lock().unwrap().busy = false;
    }
}

pub(super) fn search_schema() -> Value {
    json!({"configLabel":tr("关于与更新", "About & updates"),"fields":[
        {"key":"version","label":tr("版本与检查更新", "Version and check for updates"),"description":"ClipHost update 更新 升级 新版本 发行说明 release notes download 下载 重启 restart"},
        {"key":"installPath","label":tr("程序安装位置", "Application location"),"description":"安装目录 路径 exe installation path Program Files"},
        {"key":"dataRoot","label":tr("数据位置", "Data location"),"description":"历史 收藏 数据目录 data directory history favorites"}
    ]})
}

pub(super) struct UpdateSettingsPage {
    host: Arc<HostState>,
    runtime: Arc<UpdateRuntime>,
    tokens: DesignTokens,
    dark: bool,
    palette: Option<SettingsPalette>,
    leave_locked: bool,
    check_focus: FocusHandle,
    action_focus: FocusHandle,
    release_focus: FocusHandle,
    install_focus: FocusHandle,
    data_focus: FocusHandle,
    _watch: Task<()>,
}

impl UpdateSettingsPage {
    pub(super) fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let runtime = UpdateRuntime::shared(&host);
        let watch = cx.spawn_in(window, async move |this, cx| {
            let mut previous = String::new();
            loop {
                Timer::after(Duration::from_millis(200)).await;
                if this
                    .update_in(cx, |this, _, cx| {
                        let snapshot = this.runtime.snapshot();
                        let state = this.runtime.state.lock().unwrap();
                        let current = format!(
                            "{:?}|{}|{:?}|{}|{}",
                            snapshot.as_ref().map(|s| s.phase),
                            snapshot.as_ref().map_or(0, |s| s.downloaded_bytes),
                            state.error,
                            state.busy,
                            state.awaiting_exit
                        );
                        if previous != current {
                            previous = current;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            host,
            runtime,
            tokens,
            dark,
            palette: None,
            leave_locked: false,
            check_focus: cx.focus_handle(),
            action_focus: cx.focus_handle(),
            release_focus: cx.focus_handle(),
            install_focus: cx.focus_handle(),
            data_focus: cx.focus_handle(),
            _watch: watch,
        }
    }

    pub(super) fn set_design_tokens(
        &mut self,
        tokens: DesignTokens,
        dark: bool,
        palette: Option<SettingsPalette>,
        cx: &mut Context<Self>,
    ) {
        self.tokens = tokens;
        self.dark = dark;
        self.palette = palette;
        cx.notify();
    }
    pub(super) fn set_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.leave_locked = locked;
        cx.notify();
    }
    pub(super) fn focus_handles(&self) -> Vec<FocusHandle> {
        let snapshot = self.runtime.snapshot();
        let state = self.runtime.state.lock().unwrap();
        let mut handles = vec![self.check_focus.clone()];
        if snapshot.as_ref().is_some_and(|s| s.install_allowed)
            && (state.prepared.is_some()
                || snapshot.as_ref().is_some_and(|s| {
                    s.latest_version.is_some()
                        && matches!(s.phase, UpdatePhase::Available | UpdatePhase::Error)
                }))
        {
            handles.push(self.action_focus.clone());
        }
        if snapshot.as_ref().is_some_and(|s| s.release_url.is_some()) {
            handles.push(self.release_focus.clone());
        }
        handles.extend([self.install_focus.clone(), self.data_focus.clone()]);
        handles
    }
    pub(super) fn diagnostics(&self) -> Value {
        let snapshot = self.runtime.snapshot();
        let state = self.runtime.state.lock().unwrap();
        json!({"phase":snapshot.as_ref().map(|s| format!("{:?}",s.phase)),"busy":state.busy,
            "awaitingExit":state.awaiting_exit,"error":state.error,"installAllowed":snapshot.as_ref().is_some_and(|s| s.install_allowed),
            "installPath":self.runtime.executable.parent(),"dataRoot":self.runtime.data_root})
    }
    fn action(&mut self, action: UpdateAction, cx: &mut Context<Self>) {
        if self.leave_locked {
            return;
        }
        self.runtime.start(action, self.host.clone());
        cx.notify();
    }
    fn open_path(&self, path: PathBuf) {
        if self.leave_locked {
            return;
        }
        let host = self.host.clone();
        std::thread::spawn(move || {
            let _ = host.shell_open_path(&json!({"path":path}));
        });
    }
    fn open_release(&self, uri: String) {
        if self.leave_locked {
            return;
        }
        if !url::Url::parse(&uri).is_ok_and(|url| url.scheme() == "https") {
            return;
        }
        let host = self.host.clone();
        std::thread::spawn(move || {
            let _ = host.shell_open_uri(&json!({"uri":uri}));
        });
    }
    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        focus: &FocusHandle,
        enabled: bool,
        primary: bool,
        t: &ThemeTokens,
        p: SettingsPalette,
    ) -> gpui::Stateful<gpui::Div> {
        rsx! { <div id={id} min_h={px(self.tokens.settings.control_height)} px={px(12.0)} flex_none flex items_center justify_center
            rounded={px(7.0)} border_1 border_color={if primary { color(&t.accent) } else { p.control_border }}
            bg={if primary { color(&t.accent) } else { p.control }} text_color={if primary {gpui::white()} else {color(&t.text)}}
            text_size={px(12.0)} whitespace_nowrap>{label}</div> }
            .track_focus(focus).focus(|style| style.border_color(p.focus_border))
            .when(!enabled, |button| button.opacity(0.5))
            .when(enabled, |button| button.cursor_pointer().hover(|style| style.bg(if primary {color(&t.accent)} else {p.control_hover})))
    }
    fn path_row(
        &self,
        label: &'static str,
        path: PathBuf,
        focus: &FocusHandle,
        id: &'static str,
        t: &ThemeTokens,
        p: SettingsPalette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let value = display_path(&path);
        let keyboard_path = path.clone();
        let enabled = !self.leave_locked;
        widgets::row(true, false, &self.tokens.settings)
            .child(rsx! { <div text_size={px(12.0)} text_color={p.description}>{label}</div> })
            .child(rsx! { <div min_w_0 w_full text_size={px(12.0)} line_height={relative(1.4)} text_color={color(&t.text)}>{value}</div> })
            .child(self.button(id, tr("打开文件夹", "Open folder"), focus, enabled, false, t, p)
                .on_click(cx.listener(move |this, _, _, _| this.open_path(path.clone())))
                .on_key_down(cx.listener(move |this,event: &KeyDownEvent,window,cx| {
                    if matches!(event.keystroke.key.as_str(),"enter"|"space") {
                        window.prevent_default();cx.stop_propagation();this.open_path(keyboard_path.clone());
                    }
                })))
            .into_any_element()
    }
}

fn phase_label(phase: UpdatePhase) -> &'static str {
    match phase {
        UpdatePhase::Idle => tr("手动检查可用的新版本。", "Check for a new version."),
        UpdatePhase::Checking => tr("正在检查更新…", "Checking for updates…"),
        UpdatePhase::Available => tr("发现新版本", "An update is available"),
        UpdatePhase::UpToDate => tr("当前已是最新版本", "You're up to date"),
        UpdatePhase::Downloading => tr("正在下载更新…", "Downloading update…"),
        UpdatePhase::Verifying => tr("正在校验更新包…", "Verifying update…"),
        UpdatePhase::Ready => tr("更新已就绪", "Update is ready"),
        UpdatePhase::Installing => tr("正在保存并重启更新…", "Saving and restarting to update…"),
        UpdatePhase::Error => tr(
            "更新未完成，可以重试。",
            "The update did not finish. You can retry.",
        ),
    }
}

fn display_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_string()
    }
}

fn service_message(message: &str) -> String {
    match message {
        "当前为开发或隔离实例，可检查版本；请在正式安装版中更新。" => tr(
            "当前为开发或隔离实例，可检查版本；请在正式安装版中更新。",
            "This is a development or isolated build. You can check for updates; use the installed release to update.",
        ).to_string(),
        _ => message.to_string(),
    }
}

impl Render for UpdateSettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark);
        let s = &tokens.settings;
        let p = self.palette.unwrap_or_else(|| SettingsPalette::new(t));
        let snapshot = self.runtime.snapshot();
        let state = self.runtime.state.lock().unwrap();
        let busy = state.busy || state.awaiting_exit || self.leave_locked;
        let error = state.error.clone();
        let prepared = state.prepared.is_some();
        let awaiting_exit = state.awaiting_exit;
        drop(state);
        let install_allowed = snapshot.as_ref().is_some_and(|s| s.install_allowed);
        let phase = snapshot.as_ref().map_or(UpdatePhase::Idle, |s| s.phase);
        let available = snapshot
            .as_ref()
            .is_some_and(|s| s.latest_version.is_some())
            && matches!(phase, UpdatePhase::Available | UpdatePhase::Error);
        let status = if awaiting_exit {
            tr("正在保存并重启更新…", "Saving and restarting to update…")
        } else if error.is_some() {
            tr(
                "更新未完成，可以重试。",
                "The update did not finish. You can retry.",
            )
        } else if snapshot.is_none() {
            tr("正在准备更新服务…", "Preparing update service…")
        } else {
            phase_label(phase)
        };
        let mut card = rsx! { <div w_full min_w_0 flex_none flex flex_col gap={px(12.0)} p={px(s.row_padding_x)}
            rounded={px(s.card_radius)} border_1 border_color={p.card_border} bg={p.card}>
            <div flex flex_wrap items_center gap={px(8.0)}>
                <div text_size={px(16.0)} font_weight={FontWeight::SEMIBOLD}>{"ClipHost"}</div>
                <div text_size={px(12.0)} text_color={p.description}>{format!("{} {}{}", tr("当前版本", "Version"), env!("CARGO_PKG_VERSION"), if cfg!(debug_assertions) { " · Debug" } else { "" })}</div>
            </div>
            <div text_size={px(13.0)} line_height={relative(1.45)} text_color={color(&t.text)}>{status}</div>
        </div> };
        if let Some(latest) = snapshot.as_ref().and_then(|s| s.latest_version.as_ref()) {
            if !matches!(phase, UpdatePhase::UpToDate) {
                card = card.child(rsx! { <div text_size={px(12.0)} text_color={p.description}>{format!("{} {latest}",tr("新版本", "New version"))}</div> });
            }
        }
        if matches!(phase, UpdatePhase::Downloading | UpdatePhase::Verifying) {
            let downloaded = snapshot.as_ref().map_or(0, |s| s.downloaded_bytes);
            let total = snapshot.as_ref().and_then(|s| s.total_bytes);
            let progress = total
                .filter(|total| *total > 0)
                .map(|total| (downloaded as f32 / total as f32).clamp(0.0, 1.0));
            let progress_text = total.filter(|total| *total > 0).map_or_else(
                || format!("{:.1} MB", downloaded as f64 / 1_048_576.0),
                |total| {
                    format!(
                        "{:.0}% · {:.1} / {:.1} MB",
                        progress.unwrap_or(0.0) * 100.0,
                        downloaded as f64 / 1_048_576.0,
                        total as f64 / 1_048_576.0
                    )
                },
            );
            card = card.child(rsx! { <div h={px(4.0)} w_full rounded_full overflow_hidden bg={p.control} /> }
                .child(div().h_full().w(relative(progress.unwrap_or(0.12))).bg(color(&t.accent)).rounded_full()))
                .child(rsx! { <div text_size={px(12.0)} text_color={p.description}>{progress_text}</div> });
        }
        let mut buttons = rsx! { <div flex flex_wrap items_center gap={px(8.0)} /> }.child(
            self.button(
                "settings-update-check",
                if error.is_some() {
                    tr("重试检查", "Retry check")
                } else {
                    tr("检查更新", "Check for updates")
                },
                &self.check_focus,
                !busy,
                false,
                t,
                p,
            )
            .when(!busy, |button| {
                button
                    .on_click(cx.listener(|this, _, _, cx| this.action(UpdateAction::Check, cx)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.action(UpdateAction::Check, cx);
                        }
                    }))
            }),
        );
        if prepared || (available && install_allowed) {
            let action = if prepared {
                UpdateAction::Install
            } else {
                UpdateAction::Download
            };
            buttons = buttons.child(
                self.button(
                    "settings-update-apply",
                    tr("更新并重启", "Update & restart"),
                    &self.action_focus,
                    !busy && install_allowed,
                    true,
                    t,
                    p,
                )
                .when(!busy && install_allowed, |button| {
                    button
                        .on_click(cx.listener(move |this, _, _, cx| this.action(action, cx)))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.action(action, cx);
                            }
                        }))
                }),
            );
        }
        card = card.child(buttons);
        if let Some(error) = error {
            card = card.child(rsx! { <div min_w_0 text_size={px(12.0)} line_height={relative(1.45)} text_color={color(&t.error)}>{error}</div> });
        }
        if let Some(message) = snapshot
            .as_ref()
            .filter(|snapshot| snapshot.phase == UpdatePhase::Idle)
            .map(|snapshot| snapshot.message.as_str())
            .filter(|message| {
                !message.is_empty() && *message != "点击检查更新。" && *message != status
            })
        {
            card = card.child(widgets::note(service_message(message), p));
        }
        if snapshot.is_some() && !install_allowed && phase != UpdatePhase::Idle {
            card = card.child(widgets::note(tr("当前为开发或非标准安装位置，可以检查更新；请从正式安装版完成软件更新。", "This is a development build or a nonstandard location. You can check for updates; install updates from the installed release."),p));
        }
        if let Some(notes) = snapshot
            .as_ref()
            .and_then(|s| s.release_notes.as_ref())
            .filter(|notes| !notes.trim().is_empty())
        {
            card = card.child(rsx! { <div min_w_0 flex flex_col gap={px(6.0)} border_t_1 border_color={p.row_border} pt={px(12.0)}>
                <div text_size={px(12.0)} font_weight={FontWeight::MEDIUM}>{tr("更新说明", "Release notes")}</div>
                <div min_w_0 text_size={px(12.0)} line_height={relative(1.5)} text_color={p.description}>{notes.clone()}</div>
            </div> });
        }
        if let Some(uri) = snapshot.as_ref().and_then(|s| s.release_url.clone()) {
            let keyboard_uri = uri.clone();
            card = card.child(
                self.button(
                    "settings-update-release-notes",
                    tr("查看更新说明", "View release notes"),
                    &self.release_focus,
                    !self.leave_locked,
                    false,
                    t,
                    p,
                )
                .on_click(cx.listener(move |this, _, _, _| this.open_release(uri.clone())))
                .on_key_down(cx.listener(
                    move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.open_release(keyboard_uri.clone());
                        }
                    },
                )),
            );
        }
        let install_path = snapshot
            .as_ref()
            .map(|s| s.install_path.clone())
            .unwrap_or_else(|| {
                self.runtime
                    .executable
                    .parent()
                    .unwrap_or(&self.runtime.executable)
                    .to_path_buf()
            });
        let paths = widgets::card(
            [
                self.path_row(
                    tr("程序安装位置", "Application location"),
                    install_path,
                    &self.install_focus,
                    "settings-update-install-folder",
                    t,
                    p,
                    cx,
                ),
                self.path_row(
                    tr("数据位置", "Data location"),
                    self.runtime.data_root.clone(),
                    &self.data_focus,
                    "settings-update-data-folder",
                    t,
                    p,
                    cx,
                ),
            ],
            s,
            p,
        );
        rsx! { <div w_full min_w_0 flex_none flex flex_col gap={px(20.0)} text_color={color(&t.text)}>
            {card}
            <div flex flex_col gap={px(8.0)}>
                {paths}
                {widgets::note(tr("更新替换程序文件，历史、收藏与设置保留在原数据位置。", "Updates replace application files. History, favorites and settings stay in the current data folder."),p)}
            </div>
        </div> }
    }
}
