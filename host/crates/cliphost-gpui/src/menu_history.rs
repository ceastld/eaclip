//! History-specific menu, retaining the generic popup's one no-activate HWND.
use super::{color, ContextMenuPopup, MenuAction, MenuEntry, SelectHandler};
use crate::{
    motion, platform,
    preview::{DesignTokens, ThemeTokens},
    smooth_wheel::SmoothWheelExt,
};
use cliphost_core::{i18n::tr, window_place::overlay_work_area_from_point, HostState};
use gpui::{
    canvas, div, prelude::*, px, relative, AnimationExt, AnyElement, App, Context, FocusHandle,
    KeyDownEvent, MouseButton, ScrollHandle, SharedString, Task, Timer, Window, WindowHandle,
    WindowOptions,
};
use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};

#[path = "menu_history_model.rs"]
mod model;
use model::{contains, shadow, Groups, Layout, Rect};

#[cfg(all(windows, debug_assertions))]
#[path = "menu_history_fixture.rs"]
pub mod fixture;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuSelection {
    Action(&'static str),
    LanDevice(String),
    Dismiss,
}
pub type HistorySelectHandler = Rc<dyn Fn(u64, MenuSelection, &mut App)>;

#[derive(Clone, Debug)]
pub struct HistoryMenuContext {
    pub title: String,
    pub kind_label: String,
    pub icon: String,
    pub count: usize,
}
#[derive(Clone, Debug)]
pub struct HistoryMenuPeer {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub online: bool,
    pub paired: bool,
}

struct HistoryTooltip {
    text: String,
    theme: ThemeTokens,
}
impl Render for HistoryTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(300.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(color(&self.theme.border))
            .bg(color(&self.theme.surface))
            .text_size(px(12.0))
            .text_color(color(&self.theme.text))
            .child(self.text.clone())
    }
}
fn history_tooltip(
    text: String,
    theme: ThemeTokens,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    move |_, cx| {
        cx.new(|_| HistoryTooltip {
            text: text.clone(),
            theme: theme.clone(),
        })
        .into()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Target {
    Action(&'static str),
    LanArrow,
    AllDevices,
    Peer(String),
}

pub(super) struct HistoryMenu {
    host: HostState,
    context: HistoryMenuContext,
    peers: Vec<HistoryMenuPeer>,
    groups: Groups,
    layout: Layout,
    focus: FocusHandle,
    focus_epoch: u64,
    active: Option<Target>,
    hovered: Option<Target>,
    devices: bool,
    device_generation: u64,
    open_task: Option<Task<()>>,
    outside_since: Option<Instant>,
    keyboard_pointer: Option<(i32, i32)>,
    finished: bool,
    opening_foreground: isize,
    row_bounds: Rc<RefCell<BTreeMap<String, Rect>>>,
    card_regions: Rc<RefCell<BTreeMap<usize, Rect>>>,
    previous_regions: Rc<RefCell<Vec<Rect>>>,
    root_scroll: ScrollHandle,
    root_scroll_y: f32,
    device_scroll: ScrollHandle,
    tweens: motion::Tweens<Target>,
    on_select: HistorySelectHandler,
}

impl ContextMenuPopup {
    #[allow(clippy::too_many_arguments)]
    pub fn show_history(
        existing: &mut Option<WindowHandle<Self>>,
        host: &HostState,
        owner_hwnd: isize,
        lease: u64,
        point: (i32, i32),
        entries: Vec<MenuEntry>,
        context: HistoryMenuContext,
        peers: Vec<HistoryMenuPeer>,
        tokens: DesignTokens,
        dark: bool,
        on_select: HistorySelectHandler,
        cx: &mut App,
    ) -> Result<(), String> {
        if context.count == 0 || !host.native_context_menu_is_current(lease) {
            return Err(tr("菜单已关闭。", "The menu has closed.").into());
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
        let groups = Groups::new(entries);
        let peers: Vec<_> = peers.into_iter().filter(|peer| peer.paired).collect();
        let layout = Layout::place(
            point,
            work,
            area.dpi_scale_y,
            groups.height(model::ROOT_WIDTH, context.count > 1),
            peers.len(),
        );
        Self::show_history_layout(
            existing, host, owner_hwnd, lease, groups, context, peers, layout, tokens, dark,
            on_select, cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn show_history_layout(
        existing: &mut Option<WindowHandle<Self>>,
        host: &HostState,
        owner_hwnd: isize,
        lease: u64,
        groups: Groups,
        context: HistoryMenuContext,
        peers: Vec<HistoryMenuPeer>,
        layout: Layout,
        tokens: DesignTokens,
        dark: bool,
        on_select: HistorySelectHandler,
        cx: &mut App,
    ) -> Result<(), String> {
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
                        window.set_window_title(tr("ClipHost GPUI · 菜单", "ClipHost GPUI · Menu"));
                        cx.new(|_| Self {
                            host: Some(host.clone()),
                            tokens: tokens.clone(),
                            dark,
                            lease,
                            blocks: Vec::new(),
                            card_width: layout.root.2,
                            capped: false,
                            on_select: Rc::new(|_: u64, _: &'static str, _: &mut App| {})
                                as SelectHandler,
                            history: None,
                        })
                    },
                )
                .map_err(|error| error.to_string())?;
            *existing = Some(handle);
            handle
        };
        handle
            .update(cx, |this, window, cx| {
                this.host = Some(host.clone());
                this.tokens = tokens;
                this.dark = dark;
                this.lease = lease;
                let active = groups
                    .primary
                    .first()
                    .or_else(|| groups.actions().next())
                    .map(|action| Target::Action(action.key));
                this.history = Some(HistoryMenu {
                    host: host.clone(),
                    context,
                    peers,
                    groups,
                    layout,
                    focus: cx.focus_handle(),
                    focus_epoch: 0,
                    active,
                    hovered: None,
                    devices: false,
                    device_generation: 0,
                    open_task: None,
                    outside_since: None,
                    keyboard_pointer: None,
                    finished: false,
                    opening_foreground: cliphost_core::window_menu::foreground_hwnd(),
                    row_bounds: Rc::new(RefCell::new(BTreeMap::new())),
                    card_regions: Rc::new(RefCell::new(BTreeMap::new())),
                    previous_regions: Rc::new(RefCell::new(Vec::new())),
                    root_scroll: ScrollHandle::new(),
                    root_scroll_y: 0.0,
                    device_scroll: ScrollHandle::new(),
                    tweens: motion::Tweens::default(),
                    on_select,
                });
                window.focus(&this.history.as_ref().unwrap().focus);
                #[cfg(windows)]
                {
                    let hwnd = platform::hwnd(window)?.0 as isize;
                    if !host.set_native_context_menu_hwnd(lease, hwnd) {
                        return Err(tr("菜单已关闭。", "The menu has closed.").into());
                    }
                    if !host.register_native_context_menu_keyboard(
                        lease,
                        hwnd,
                        cliphost_core::NativeContextMenuKeyboardScope {
                            presentation: lease,
                            focus: 0,
                        },
                    ) {
                        return Err(tr("菜单已关闭。", "The menu has closed.").into());
                    }
                }
                this.watch_history(window, cx);
                cx.notify();
                // Set the hidden HWND's monitor before converting the DIP region.
                platform::set_geometry(window, Some((layout.window.0, layout.window.1)), None)?;
                platform::overlay_set_regions(window, vec![shadow(layout.root)])?;
                platform::overlay_show(window, owner_hwnd, layout.window)
            })
            .map_err(|error| error.to_string())?
    }

    /// The owner drains the core's lease-bound no-activate keyboard queue.
    pub fn handle_history_key(
        handle: WindowHandle<Self>,
        lease: u64,
        key: &str,
        cx: &mut App,
    ) -> bool {
        handle
            .update(cx, |this, window, cx| {
                if this.lease != lease || !this.history_current() {
                    return false;
                }
                this.history_key(key, window, cx)
            })
            .unwrap_or(false)
    }

    fn history_current(&self) -> bool {
        self.lease != 0
            && self.history.as_ref().is_some_and(|history| {
                !history.finished && history.host.native_context_menu_is_current(self.lease)
            })
    }

    fn select_history(&mut self, lease: u64, selection: MenuSelection, cx: &mut Context<Self>) {
        if self.lease != lease || !self.history_current() {
            return;
        }
        let history = self.history.as_mut().unwrap();
        if matches!(selection, MenuSelection::Action("lan-share")) && history.peers.is_empty() {
            return;
        }
        history.finished = true;
        history.open_task = None;
        let host = history.host.clone();
        let select = history.on_select.clone();
        cx.defer(move |cx| {
            if host.native_context_menu_is_current(lease) {
                select(lease, selection, cx);
            }
        });
    }

    fn history_pointer_focus(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.history_current() {
            return;
        }
        let history = self.history.as_mut().unwrap();
        if history.active.as_ref() == Some(&target) && history.hovered.as_ref() == Some(&target) {
            return;
        }
        history.active = Some(target.clone());
        history.hovered = Some(target);
        history.keyboard_pointer = None;
        // Pointer focus invalidates keys queued for the old target. Navigation
        // keys themselves do not advance this scope, preserving a drained batch.
        self.history_advance_scope(window);
        cx.notify();
    }

    fn history_advance_scope(&mut self, window: &mut Window) {
        let Some(history) = self.history.as_mut() else {
            return;
        };
        history.focus_epoch = history.focus_epoch.wrapping_add(1);
        #[cfg(windows)]
        if let Ok(hwnd) = platform::hwnd(window) {
            history.host.register_native_context_menu_keyboard(
                self.lease,
                hwnd.0 as isize,
                cliphost_core::NativeContextMenuKeyboardScope {
                    presentation: self.lease,
                    focus: history.focus_epoch,
                },
            );
        }
    }

    fn history_devices(
        &mut self,
        open: bool,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(history) = self.history.as_mut() else {
            return;
        };
        history.open_task = None;
        history.outside_since = None;
        let changed = history.devices != open;
        if changed {
            history.device_generation = history.device_generation.wrapping_add(1);
        }
        history.devices = open;
        if open {
            history.root_scroll_y = f32::from(history.root_scroll.offset().y);
        }
        history.keyboard_pointer = if keyboard && open {
            platform::cursor_position()
        } else {
            None
        };
        if keyboard && open {
            history.active = Some(Target::AllDevices);
        }
        if !open && matches!(history.active, Some(Target::AllDevices | Target::Peer(_))) {
            history.active = Some(Target::LanArrow);
        }
        history.hovered = None;
        history
            .device_scroll
            .set_offset(gpui::point(px(0.0), px(0.0)));
        if changed && !keyboard {
            self.history_advance_scope(window);
        }
        cx.notify();
    }

    fn history_lan_hover(
        &mut self,
        lease: u64,
        entered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lease != lease || !self.history_current() {
            return;
        }
        let history = self.history.as_mut().unwrap();
        history.open_task = None;
        if !entered {
            return;
        }
        history.outside_since = None;
        if history.devices {
            return;
        }
        history.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            Timer::after(motion::HISTORY_MENU_DEVICE_OPEN).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.lease == lease && this.history_current() {
                    this.history_devices(true, false, window, cx);
                }
            });
        }));
    }

    fn watch_history(&self, window: &mut Window, cx: &mut Context<Self>) {
        let lease = self.lease;
        cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(40)).await;
            let keep = this
                .update_in(cx, |this, window, cx| {
                    if this.lease != lease || !this.history_current() {
                        return false;
                    }
                    #[cfg(windows)]
                    if let Ok(hwnd) = platform::hwnd(window) {
                        let history = this.history.as_ref().unwrap();
                        if !history.host.register_native_context_menu_keyboard(
                            lease,
                            hwnd.0 as isize,
                            cliphost_core::NativeContextMenuKeyboardScope {
                                presentation: lease,
                                focus: history.focus_epoch,
                            },
                        ) {
                            this.select_history(lease, MenuSelection::Dismiss, cx);
                            return false;
                        }
                        let foreground = cliphost_core::window_menu::foreground_hwnd();
                        if foreground != 0
                            && foreground != hwnd.0 as isize
                            && foreground != history.host.host_hwnd()
                            && foreground != history.opening_foreground
                        {
                            this.select_history(lease, MenuSelection::Dismiss, cx);
                            return false;
                        }
                    }
                    let history = this.history.as_mut().unwrap();
                    let scroll_y = f32::from(history.root_scroll.offset().y);
                    let root_scrolled = (scroll_y - history.root_scroll_y).abs() > 0.1;
                    history.root_scroll_y = scroll_y;
                    if !history.devices {
                        return true;
                    }
                    if root_scrolled {
                        this.history_devices(false, false, window, cx);
                        return true;
                    }
                    let cursor = platform::cursor_position();
                    // A keyboard-opened submenu has no pointer-leave event.
                    // Keep its keyboard path until the pointer actually moves.
                    if history.keyboard_pointer.is_some() && history.keyboard_pointer == cursor {
                        history.outside_since = None;
                        return true;
                    }
                    history.keyboard_pointer = None;
                    let inside = cursor.is_some_and(|(x, y)| {
                        if !history.host.native_context_menu_pointer_inside(lease, x, y) {
                            return false;
                        }
                        let (left, top, scale) = {
                            #[cfg(windows)]
                            {
                                if let Ok(geometry) = platform::geometry(window) {
                                    (geometry.left, geometry.top, geometry.scale)
                                } else {
                                    (
                                        history.layout.window.0,
                                        history.layout.window.1,
                                        history.layout.scale,
                                    )
                                }
                            }
                            #[cfg(not(windows))]
                            {
                                (
                                    history.layout.window.0,
                                    history.layout.window.1,
                                    history.layout.scale,
                                )
                            }
                        };
                        let (x, y) = (
                            (f64::from(x - left) / scale) as f32,
                            (f64::from(y - top) / scale) as f32,
                        );
                        let row = history.row_bounds.borrow().get("lan-row").copied();
                        row.is_some_and(|rect| contains(rect, x, y))
                            || contains(
                                history
                                    .layout
                                    .devices(row.map_or(history.layout.root.1, |rect| rect.1)),
                                x,
                                y,
                            )
                    });
                    if inside {
                        history.outside_since = None;
                    } else if history
                        .outside_since
                        .get_or_insert_with(Instant::now)
                        .elapsed()
                        >= motion::HISTORY_MENU_DEVICE_LEAVE
                    {
                        this.history_devices(false, false, window, cx);
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

    fn history_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let lease = self.lease;
        if key == "tab" {
            self.select_history(lease, MenuSelection::Dismiss, cx);
            return true;
        }
        if key == "escape" {
            if self.history.as_ref().unwrap().devices {
                self.history_devices(false, true, window, cx);
            } else {
                self.select_history(lease, MenuSelection::Dismiss, cx);
            }
            return true;
        }
        let shortcut = match key {
            "ctrl-c" => Some("copy-only"),
            "ctrl-p" => Some("pin-toggle"),
            "delete" => Some("delete"),
            _ => None,
        };
        if let Some(key) = shortcut {
            if self
                .history
                .as_ref()
                .unwrap()
                .groups
                .actions()
                .any(|action| action.key == key)
            {
                self.select_history(lease, MenuSelection::Action(key), cx);
            }
            return true;
        }
        let history = self.history.as_ref().unwrap();
        if key == "left" && history.devices {
            self.history_devices(false, true, window, cx);
            return true;
        }
        if key == "right"
            && matches!(
                history.active,
                Some(Target::Action("lan-share") | Target::LanArrow)
            )
        {
            self.history_devices(true, true, window, cx);
            return true;
        }
        if matches!(key, "enter" | "space") {
            match history.active.clone() {
                Some(Target::LanArrow) => self.history_devices(true, true, window, cx),
                Some(Target::Action(key)) => {
                    self.select_history(lease, MenuSelection::Action(key), cx)
                }
                Some(Target::AllDevices) => {
                    self.select_history(lease, MenuSelection::Action("lan-share"), cx)
                }
                Some(Target::Peer(id)) => {
                    self.select_history(lease, MenuSelection::LanDevice(id), cx)
                }
                None => {}
            }
            return true;
        }
        if !matches!(key, "up" | "down" | "left" | "right" | "home" | "end") {
            return false;
        }
        let in_devices =
            history.devices && matches!(history.active, Some(Target::AllDevices | Target::Peer(_)));
        let targets: Vec<_> = if in_devices {
            std::iter::once(Target::AllDevices)
                .chain(
                    history
                        .peers
                        .iter()
                        .map(|peer| Target::Peer(peer.id.clone())),
                )
                .collect()
        } else {
            history
                .groups
                .actions()
                .flat_map(|action| {
                    let mut targets = vec![Target::Action(action.key)];
                    if action.key == "lan-share" {
                        targets.push(Target::LanArrow);
                    }
                    targets
                })
                .collect()
        };
        if targets.is_empty() {
            return true;
        }
        let position = history
            .active
            .as_ref()
            .and_then(|target| targets.iter().position(|item| item == target))
            .unwrap_or(0);
        let next = match key {
            "home" => 0,
            "end" => targets.len() - 1,
            "up" | "left" => (position + targets.len() - 1) % targets.len(),
            _ => (position + 1) % targets.len(),
        };
        self.history.as_mut().unwrap().active = Some(targets[next].clone());
        if in_devices {
            let history = self.history.as_mut().unwrap();
            history.keyboard_pointer = platform::cursor_position();
            history.outside_since = None;
        }
        self.history.as_ref().unwrap().focus.focus(window);
        self.history_reveal_active();
        cx.notify();
        true
    }

    fn history_reveal_active(&self) {
        let Some(history) = self.history.as_ref() else {
            return;
        };
        let Some(target) = history.active.as_ref() else {
            return;
        };
        let Some(rect) = history
            .row_bounds
            .borrow()
            .get(&format!("{target:?}"))
            .copied()
        else {
            return;
        };
        let (viewport, scroll) = if matches!(target, Target::AllDevices | Target::Peer(_)) {
            let top = history
                .row_bounds
                .borrow()
                .get("lan-row")
                .map_or(history.layout.root.1, |row| row.1);
            (history.layout.devices(top), &history.device_scroll)
        } else {
            (history.layout.root, &history.root_scroll)
        };
        let delta = if rect.1 < viewport.1 + 5.0 {
            viewport.1 + 5.0 - rect.1
        } else if rect.1 + rect.3 > viewport.1 + viewport.3 - 5.0 {
            viewport.1 + viewport.3 - 5.0 - rect.1 - rect.3
        } else {
            0.0
        };
        if delta != 0.0 {
            let offset = scroll.offset();
            scroll.set_offset(gpui::point(offset.x, offset.y + px(delta)));
        }
    }

    fn history_button(
        &mut self,
        target: Target,
        tooltip: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let lease = self.lease;
        let theme = self.tokens.theme(self.dark).clone();
        let history = self.history.as_mut().unwrap();
        let active =
            history.active.as_ref() == Some(&target) || history.hovered.as_ref() == Some(&target);
        let now = Instant::now();
        let progress = history.tweens.value(
            target.clone(),
            if active { 1.0 } else { 0.0 },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if history.tweens.any_active(now) {
            window.request_animation_frame();
        }
        let hover_target = target.clone();
        let move_target = target.clone();
        let click_target = target.clone();
        let child_generation = matches!(target, Target::AllDevices | Target::Peer(_))
            .then_some(history.device_generation);
        let probe = history.row_bounds.clone();
        let probe_id = format!("{target:?}");
        let hover = if matches!(&target, Target::Action("delete")) {
            color(&theme.error).opacity(0.1)
        } else {
            color(&theme.hover)
        };
        div()
            .id(SharedString::from(format!(
                "history-menu-{lease}-{child_generation:?}-{target:?}"
            )))
            .relative()
            .flex()
            .items_center()
            .flex_none()
            .min_w_0()
            .rounded(px(5.0))
            .cursor_pointer()
            .gap(px(8.0))
            .bg(motion::mix_hsla(
                color(&theme.surface).opacity(0.0),
                hover,
                progress,
            ))
            .tooltip(history_tooltip(tooltip, theme))
            .on_hover(cx.listener(move |this, entered: &bool, window, cx| {
                if this.lease != lease {
                    return;
                }
                if child_generation.is_some_and(|generation| {
                    this.history.as_ref().is_none_or(|history| {
                        !history.devices || history.device_generation != generation
                    })
                }) {
                    return;
                }
                if *entered {
                    this.history_pointer_focus(hover_target.clone(), window, cx);
                } else if let Some(history) = this.history.as_mut() {
                    if history.hovered.as_ref() == Some(&hover_target) {
                        history.hovered = None;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_move(cx.listener(move |this, _, window, cx| {
                if this.lease != lease
                    || child_generation.is_some_and(|generation| {
                        this.history.as_ref().is_none_or(|history| {
                            !history.devices || history.device_generation != generation
                        })
                    })
                {
                    return;
                }
                this.history_pointer_focus(move_target.clone(), window, cx);
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                if this.lease != lease || !this.history_current() {
                    return;
                }
                if child_generation.is_some_and(|generation| {
                    this.history.as_ref().is_none_or(|history| {
                        !history.devices || history.device_generation != generation
                    })
                }) {
                    return;
                }
                match &click_target {
                    Target::LanArrow => {
                        this.history_devices(true, true, window, cx);
                        this.history_advance_scope(window);
                    }
                    Target::Action(key) => {
                        this.select_history(lease, MenuSelection::Action(key), cx)
                    }
                    Target::AllDevices => {
                        this.select_history(lease, MenuSelection::Action("lan-share"), cx)
                    }
                    Target::Peer(id) => {
                        this.select_history(lease, MenuSelection::LanDevice(id.clone()), cx)
                    }
                }
            }))
            .child(
                canvas(
                    move |bounds, _, _| {
                        probe.borrow_mut().insert(
                            probe_id.clone(),
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
                .size_full(),
            )
    }

    fn history_action(
        &mut self,
        action: &MenuAction,
        primary: bool,
        scope: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.theme(self.dark).clone();
        let count = self.history.as_ref().unwrap().context.count;
        let label = if action.key == "lan-share" {
            tr("发送到局域网", "Send over LAN").to_owned()
        } else {
            action.label.clone()
        };
        let tip = if action.key == "lan-share" {
            tr("单击发送到所有已配对设备", "Send to all paired devices").to_owned()
        } else if primary {
            format!("{} · {} {}", label, count, tr("项", "items"))
        } else if let Some(scope) = scope {
            format!("{label} · {scope}")
        } else {
            label.clone()
        };
        let tint = color(if action.danger {
            &theme.error
        } else {
            &theme.text
        });
        let icon_tint = color(if action.danger {
            &theme.error
        } else {
            &theme.text_secondary
        });
        let icon = match action.key {
            "lan-share" => Some("desktop"),
            "distinct" => Some("filter"),
            "split-lines" => Some("list"),
            "split-files" => Some("file"),
            "fifo-paste" => Some("list"),
            _ => action.icon(),
        };
        let mut button = self
            .history_button(Target::Action(action.key), tip, window, cx)
            .text_color(tint)
            .line_height(px(18.0))
            .text_size(px(12.0))
            .children(icon.and_then(|key| self.icon(key, 16.0, icon_tint)));
        if primary {
            button = button.h(px(32.0)).flex_1().justify_center().px(px(6.0));
        } else {
            button = button
                .w_full()
                .h_full()
                .px(px(7.0))
                .py(px(5.0))
                .child(div().flex_1().min_w_0().child(label));
        }
        if action.key == "lan-share" && self.history.as_ref().unwrap().peers.is_empty() {
            button = button.opacity(0.4).cursor_default();
        }
        if let Some(scope) = scope {
            button = button.child(
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(color(&theme.text_muted))
                    .child(scope.to_owned()),
            );
        }
        if primary && action.key == "pin-toggle" && action.is_unpin() {
            button = button.child(
                div()
                    .absolute()
                    .right(px(5.0))
                    .top(px(4.0))
                    .size(px(4.0))
                    .rounded_full()
                    .bg(color(&theme.text_secondary)),
            );
        }
        if primary && action.key == "delete" {
            button = button.child(
                div()
                    .absolute()
                    .left(px(-2.0))
                    .top(px(7.0))
                    .bottom(px(7.0))
                    .w(px(1.0))
                    .bg(color(&theme.border)),
            );
        }
        button.into_any_element()
    }

    fn history_grid(
        &mut self,
        actions: &[MenuAction],
        location: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let width = self.history.as_ref().unwrap().layout.root.2 - 10.0;
        let theme = self.tokens.theme(self.dark).clone();
        let multiple = self.history.as_ref().unwrap().context.count > 1;
        let mut section = div()
            .flex()
            .flex_col()
            .flex_none()
            .py(px(3.0))
            .gap(px(2.0))
            .when(location, |section| {
                section.border_t_1().border_color(color(&theme.border))
            });
        for pair in actions.chunks(2) {
            let mut row = div()
                .flex()
                .flex_none()
                .gap(px(2.0))
                .h(px(model::secondary_height(
                    pair,
                    width,
                    location && multiple,
                )));
            for action in pair {
                let button = self.history_action(
                    action,
                    false,
                    if location && multiple {
                        Some(tr("首项", "First"))
                    } else {
                        None
                    },
                    window,
                    cx,
                );
                row = row.child(div().flex_1().min_w_0().h_full().child(button));
            }
            section = section.child(row);
        }
        section.into_any_element()
    }

    fn history_lan_row(
        &mut self,
        action: &MenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lease = self.lease;
        let probe = self.history.as_ref().unwrap().row_bounds.clone();
        let theme = self.tokens.theme(self.dark).clone();
        let expanded = self.history.as_ref().unwrap().devices;
        let row = self.history_action(action, false, Some(tr("全部", "All")), window, cx);
        let arrow = self
            .history_button(
                Target::LanArrow,
                tr("选择接收设备", "Choose a receiving device").into(),
                window,
                cx,
            )
            .w(px(26.0))
            .h_full()
            .justify_center()
            .rounded_tl(px(0.0))
            .rounded_bl(px(0.0))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(7.0))
                    .bottom(px(7.0))
                    .w(px(1.0))
                    .bg(color(&theme.border)),
            )
            .children(self.icon("chevron-right", 14.0, color(&theme.text_muted)));
        div()
            .id(SharedString::from(format!("history-menu-lan-row-{lease}")))
            .relative()
            .flex()
            .h(px(30.0))
            .flex_none()
            .when(expanded, |row| row.bg(color(&theme.hover)).rounded(px(5.0)))
            .on_hover(cx.listener(move |this, entered: &bool, window, cx| {
                this.history_lan_hover(lease, *entered, window, cx)
            }))
            .child(div().flex_1().min_w_0().h_full().child(row))
            .child(arrow)
            .child(
                canvas(
                    move |bounds, _, _| {
                        probe.borrow_mut().insert(
                            "lan-row".into(),
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
                .size_full(),
            )
            .into_any_element()
    }

    fn history_card_probe(&self, index: usize) -> AnyElement {
        let regions = self.history.as_ref().unwrap().card_regions.clone();
        canvas(
            move |bounds, _, _| {
                regions.borrow_mut().insert(
                    index,
                    shadow((
                        f32::from(bounds.left()),
                        f32::from(bounds.top()),
                        f32::from(bounds.size.width),
                        f32::from(bounds.size.height),
                    )),
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

    pub(super) fn render_history(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let render_lease = self.lease;
        let theme = self.tokens.theme(self.dark).clone();
        let history = self.history.as_ref().unwrap();
        let layout = history.layout;
        let context = history.context.clone();
        let groups = history.groups.clone();
        let focus = history.focus.clone();
        let root_scroll = history.root_scroll.clone();
        history.card_regions.borrow_mut().clear();
        let root_tip = context.title.clone();
        let context_icon = div().size(px(16.0)).flex_none().children(self.icon(
            &context.icon,
            16.0,
            color(&theme.text_secondary),
        ));
        let context_row = rsx! {
            <div id={"history-menu-context"} h={px(30.0)} flex_none flex items_center gap={px(7.0)} px={px(5.0)} pb={px(6.0)}>
                {context_icon}
                <div flex_1 min_w_0 flex items_center gap={px(6.0)}>
                    <div flex_none text_size={px(11.0)}>{context.kind_label}</div>
                    <div flex_1 min_w_0 truncate text_size={px(10.0)} text_color={color(&theme.text_muted)}>
                        {context.title}
                    </div>
                </div>
                <div flex_none text_size={px(10.0)} text_color={color(&theme.text_muted)}>
                    {format!("{} {}", context.count, tr("项", "items"))}
                </div>
            </div>
        }.tooltip(history_tooltip(root_tip, theme.clone()));
        let mut contents = div().flex().flex_col().min_h_0().child(context_row);
        if !groups.primary.is_empty() {
            let mut strip = div()
                .h(px(38.0))
                .flex_none()
                .flex()
                .p(px(2.0))
                .gap(px(2.0))
                .border_1()
                .border_color(color(&theme.border))
                .rounded(px(6.0))
                .bg(color(&theme.secondary));
            for action in &groups.primary {
                strip = strip.child(self.history_action(action, true, None, window, cx));
            }
            contents = contents.child(strip);
        }
        if !groups.paste.is_empty() {
            contents = contents.child(self.history_grid(&groups.paste, false, window, cx));
        }
        if !groups.location.is_empty() {
            contents = contents.child(self.history_grid(&groups.location, true, window, cx));
        }
        if !groups.rows.is_empty() {
            let mut rows = div()
                .flex_none()
                .flex()
                .flex_col()
                .py(px(3.0))
                .border_t_1()
                .border_color(color(&theme.border));
            for action in &groups.rows {
                rows = rows.child(if action.key == "lan-share" {
                    self.history_lan_row(action, window, cx)
                } else {
                    div()
                        .h(px(30.0))
                        .flex_none()
                        .child(self.history_action(action, false, None, window, cx))
                        .into_any_element()
                });
            }
            contents = contents.child(rows);
        }
        if !groups.global.is_empty() {
            let mut rows = div()
                .flex_none()
                .flex()
                .flex_col()
                .pt(px(3.0))
                .border_t_1()
                .border_color(color(&theme.border));
            for action in &groups.global {
                rows = rows.child(div().h(px(30.0)).flex_none().child(self.history_action(
                    action,
                    false,
                    Some(tr("全部历史", "All history")),
                    window,
                    cx,
                )));
            }
            contents = contents.child(rows);
        }
        let root_card = div()
            .id(SharedString::from(format!(
                "history-menu-card-{}",
                self.lease
            )))
            .absolute()
            .left(px(layout.root.0))
            .top(px(layout.root.1))
            .w(px(layout.root.2))
            .h(px(layout.root.3))
            .min_h_0()
            .p(px(4.0))
            .border_1()
            .border_color(color(&theme.border_secondary))
            .rounded(px(8.0))
            .bg(color(&theme.surface))
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("history-menu-root-scroll")
                    .w_full()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&root_scroll)
                    .smooth_wheel("history-menu-root-scroll", &root_scroll, cx)
                    .child(contents),
            )
            .child(self.history_card_probe(0))
            .with_animation(
                SharedString::from(format!("history-menu-enter-{}", self.lease)),
                motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                |card, value| card.opacity(value),
            );
        let mut root = div()
            .size_full()
            .relative()
            .track_focus(&focus)
            .bg(gpui::transparent_black())
            .font_family(self.tokens.typography.font_family.clone())
            .text_size(px(12.0))
            .line_height(relative(1.45))
            .text_color(color(&theme.text))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.select_history(render_lease, MenuSelection::Dismiss, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.select_history(render_lease, MenuSelection::Dismiss, cx);
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if this.lease != render_lease {
                    return;
                }
                if event.is_held && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    return;
                }
                let modifiers = &event.keystroke.modifiers;
                let key = if modifiers.control
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.platform
                {
                    format!("ctrl-{}", event.keystroke.key)
                } else if modifiers.modified() && !(event.keystroke.key == "tab" && modifiers.shift)
                {
                    return;
                } else {
                    event.keystroke.key.to_string()
                };
                if this.history_current() && this.history_key(&key, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(root_card);
        if self.history.as_ref().unwrap().devices {
            root = root.child(self.render_history_devices(window, cx));
        }
        let history = self.history.as_ref().unwrap();
        let (regions, previous) = (
            history.card_regions.clone(),
            history.previous_regions.clone(),
        );
        let (host, lease) = (history.host.clone(), self.lease);
        root.child(
            canvas(
                move |_, window, cx| {
                    let rectangles: Vec<_> = regions.borrow().values().copied().collect();
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
                        let guard_host = host.clone();
                        let _guard = platform::guard_condition_inherited(move || {
                            guard_host.native_context_menu_is_current(lease)
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
        .into_any_element()
    }

    fn render_history_devices(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.theme(self.dark).clone();
        let history = self.history.as_ref().unwrap();
        let row = history.row_bounds.borrow().get("lan-row").copied();
        let rect = history
            .layout
            .devices(row.map_or(history.layout.root.1, |row| row.1));
        let peers = history.peers.clone();
        let scroll = history.device_scroll.clone();
        let generation = history.device_generation;
        let lease = self.lease;
        let mut contents = div().flex().flex_col().min_h_0().child(
            div()
                .h(px(23.0))
                .flex_none()
                .flex()
                .items_center()
                .px(px(7.0))
                .text_size(px(10.0))
                .text_color(color(&theme.text_muted))
                .child(tr("发送到", "Send to"))
                .child(div().flex_1())
                .child(format!("{} {}", peers.len(), tr("台已配对", "paired"))),
        );
        let mut all = self
            .history_button(
                Target::AllDevices,
                tr("发送到所有已配对设备", "Send to all paired devices").into(),
                window,
                cx,
            )
            .w_full()
            .h(px(30.0))
            .px(px(7.0))
            .children(self.icon("all", 16.0, color(&theme.text_secondary)))
            .child(tr("所有设备", "All devices"));
        if peers.is_empty() {
            all = all.opacity(0.4).cursor_default();
        }
        contents = contents.child(all).child(
            div()
                .h(px(4.0))
                .flex_none()
                .mt(px(3.0))
                .border_t_1()
                .border_color(color(&theme.border)),
        );
        if peers.is_empty() {
            contents = contents.child(
                div()
                    .h(px(30.0))
                    .flex_none()
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(color(&theme.text_muted))
                    .child(tr("暂无已配对设备", "No paired devices")),
            );
        }
        for peer in peers {
            let tip = if peer.online {
                format!("{}「{}」", tr("发送到", "Send to "), peer.name)
            } else {
                format!(
                    "{}「{}」{}",
                    tr("加入", "Queue for "),
                    peer.name,
                    tr("的发送队列，上线后自动发送", "; sends when online")
                )
            };
            let button = self
                .history_button(Target::Peer(peer.id), tip, window, cx)
                .w_full()
                .h(px(30.0))
                .px(px(7.0))
                .children(self.icon(&peer.icon, 16.0, color(&theme.text_secondary)))
                .child(div().flex_1().min_w_0().truncate().child(peer.name))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_size(px(10.0))
                        .text_color(color(&theme.text_muted))
                        .child(div().size(px(4.0)).rounded_full().bg(if peer.online {
                            gpui::rgb(0x22c55e).into()
                        } else {
                            color(&theme.text_muted)
                        }))
                        .child(if peer.online {
                            tr("在线", "Online")
                        } else {
                            tr("离线·排队", "Offline · queue")
                        }),
                );
            contents = contents.child(button);
        }
        div()
            .id(SharedString::from(format!(
                "history-device-menu-{lease}-{generation}"
            )))
            .absolute()
            .left(px(rect.0))
            .top(px(rect.1))
            .w(px(rect.2))
            .h(px(rect.3))
            .min_h_0()
            .p(px(4.0))
            .border_1()
            .border_color(color(&theme.border_secondary))
            .rounded(px(8.0))
            .bg(color(&theme.surface))
            .shadow_lg()
            // The upper card owns every hit in an overlap, including blank/header
            // pixels. A click cannot fall through to the parent's All action.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("history-menu-device-scroll")
                    .w_full()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .smooth_wheel("history-menu-device-scroll", &scroll, cx)
                    .child(contents),
            )
            .child(self.history_card_probe(1))
            .with_animation(
                SharedString::from(format!("history-device-enter-{lease}-{generation}")),
                motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                |card, value| card.opacity(value),
            )
            .into_any_element()
    }
}
