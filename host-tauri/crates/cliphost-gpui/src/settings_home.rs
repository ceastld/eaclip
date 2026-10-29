//! Home page of the unified main window. It replaces the former management
//! window's home: commands and window behavior still go through HostState's
//! `mainHostCommand`; navigation to sibling pages is requested from the parent.

use super::*;
use gpui::{ClipboardItem, Div, EventEmitter};

/// Asks SettingsWindow to open another page through its save/leave barrier.
pub(super) enum HomeEvent {
    Open(&'static str),
}

impl EventEmitter<HomeEvent> for HomeSettingsPage {}

pub(super) struct HomeSettingsPage {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    palette: Option<SettingsPalette>,
    status: Value,
    info: Value,
    message: Option<(bool, String)>,
    leave_locked: bool,
    request: u64,
    _watch: Task<()>,
}

const QUICK_OPEN: [(&str, &str, &str, &str); 8] = [
    ("clipboard", "剪贴板", "Clipboard", "clipboard"),
    ("panel", "超级面板", "Panel", "star"),
    ("super", "超级菜单", "Super menu", "list"),
    ("file", "文件菜单", "Files", "file"),
    ("window", "窗口菜单", "Windows", "desktop"),
    ("bookmarks", "书签", "Bookmarks", "link"),
    ("favor", "收藏", "Favorites", "star"),
    ("clipboardMenu", "剪贴板菜单", "Clipboard menu", "paste"),
];

impl HomeSettingsPage {
    pub(super) fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Same 500 ms state watch as the former management home.
        let watch = cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if this
                .update_in(cx, |this, _, cx| {
                    let status = this.host.main_host_state_dto();
                    if status != this.status {
                        this.status = status;
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        });
        let mut page = Self {
            status: host.main_host_state_dto(),
            host,
            tokens,
            dark,
            palette: None,
            info: Value::Null,
            message: None,
            leave_locked: false,
            request: 0,
            _watch: watch,
        };
        page.refresh(window, cx);
        page
    }

    pub(super) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let info = cx.background_executor().spawn(async move { host.app_info() }).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.request == request {
                    this.info = info;
                    this.status = this.host.main_host_state_dto();
                    cx.notify();
                }
            });
        })
        .detach();
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
        if self.leave_locked != locked {
            self.leave_locked = locked;
            cx.notify();
        }
    }

    fn command(&mut self, body: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked {
            return;
        }
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.chrome_web_action(&body) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.message = (result["ok"] == false).then(|| (true, string(&result, "error")));
                this.status = this.host.main_host_state_dto();
                cx.notify();
            });
        })
        .detach();
    }

    fn host_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.command(json!({"kind": "mainHostCommand", "command": command}), window, cx);
    }

    fn copy_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked {
            return;
        }
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { host.latest_log() }).await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.message = Some(match result {
                    Ok(log) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(string(&log, "text")));
                        (false, tr("已复制最新日志到剪贴板", "Latest log copied to the clipboard.").into())
                    }
                    Err(error) => (true, error),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn version(&self) -> String {
        let host = &self.info["clipHost"];
        let version = if host["fileVersion"].is_null() { &host["assemblyVersion"] } else { &host["fileVersion"] };
        version.as_str().unwrap_or_default().to_string()
    }

    fn icon(&self, name: &str) -> SharedString {
        self.tokens.icons.get(name).cloned().unwrap_or_default().into()
    }

    fn button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        danger: bool,
        t: &ThemeTokens,
        palette: SettingsPalette,
    ) -> gpui::Stateful<Div> {
        let s = &self.tokens.settings;
        div()
            .id(id.into())
            .h(px(s.control_height))
            .px(px(10.0))
            .flex_none()
            .flex()
            .items_center()
            .rounded(px(8.0))
            .border_1()
            .border_color(palette.control_border)
            .bg(palette.control)
            .text_size(px(12.0))
            .text_color(color(if danger { &t.error } else { &t.text_secondary }))
            .when(!self.leave_locked, |button| {
                button.cursor_pointer().hover(|style| style.bg(palette.control_hover))
            })
            .when(self.leave_locked, |button| button.opacity(0.45))
            .child(label.into())
    }

    fn tile(
        &self,
        id: &'static str,
        icon: &str,
        title: &'static str,
        detail: String,
        action: &'static str,
        t: &ThemeTokens,
        palette: SettingsPalette,
    ) -> gpui::Stateful<Div> {
        let s = &self.tokens.settings;
        div()
            .id(SharedString::from(format!("home-tile-{id}")))
            .flex_1()
            .min_w(px(160.0))
            .p(px(12.0))
            .flex()
            .gap(px(10.0))
            .rounded(px(s.card_radius))
            .border_1()
            .border_color(palette.card_border)
            .bg(palette.card)
            .cursor_pointer()
            .hover(|style| style.bg(palette.row_hover).border_color(palette.control_hover_border))
            .child(svg().path(self.icon(icon)).size(px(18.0)).flex_none().mt(px(2.0)).text_color(color(&t.accent)))
            .child(rsx! {
                <div flex_1 min_w_0 flex flex_col gap={px(2.0)}>
                    <div text_size={px(14.0)} font_weight={FontWeight::SEMIBOLD}
                        text_color={color(&t.text)}>{title}</div>
                    <div text_size={px(12.0)} text_color={palette.description} truncate>{detail}</div>
                    <div pt={px(6.0)} flex items_center gap={px(2.0)} text_size={px(12.0)}
                        text_color={color(&t.text_secondary)}>
                        {action}
                        {svg().path(self.icon("chevron-right")).size(px(12.0)).text_color(color(&t.text_muted))}
                    </div>
                </div>
            })
    }
}

impl Render for HomeSettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        let s = self.tokens.settings.clone();
        let palette = self.palette.unwrap_or_else(|| SettingsPalette::new(&t));
        let version = self.version();
        let hero = rsx! {
            <div flex_none flex flex_col gap={px(2.0)}>
                <div flex items_center gap={px(10.0)}>
                    <div text_size={px(18.0)} font_weight={FontWeight::SEMIBOLD}>{"ClipHost"}</div>
                    <div h={px(20.0)} px={px(8.0)} flex items_center gap={px(5.0)} rounded_full
                        bg={color(&t.accent).opacity(0.12)} text_size={px(11.0)}
                        text_color={color(&t.accent)}>
                        <div size={px(6.0)} rounded_full bg={color(&t.accent)} />
                        {tr("正在运行", "Running")}
                    </div>
                </div>
                <div text_size={px(12.0)} text_color={palette.description}>
                    {if version.is_empty() { tr("剪贴板监听中", "Watching the clipboard").to_string() }
                        else { format!("{version} · {}", tr("剪贴板监听中", "Watching the clipboard")) }}
                </div>
            </div>
        };
        let tiles = rsx! { <div w_full flex flex_wrap gap={px(8.0)} /> }
            .child(self.tile("clipboard", "clipboard", tr("剪贴板", "Clipboard"),
                tr("查看与粘贴历史", "Browse and paste history").into(), tr("打开", "Open"), &t, palette)
                .on_click(cx.listener(|this, _, window, cx| this.host_command("openClipboard", window, cx))))
            .child(self.tile("devices", "laptop", tr("跨设备", "Cross-device"),
                tr("配对设备与同步剪贴板", "Pair devices and sync clipboard").into(), tr("设置", "Settings"), &t, palette)
                .on_click(cx.listener(|this, _, _, cx| if !this.leave_locked { cx.emit(HomeEvent::Open(DEVICES_PAGE)) })))
            .child(self.tile("update", "refresh", tr("软件更新", "Updates"),
                if version.is_empty() { tr("检查与安装新版本", "Check and install updates").into() }
                else { format!("{} {version}", tr("当前版本", "Version")) },
                tr("查看", "View"), &t, palette)
                .on_click(cx.listener(|this, _, _, cx| if !this.leave_locked { cx.emit(HomeEvent::Open(UPDATE_PAGE)) })));

        let launch = rsx! { <div w_full flex flex_wrap gap={px(2.0)} p={px(4.0)} /> }
            .children(QUICK_OPEN.into_iter().map(|(menu, zh, en, icon)| {
                div()
                    .id(SharedString::from(format!("home-open-{menu}")))
                    .w(px(84.0))
                    .h(px(64.0))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .rounded(px(8.0))
                    .text_size(px(12.0))
                    .text_color(color(&t.text_secondary))
                    .cursor_pointer()
                    .hover(|style| style.bg(palette.control_hover))
                    .child(svg().path(self.icon(icon)).size(px(18.0)).text_color(color(&t.text_secondary)))
                    .child(tr(zh, en))
                    .on_click(cx.listener(move |this, _, window, cx| match menu {
                        "clipboard" => this.host_command("openClipboard", window, cx),
                        "panel" => this.host_command("openPanel", window, cx),
                        "clipboardMenu" => this.command(json!({"kind": "mainHostCommand", "command": "openMenu", "menu": "clipboard"}), window, cx),
                        _ => this.command(json!({"kind": "mainHostCommand", "command": "openMenu", "menu": menu}), window, cx),
                    }))
            }));

        let show_tray = self.status["showTrayIcon"] != false;
        let background = self.status["closeBehavior"] != "ExitApplication";
        let knob = (s.switch_height - 6.0).max(1.0);
        let travel = (s.switch_width - 6.0 - knob).max(0.0);
        let tray_switch = div()
            .id("home-tray-switch")
            .flex_none()
            .w(px(s.switch_width))
            .h(px(s.switch_height))
            .rounded_full()
            .border_1()
            .border_color(if show_tray { color(&t.accent) } else { palette.control_border })
            .p(px(2.0))
            .flex()
            .bg(color(if show_tray { &t.accent } else { &t.tertiary }))
            .child(div().relative().left(px(if show_tray { travel } else { 0.0 })).flex_none().size(px(knob))
                .rounded_full().bg(if show_tray { gpui::white() } else { color(&t.text_muted) }))
            .when(!self.leave_locked, |switch| switch.cursor_pointer())
            .on_click(cx.listener(move |this, _, window, cx| this.command(
                json!({"kind": "mainHostCommand", "command": "setShowTrayIcon", "show": !show_tray}), window, cx)));
        let close_choice = widgets::segmented(palette).flex_none().children([
            ("minimizeToTray", if show_tray { tr("缩小到托盘", "Minimize to tray") } else { tr("在后台运行", "Keep running") }),
            ("exitApplication", tr("退出程序", "Exit application")),
        ].into_iter().map(|(behavior, label)| {
            let active = background == (behavior == "minimizeToTray");
            div()
                .id(SharedString::from(format!("home-close-{behavior}")))
                .min_h(px(s.chip_height))
                .px(px(s.chip_padding_x))
                .py(px(3.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .text_size(px(13.0))
                .text_color(color(if active { &t.text } else { &t.text_secondary }))
                .when(active, |chip| chip.bg(palette.control_hover).font_weight(FontWeight::SEMIBOLD))
                .when(!active && !self.leave_locked, |chip| chip.cursor_pointer().hover(|style| style.bg(palette.control_hover)))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| this.command(
                    json!({"kind": "mainHostCommand", "command": "setCloseBehavior", "behavior": behavior}), window, cx)))
        }));
        let row = |label: &'static str, control: AnyElement| {
            widgets::row(false, false, &s)
                .child(widgets::label_column(false).child(widgets::label(false, &t).child(label)))
                .child(control)
                .into_any_element()
        };
        let behavior = widgets::card([
            row(tr("显示托盘图标", "Show tray icon"), tray_switch.into_any_element()),
            row(tr("关闭主界面时", "When closing the main window"), close_choice.into_any_element()),
        ], &s, palette);

        let quicker = if self.status["verbRpcListenerReady"] == true {
            tr("可以使用", "Ready to use")
        } else {
            tr("暂时无法打开", "Currently unavailable")
        };
        let runtime = widgets::card([
            row(tr("从 Quicker 打开剪贴板", "Open clipboard from Quicker"),
                widgets::description(palette).child(quicker).into_any_element()),
            row(tr("遇到问题时查看日志", "View logs to investigate a problem"), rsx! { <div flex flex_wrap justify_end gap={px(6.0)} /> }
                .child(self.button("home-log", tr("查看日志", "View log"), false, &t, palette)
                    .on_click(cx.listener(|this, _, window, cx| this.host_command("openLog", window, cx))))
                .child(self.button("home-copy-log", tr("复制日志", "Copy log"), false, &t, palette)
                    .on_click(cx.listener(|this, _, window, cx| this.copy_log(window, cx))))
                .into_any_element()),
            row(tr("重启或退出剪贴板", "Restart or quit ClipHost"), rsx! { <div flex flex_wrap justify_end gap={px(6.0)} /> }
                .child(self.button("home-restart", tr("重启", "Restart"), false, &t, palette)
                    .on_click(cx.listener(|this, _, window, cx| this.host_command("restart", window, cx))))
                .child(self.button("home-exit", tr("退出", "Exit"), true, &t, palette)
                    .on_click(cx.listener(|this, _, window, cx| this.host_command("exit", window, cx))))
                .into_any_element()),
        ], &s, palette);

        rsx! { <div w_full min_w_0 flex_none flex flex_col gap={px(20.0)} text_color={color(&t.text)} /> }
            .child(hero)
            .children(self.message.as_ref().map(|(error, message)| {
                widgets::note(message.clone(), palette).text_color(color(if *error { &t.error } else { &t.accent }))
            }))
            .child(tiles)
            .child(widgets::section(tr("快捷打开", "Quick open"), &s, &t)
                .child(widgets::card([launch.into_any_element()], &s, palette)))
            .child(widgets::section(tr("窗口行为", "Window behavior"), &s, &t).child(behavior))
            .child(widgets::section(tr("程序管理", "App controls"), &s, &t).child(runtime))
    }
}

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}
