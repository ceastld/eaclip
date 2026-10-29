//! Retained browser entities docked beside the file menu's root column
//! (2026-10-26): the menu is the sidebar, at most two panels sit side by side.
use super::*;
use crate::file_browser::{FileBrowserEvent, FileBrowserWindow};

pub(super) struct BrowserPanel {
    pub id: u64,
    pub browser: Entity<FileBrowserWindow>,
    _subscription: Subscription,
    _observer: Subscription,
}

pub(super) const MAX_BROWSER_PANELS: usize = 2;
const PANEL_MIN_WIDTH: f32 = 360.0;
const SINGLE_DOCK_WIDTH: f32 = 640.0;
const SPLIT_DOCK_WIDTH: f32 = 1100.0;
pub(super) const DOCK_MAX_HEIGHT: f32 = 720.0;
const PANEL_GAP: f32 = 8.0;
const FILE_HINT_HEIGHT: f32 = 20.0;
/// Strip edge to the target/hint line beside it.
const FILE_HINT_GAP: f32 = 12.0;
/// Below this the line would show little more than a clipped target chip.
const FILE_HINT_MIN_WIDTH: f32 = 120.0;

/// The docked area beside the root column. Like a cascade it prefers the
/// right and turns left only when that side cannot hold one readable panel.
pub(super) fn dock_area(root: Rect, count: usize, size: (f32, f32)) -> Rect {
    let height = (size.1 - EDGE - root.y).clamp(1.0, DOCK_MAX_HEIGHT);
    let right_x = root.x + root.width + GAP;
    let right_space = size.0 - EDGE - right_x;
    let left_space = root.x - GAP - EDGE;
    let want = if count >= 2 { SPLIT_DOCK_WIDTH } else { SINGLE_DOCK_WIDTH };
    if right_space >= PANEL_MIN_WIDTH || right_space >= left_space {
        Rect { x: right_x, y: root.y, width: right_space.min(want).max(1.0), height }
    } else {
        let width = left_space.min(want).max(1.0);
        Rect { x: root.x - GAP - width, y: root.y, width, height }
    }
}

fn split_fits(root: Rect, size: (f32, f32)) -> bool {
    let right = size.0 - EDGE - (root.x + root.width + GAP);
    let left = root.x - GAP - EDGE;
    right.max(left) >= PANEL_MIN_WIDTH * 2.0 + PANEL_GAP
}

/// A row that can be shown in the docked browser: real directories only.
pub(super) fn browse_path(node: &Value) -> Option<String> {
    let role = node["role"].as_str()?;
    let path = node["payload"]["path"].as_str()?.trim();
    if path.is_empty() || cliphost_core::explorer::is_shell_path(path) {
        return None;
    }
    match role {
        "file" if node["payload"]["isDirectory"].as_bool() == Some(true) => Some(path.to_string()),
        "opened-folder" | "reopen" => Some(path.to_string()),
        _ => None,
    }
}

impl SuperMenuWindow {
    pub(super) fn browsing(&self) -> bool {
        !self.browser_panels.is_empty()
    }

    pub(super) fn browser_composing(&self, cx: &App) -> bool {
        self.browser_panels.iter().any(|p| p.browser.read(cx).is_composing(cx))
    }

    fn root_rect(&self, size: (f32, f32)) -> Rect {
        self.column_rects.borrow().get(&0).copied().unwrap_or(Rect {
            x: EDGE,
            y: EDGE,
            width: 240.0,
            height: 1.0,
        })
        .clamp_to(size)
    }

    fn can_split(&self, size: (f32, f32)) -> bool {
        self.browser_panels.len() < MAX_BROWSER_PANELS && split_fits(self.root_rect(size), size)
    }

    /// Target kind and title for the accepted menu, cached per session.
    pub(super) fn folder_target(&self) -> (&'static str, String) {
        let Some(request) = &self.keyboard_request else {
            return ("other", String::new());
        };
        let session = self.session.as_ref().map(|key| key.id.clone()).unwrap_or_default();
        if let Some((cached, kind, title)) = self.folder_target_cache.borrow().as_ref() {
            if *cached == session {
                return (kind, title.clone());
            }
        }
        let (kind, title) = self.host.super_menu_folder_target(request);
        *self.folder_target_cache.borrow_mut() = Some((session, kind, title.clone()));
        (kind, title)
    }

    pub(super) fn folder_verb(kind: &str) -> &'static str {
        match kind {
            "dialog" => tr("在对话框中定位", "Locate in the dialog"),
            "explorer" => tr("在资源管理器中转到", "Go there in Explorer"),
            _ => tr("用资源管理器打开", "Open in Explorer"),
        }
    }

    pub(super) fn suspend_browsers(&mut self, cx: &mut Context<Self>) {
        for panel in &self.browser_panels {
            panel
                .browser
                .update(cx, |browser, cx| browser.suspend_embedded(cx));
        }
    }

    pub(super) fn prepare_browsers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let request = self
            .keyboard_request
            .clone()
            .ok_or("Menu session has ended.")?;
        for panel in &self.browser_panels {
            panel.browser.update(cx, |browser, cx| {
                browser.resume_embedded(request.clone(), window, cx)
            });
        }
        if let Some(path) = self.browser_pending_path.take() {
            let split = std::mem::take(&mut self.browser_pending_split);
            self.open_in_browser(&path, split, window, cx);
        }
        Ok(())
    }

    pub(super) fn focus_active_browser(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self
            .browser_panels
            .iter()
            .find(|p| Some(p.id) == self.browser_active)
        {
            panel
                .browser
                .update(cx, |browser, cx| browser.focus_embedded(window, cx));
        } else {
            window.focus(&self.focus);
        }
    }

    /// Keep each panel's parent-owned chrome current: width, split room, the
    /// footer verb for the accepted target and file-menu membership.
    pub(super) fn maintain_browser_geometry(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.category != Category::File || !self.browsing() {
            return;
        }
        let size = (
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let area = dock_area(self.root_rect(size), self.browser_panels.len(), size);
        let count = self.browser_panels.len() as f32;
        let width = ((area.width - PANEL_GAP * (count - 1.0)) / count).max(1.0);
        let can_split = self.can_split(size);
        let verb = Self::folder_verb(self.folder_target().0);
        for panel in &self.browser_panels {
            let in_menu = self.host.file_menu_contains(panel.browser.read(cx).current_path());
            panel.browser.update(cx, |browser, cx| {
                browser.set_embedded_width(width, cx);
                browser.set_embedded_chrome(can_split, verb, in_menu, cx);
            });
        }
    }

    /// Open a folder from the menu. The first panel switches the menu into
    /// focused browsing input; later requests reuse or split the panels.
    pub(super) fn browse_from_menu(&mut self, path: String, split: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_file_preview();
        self.columns.truncate(1);
        self.column_tasks.retain(|index, _| *index == 0);
        if self.browsing() {
            self.open_in_browser(&path, split, window, cx);
            self.focus_active_browser(window, cx);
            cx.notify();
            return;
        }
        self.browser_pending_path = Some(path);
        self.browser_pending_split = split;
        self.prepared_category = None;
        if let Err(error) = self.prepare_category_input(window, cx) {
            self.status = error;
        }
        cx.notify();
    }

    fn open_in_browser(&mut self, path: &str, split: bool, window: &mut Window, cx: &mut Context<Self>) {
        let size = (
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let target = self
            .browser_panels
            .iter()
            .find(|p| Some(p.id) == self.browser_active)
            .or(self.browser_panels.first())
            .map(|p| p.browser.clone());
        match target {
            Some(browser) if !(split && self.can_split(size)) => {
                browser.update(cx, |browser, cx| browser.navigate_embedded(path, window, cx));
            }
            _ => self.add_browser(path, window, cx),
        }
        self.maintain_browser_geometry(window, cx);
    }

    fn add_browser(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self
            .keyboard_request
            .clone()
            .filter(|r| self.host.native_super_menu_request_is_current(r))
        else {
            return;
        };
        self.browser_next_id = self.browser_next_id.wrapping_add(1);
        let id = self.browser_next_id;
        let browser = cx.new(|cx| {
            #[cfg(debug_assertions)]
            if self.browser_fixture {
                return FileBrowserWindow::new_embedded_fixture(
                    self.host.clone(),
                    self.tokens.clone(),
                    self.dark,
                    request,
                    path,
                    window,
                    cx,
                );
            }
            FileBrowserWindow::new_embedded(
                self.host.clone(),
                self.tokens.clone(),
                self.dark,
                request,
                path,
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&browser, window, move |menu, _, event, window, cx| {
            if menu.category != Category::File
                || !menu.session.as_ref().is_some_and(|key| menu.current(key))
            {
                return;
            }
            match event {
                FileBrowserEvent::Dismiss => menu.dismiss(cx),
                FileBrowserEvent::Close => menu.close_browser(id, window, cx),
                FileBrowserEvent::OpenPanel(path) => {
                    menu.browser_active = Some(id);
                    menu.open_in_browser(path, true, window, cx);
                    menu.focus_active_browser(window, cx);
                }
                FileBrowserEvent::Go(path) => menu.send_folder_to_target(path.clone(), cx),
                FileBrowserEvent::CopyPath(path) => menu.browser_copy_path(path.clone(), cx),
                FileBrowserEvent::ToggleFileMenu(path) => menu.browser_toggle_file_menu(path.clone(), window, cx),
            }
        });
        let observer = cx.observe(&browser, |_, _, cx| cx.notify());
        self.browser_panels.push(BrowserPanel {
            id,
            browser,
            _subscription: subscription,
            _observer: observer,
        });
        self.browser_active = Some(id);
        self.focus_active_browser(window, cx);
        cx.notify();
    }

    fn close_browser(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.browser_panels.iter().position(|p| p.id == id) {
            self.browser_panels[index]
                .browser
                .update(cx, |b, cx| b.suspend_embedded(cx));
            self.browser_panels.remove(index);
            if self.browser_active == Some(id) {
                self.browser_active = self.browser_panels.last().map(|p| p.id);
            }
            if self.browser_panels.is_empty() {
                // Back to the plain menu: hand keyboard focus back to the
                // accepted target and resume the native key hook.
                self.prepared_category = None;
                if let Err(error) = self.prepare_category_input(window, cx) {
                    self.status = error;
                }
            } else {
                self.focus_active_browser(window, cx);
            }
            cx.notify();
        }
    }

    fn activate_browser(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.browser_active != Some(id) && self.browser_panels.iter().any(|p| p.id == id) {
            self.browser_active = Some(id);
            cx.notify();
        }
    }

    fn browser_request(&mut self, cx: &mut Context<Self>) -> Option<(SessionKey, NativeSuperMenuPasteRequest)> {
        let key = self.session.clone().filter(|key| self.current(key))?;
        if self.busy {
            return None;
        }
        match self.accepted_action_request(&key) {
            Ok(request) => Some((key, request)),
            Err(error) => {
                self.status = error;
                cx.notify();
                None
            }
        }
    }

    /// Footer button: the same routed destination as a folder row's click.
    fn send_folder_to_target(&mut self, path: String, cx: &mut Context<Self>) {
        let Some((key, request)) = self.browser_request(cx) else { return };
        let host = self.host.clone();
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let reply_request = request.clone();
            let result = crate::native_dialog::run(move || host.menu_go_to_folder_native(&path, &request)).await;
            let _ = this.update(cx, |menu, cx| {
                if !menu.current(&key) {
                    return;
                }
                menu.busy = false;
                menu.action_task = None;
                match result {
                    Ok(_) if menu.host.native_super_menu_close_reply_is_current(&reply_request) => menu.dismiss(cx),
                    Ok(_) => {}
                    Err(error) => menu.status = error,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn browser_copy_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some((key, request)) = self.browser_request(cx) else { return };
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let result = crate::native_dialog::run(move || host.menu_copy_path_native(&path, &request)).await;
            let _ = this.update(cx, |menu, cx| {
                if menu.current(&key) {
                    menu.status = match result {
                        Ok(()) => tr("已复制路径", "Path copied").to_string(),
                        Err(error) => error,
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Add/remove keeps the menu open and reloads the root column, like the
    /// file menu's own context commands.
    fn browser_toggle_file_menu(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser_request(cx).is_none() {
            return;
        }
        let result = if self.host.file_menu_contains(&path) {
            self.host.file_menu_remove(&path)
        } else {
            self.host.file_menu_add(&path)
        };
        if let Err(error) = result {
            self.status = error;
        }
        self.reload_column(0, cx);
        self.maintain_browser_geometry(window, cx);
        cx.notify();
    }

    pub(super) fn browser_workspace(&self, size: (f32, f32), cx: &mut Context<Self>) -> AnyElement {
        let area = dock_area(self.root_rect(size), self.browser_panels.len(), size);
        let t = self.tokens.theme(self.dark);
        let two = self.browser_panels.len() > 1;
        let mut workspace = div()
            .id("file-dock")
            .absolute()
            .left(px(area.x))
            .top(px(area.y))
            .w(px(area.width))
            .h(px(area.height))
            .flex()
            .gap(px(PANEL_GAP));
        for panel in &self.browser_panels {
            let id = panel.id;
            let active = two && Some(id) == self.browser_active;
            workspace = workspace.child(
                self.panel(SharedString::from(format!("file-panel-{id}")))
                    .occlude()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .when(active, |panel| panel.border_color(color(&t.accent).opacity(0.7)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |menu, _, _, cx| {
                            menu.activate_browser(id, cx);
                            menu.content_pointer();
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |menu, _, _, cx| {
                            menu.activate_browser(id, cx);
                            menu.content_pointer();
                            cx.stop_propagation();
                        }),
                    )
                    .child(panel.browser.clone()),
            );
        }
        workspace.into_any_element()
    }

    /// The accepted target and what a click does, in the strip's row (2026-10-28):
    /// above the root it pushed the file menu lower than every other category.
    /// Hidden while settings fly out beside the strip or when neither side has room.
    pub(super) fn file_hint(&self, strip: Rect, size: (f32, f32)) -> Option<AnyElement> {
        if self.settings_open {
            return None;
        }
        let room_right = size.0 - EDGE - (strip.x + strip.width + FILE_HINT_GAP);
        let room_left = strip.x - FILE_HINT_GAP - EDGE;
        let right = room_right >= 360.0 || room_right >= room_left;
        if room_right.max(room_left) < FILE_HINT_MIN_WIDTH {
            return None;
        }
        let t = self.tokens.theme(self.dark);
        let (kind, title) = self.folder_target();
        let verb = Self::folder_verb(kind);
        let target = if title.trim().is_empty() {
            match kind {
                "dialog" => tr("文件对话框", "File dialog").to_string(),
                "explorer" => tr("资源管理器", "Explorer").to_string(),
                _ => tr("当前窗口", "Current window").to_string(),
            }
        } else {
            title
        };
        let text = if self.browsing() {
            format!("{} · {} · {} {verb}", tr("单击 在浏览栏打开", "Click opens here"),
                tr("Ctrl+单击 并排", "Ctrl+click side by side"), tr("底部按钮", "Footer:"))
        } else if kind != "dialog" && self.host.file_menu_prefers_internal_browser() {
            format!("{} · {}", tr("单击 在浏览栏打开", "Click opens here"), tr("悬停 展开", "Hover expands"))
        } else {
            format!("{} {verb} · {} · ⇢ {}", tr("单击", "Click:"), tr("悬停 展开", "Hover expands"),
                tr("在浏览栏打开", "open here"))
        };
        let icon = match kind {
            "dialog" => "file",
            "explorer" => "folder-open",
            _ => "all",
        };
        // Near the right edge the line grows leftwards from the strip's start.
        let hint = div().absolute().top(px(strip.y + (strip.height - FILE_HINT_HEIGHT) * 0.5));
        let hint = if right {
            hint.left(px(strip.x + strip.width + FILE_HINT_GAP)).max_w(px(room_right.max(1.0)))
        } else {
            hint.right(px(size.0 - strip.x + FILE_HINT_GAP))
                .max_w(px(room_left.max(1.0)))
                .justify_end()
        };
        let hint = hint
            .h(px(FILE_HINT_HEIGHT))
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(11.0))
            .text_color(color(&t.text_muted))
            .child(
                div()
                    .h(px(19.0))
                    .max_w(px(220.0))
                    .px(px(7.0))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.0))
                    .rounded_full()
                    .border_1()
                    .border_color(color(&t.border))
                    .text_color(color(&t.text_secondary))
                    .children(self.icon(icon, 11.0))
                    .child(div().min_w_0().truncate().child(target)),
            )
            .child(div().min_w_0().truncate().child(text))
            .into_any_element();
        Some(hint)
    }
}

trait ClampTo {
    fn clamp_to(self, size: (f32, f32)) -> Self;
}

impl ClampTo for Rect {
    fn clamp_to(self, size: (f32, f32)) -> Self {
        clamp_rect(self, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docked_browser_prefers_the_right_and_turns_left_at_the_edge() {
        let size = (1600.0, 900.0);
        let root = Rect { x: 200.0, y: 160.0, width: 240.0, height: 300.0 };
        let one = dock_area(root, 1, size);
        assert_eq!((one.x, one.y, one.width), (root.x + root.width + GAP, 160.0, SINGLE_DOCK_WIDTH));
        assert_eq!(one.height, (900.0 - EDGE - 160.0_f32).min(DOCK_MAX_HEIGHT));
        assert_eq!(dock_area(Rect { y: 300.0, ..root }, 1, size).height, 900.0 - EDGE - 300.0);
        let two = dock_area(root, 2, size);
        assert_eq!(two.width, SPLIT_DOCK_WIDTH);
        assert!(split_fits(root, size));
        // A root near the right edge docks the browser on its left.
        let right = Rect { x: 1300.0, ..root };
        let left = dock_area(right, 1, size);
        assert!(left.x + left.width <= right.x - GAP + 0.5);
        assert!(left.width >= PANEL_MIN_WIDTH);
        // A cramped viewport keeps one panel and refuses a split.
        let small = (900.0, 640.0);
        assert!(!split_fits(Rect { x: 12.0, ..root }, small));
        assert_eq!(dock_area(root, 1, (1600.0, 2000.0)).height, DOCK_MAX_HEIGHT);
    }

    #[test]
    fn only_real_directories_open_in_the_docked_browser() {
        assert_eq!(browse_path(&json!({"role":"file","payload":{"path":"D:\\项目","isDirectory":true}})).as_deref(), Some("D:\\项目"));
        assert_eq!(browse_path(&json!({"role":"file","payload":{"path":"D:\\a.txt","isDirectory":false}})), None);
        assert_eq!(browse_path(&json!({"role":"file","payload":{"path":"D:\\unknown"}})), None);
        assert_eq!(browse_path(&json!({"role":"opened-folder","payload":{"path":"C:\\Users","windowHandle":42}})).as_deref(), Some("C:\\Users"));
        assert_eq!(browse_path(&json!({"role":"opened-folder","payload":{"path":"shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"}})), None);
        assert_eq!(browse_path(&json!({"role":"recent-files","payload":{"path":"D:\\"}})), None);
    }
}

/// A disposable native workspace using the same fixed data as the design stage.
#[cfg(all(windows, debug_assertions))]
pub fn run_visual_fixture() -> anyhow::Result<()> {
    use anyhow::{anyhow, Context as _};
    use gpui::{size, Timer, WindowBounds};
    use std::{fs, path::PathBuf};
    let args = std::env::args().collect::<Vec<_>>();
    let argument = |name: &str| args.windows(2).find(|a| a[0] == name).map(|a| a[1].clone());
    let output = PathBuf::from(
        argument("--output").context("--output must name a new directory in clip/.tmp")?,
    );
    let run = super::style_fixture::output_directory(&output)?;
    let dark = argument("--theme").as_deref() != Some("light");
    let width = argument("--width")
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(1180.0)
        .clamp(640.0, 1800.0);
    let height = argument("--height")
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(784.0)
        .clamp(604.0, 1100.0);
    let duration = argument("--exit-after-ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(12000)
        .clamp(3000, 180000);
    let interactive = args.iter().any(|v| v == "--interactive");
    let smoke = args.iter().any(|v| v == "--smoke");
    let split = args.iter().any(|v| v == "--split");
    let menu_only = args.iter().any(|v| v == "--menu-only");
    cliphost_core::force_headless(true);
    cliphost_core::i18n::set_language(cliphost_core::i18n::LanguageMode::Chinese);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();
    let host = Arc::new(
        HostState::open_isolated_visual(
            cliphost_core::AppPaths::from_data_root(run.join("data")),
            29065,
        )
        .map_err(|e| anyhow!(e))?,
    );
    let tokens = DesignTokens::from_json(crate::preview::TOKENS_JSON).map_err(|e| anyhow!(e))?;
    gpui::Application::new().with_assets(crate::DesignAssets).run(move |cx| {
        let build_host = host.clone();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let handle = cx.open_window(WindowOptions { titlebar: None, focus: false, show: false,
            window_bounds: Some(WindowBounds::Windowed(bounds)), window_background: gpui::WindowBackgroundAppearance::Opaque,
            ..Default::default() }, move |window, cx| {
                window.set_window_title("File menu dock · isolated native fixture");
                cx.new(|cx| SuperMenuWindow::new(build_host, tokens, dark, cx))
            }).expect("create isolated workspace");
        handle.update(cx, |menu, window, cx| {
            let hwnd = platform::hwnd(window).expect("fixture HWND").0 as isize;
            menu.host.set_super_menu_hwnd(hwnd);
            menu.source_hwnd = hwnd as i64;
            menu.host.menu_create_session_ex("file", "", hwnd as i64, 0);
            menu.sync(menu.host.super_menu_state_detail(), cx);
            menu.browser_fixture = true;
            menu.change_category(Category::File, cx);
            platform::set_geometry(window, None, Some((f64::from(width), f64::from(height)))).expect("fixture geometry");
            platform::set_no_activate(window, !interactive).expect("fixture input style");
            platform::show_no_activate(window).expect("show fixture");
        }).expect("configure workspace fixture");
        let began = Instant::now();
        cx.spawn(async move |cx| {
            let mut opened = false;
            let mut captured = false;
            let capture_path = run.join("dock.png");
            let mut tested = false;
            let mut smoke_result = Value::Null;
            loop {
                Timer::after(Duration::from_millis(120)).await;
                let done = began.elapsed() >= Duration::from_millis(duration);
                let Ok(report) = handle.update(cx, |menu, window, cx| {
                    // Open after the root column has painted, as a ⇢ click would.
                    if !opened && menu_only && began.elapsed() >= Duration::from_millis(900) {
                        opened = true;
                    } else if !opened && began.elapsed() >= Duration::from_millis(900) {
                        opened = true;
                        menu.browse_from_menu("D:\\项目".into(), false, window, cx);
                        if split {
                            menu.browse_from_menu("C:\\Users\\demo\\Documents".into(), true, window, cx);
                        }
                        // Browsing makes the real menu activatable; the fixture stays passive.
                        let _ = platform::set_no_activate(window, !interactive);
                    }
                    if opened && !captured && !smoke && began.elapsed() >= Duration::from_millis(2600) {
                        captured = true;
                        if let Ok(hwnd) = platform::hwnd(window) {
                            if let Err(error) = super::style_fixture::capture(hwnd.0 as isize, &capture_path) {
                                eprintln!("capture failed: {error}");
                            }
                        }
                    }
                    if smoke && opened && !tested && menu.browser_panels.len() == if split { 2 } else { 1 } {
                        tested = true;
                        let first = menu.browser_panels[0].browser.clone();
                        menu.browse_from_menu("C:\\Users\\demo\\Documents".into(), true, window, cx);
                        let second = menu.browser_panels.last().unwrap().browser.clone();
                        let first_path = first.read(cx).current_path().to_string();
                        let second_path = second.read(cx).current_path().to_string();
                        let capped = { menu.browse_from_menu("D:\\素材".into(), true, window, cx); menu.browser_panels.len() };
                        menu.suspend_browsers(cx);
                        menu.prepare_browsers(window, cx).expect("resume fixture");
                        let preserved = first.read(cx).current_path() == first_path;
                        let last = menu.browser_panels.last().unwrap().id;
                        menu.close_browser(last, window, cx);
                        let one_left = menu.browser_panels.len() == 1;
                        let only = menu.browser_panels[0].id;
                        menu.close_browser(only, window, cx);
                        smoke_result = json!({"independentPaths": first_path != second_path, "atMostTwoPanels": capped == MAX_BROWSER_PANELS,
                            "retainedAfterSuspend": preserved, "closeOneKeepsOther": one_left, "closingAllReturnsToMenu": !menu.browsing(),
                            "rootColumnKept": !menu.columns.is_empty()});
                        menu.browse_from_menu("D:\\项目".into(), false, window, cx);
                    }
                    menu.maintain_browser_geometry(window, cx);
                    json!({"pid":std::process::id(),"hwnd":platform::hwnd(window).ok().map(|h| h.0 as isize),
                        "clientWidth":f32::from(window.viewport_size().width),"clientHeight":f32::from(window.viewport_size().height),
                        "root":menu.column_rects.borrow().get(&0).map(|r| json!([r.x, r.y, r.width, r.height])),
                        "active":menu.browser_active,"panels":menu.browser_panels.iter().map(|p| json!({"id":p.id,
                            "browser":p.browser.read(cx).diagnostics(cx)})).collect::<Vec<_>>(),
                        "nativeError":platform::take_error(window).ok().flatten(),"smoke":smoke_result,"syntheticFilesystem":true,"isolatedCore":true})
                }) else { break; };
                let _ = fs::write(run.join("report.json"), serde_json::to_vec_pretty(&report).unwrap());
                if done { let _ = cx.update(|cx| cx.quit()); break; }
            }
        }).detach();
    });
    Ok(())
}
