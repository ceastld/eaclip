//! Development-only controls for the core's WebDAV worker. Account setup and
//! recovery keys stay in the helper script, outside this view's state.

use super::*;

pub(super) struct WebDavSettingsPage {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    palette: Option<SettingsPalette>,
    status: Value,
    loading: bool,
    action: Option<&'static str>,
    error: Option<String>,
    request: u64,
    active: bool,
    poll: Option<Task<()>>,
    focus: [FocusHandle; 4],
}

impl WebDavSettingsPage {
    pub(super) fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut page = Self {
            host,
            tokens,
            dark,
            palette: None,
            status: Value::Null,
            loading: false,
            action: None,
            error: None,
            request: 0,
            active: false,
            poll: None,
            focus: std::array::from_fn(|_| cx.focus_handle()),
        };
        page.refresh(window, cx);
        page
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

    pub(super) fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        self.poll = None;
        if !active {
            return;
        }
        self.refresh(window, cx);
        self.poll = Some(cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_secs(3)).await;
            let result = this.update_in(cx, |this, window, cx| {
                if this.active {
                    // Status is a local read; polling never initiates a sync.
                    this.refresh(window, cx);
                }
                this.active
            });
            if !matches!(result, Ok(true)) {
                break;
            }
        }));
    }

    pub(super) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.action.is_some() {
            return;
        }
        self.loading = true;
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { host.webdav_dev_status() })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.request != request {
                    return;
                }
                this.loading = false;
                this.status = status;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn ready(&self, command: &str) -> bool {
        if self.loading || self.action.is_some() || !self.status["configured"].is_boolean() {
            return false;
        }
        let configured = self.status["configured"] == true;
        let enabled = self.status["enabled"] == true;
        let running = self.status["running"] == true;
        match command {
            "sync" => configured && !running,
            "enable" => configured && !enabled,
            "disable" => enabled,
            "status" => true,
            _ => false,
        }
    }

    pub(super) fn focus_handles(&self) -> Vec<FocusHandle> {
        ["sync", "enable", "disable", "status"]
            .into_iter()
            .enumerate()
            .filter(|(_, command)| self.ready(command))
            .map(|(index, _)| self.focus[index].clone())
            .collect()
    }

    fn run(&mut self, command: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(command) {
            return;
        }
        if command == "status" {
            self.error = None;
            self.refresh(window, cx);
            return;
        }
        self.action = Some(command);
        self.error = None;
        self.request = self.request.wrapping_add(1);
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.webdav_dev_command(&json!({"command": command})) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.action = None;
                match result {
                    Ok(status) => this.status = status,
                    Err(error) => this.error = Some(error),
                }
                this.refresh(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn button(
        &self,
        command: &'static str,
        label: &'static str,
        index: usize,
        palette: SettingsPalette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let ready = self.ready(command);
        div()
            .id(SharedString::from(format!("settings-webdav-{command}")))
            .track_focus(&self.focus[index])
            .flex_none()
            .min_h(px(self.tokens.settings.control_height))
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(palette.control_border)
            .bg(palette.control)
            .text_size(px(12.0))
            .flex()
            .items_center()
            .justify_center()
            .focus(|style| style.border_color(palette.focus_border))
            .child(label)
            .when(!ready, |button| button.opacity(0.45))
            .when(ready, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(palette.control_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run(command, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "space" | "enter") {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.run(command, window, cx);
                        }
                    }))
            })
            .into_any_element()
    }

    fn status_label(&self) -> &'static str {
        if self.status.is_null() {
            tr("正在读取状态…", "Loading status…")
        } else if self.status["configured"] != true {
            tr("未配置", "Not configured")
        } else if self.action == Some("sync") || self.status["running"] == true {
            tr("正在同步…", "Syncing…")
        } else if self.status["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
        {
            tr("同步需要处理", "Sync needs attention")
        } else if self.status["enabled"] == true {
            tr("后台已开启", "Background sync enabled")
        } else {
            tr("后台已停止", "Background sync stopped")
        }
    }
}

impl Render for WebDavSettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark);
        let s = &self.tokens.settings;
        let palette = self.palette.unwrap_or_else(|| SettingsPalette::new(t));
        let configured = self.status["configured"] == true;
        let enabled = self.status["enabled"] == true;
        let mut rows = vec![widgets::row(false, false, s)
            .child(
                widgets::label_column(false)
                    .child(div().text_size(px(14.0)).child(tr("当前状态", "Status")))
                    .child(widgets::description(palette).child(format!(
                        "{} · {}",
                        if configured {
                            tr("账号已配置", "Account configured")
                        } else {
                            tr("账号未配置", "Account not configured")
                        },
                        if enabled {
                            tr("自动同步已开启", "Automatic sync enabled")
                        } else {
                            tr("自动同步已关闭", "Automatic sync disabled")
                        },
                    ))),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.0))
                    .child(self.status_label()),
            )
            .into_any_element()];
        let report = &self.status["lastReport"];
        if report.is_object() {
            let count = |key: &str| report[key].as_u64().unwrap_or(0);
            rows.push(
                widgets::row(true, false, s)
                    .child(div().text_size(px(13.0)).child(format!(
                        "{} {} · {} {} · {} {}",
                        tr("记录", "Records"),
                        count("entries"),
                        tr("附件", "Attachments"),
                        count("blobs"),
                        tr("冲突", "Conflicts"),
                        count("conflicts"),
                    )))
                    .child(widgets::description(palette).child(format!(
                        "{} {} · {} {}",
                        tr("上传", "Uploaded"),
                        count("uploadedObjects"),
                        tr("下载", "Downloaded"),
                        count("downloadedObjects"),
                    )))
                    .into_any_element(),
            );
        } else {
            rows.push(
                widgets::row(false, false, s)
                    .child(
                        widgets::description(palette)
                            .child(tr("尚无同步记录", "No sync results yet")),
                    )
                    .into_any_element(),
            );
        }
        rows.push(
            widgets::row(true, false, s)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .child(self.button("sync", tr("立即同步", "Sync now"), 0, palette, cx))
                        .child(self.button(
                            "enable",
                            tr("开启后台", "Enable background sync"),
                            1,
                            palette,
                            cx,
                        ))
                        .child(self.button(
                            "disable",
                            tr("停止后台", "Stop background sync"),
                            2,
                            palette,
                            cx,
                        ))
                        .child(self.button("status", tr("刷新", "Refresh"), 3, palette, cx)),
                )
                .into_any_element(),
        );
        let mut section = widgets::section(
            tr(
                "WebDAV 收藏同步 · 开发版",
                "WebDAV collection sync · Development",
            ),
            s,
            t,
        )
        .child(widgets::card(rows, s, palette));
        if !configured {
            section = section.child(widgets::note(tr(
                "先使用 Invoke-ClipWebDav.ps1 配置账号，并创建或加入同步集合。",
                "Use Invoke-ClipWebDav.ps1 to configure an account and create or join a collection.",
            ), palette));
        }
        if let Some(error) = self
            .error
            .as_deref()
            .or_else(|| self.status["error"].as_str())
            .filter(|error| !error.is_empty())
        {
            section =
                section.child(widgets::note(error.to_owned(), palette).text_color(color(&t.error)));
        }
        section
    }
}
