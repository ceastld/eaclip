//! List context menu presentation. A retained no-activate popup owned by the
//! clipboard root shows entries; the core lease decides outside-click, Escape
//! and owner-hide dismissal, and only a still-current lease reports a choice.

use gpui_rsx::rsx;
use std::rc::Rc;
use crate::smooth_wheel::SmoothWheelExt;

use cliphost_core::{HostState, i18n::tr, window_place::overlay_work_area_from_point};
use gpui::{
    App, Context, Hsla, SharedString, Window, WindowHandle, WindowOptions, div, prelude::*, px,
    relative,
};
use crate::crisp_svg::svg;

use crate::{platform, preview::DesignTokens};

#[path = "menu_history.rs"]
mod history;
#[cfg(all(windows, debug_assertions))]
pub use history::fixture as history_fixture;
pub use history::{HistoryMenuContext, HistoryMenuPeer, HistorySelectHandler, MenuSelection};

/// Transparent margin around the menu card for its shadow, as in the Tauri popup.
const SHADOW_MARGIN: f32 = 8.0;
const MIN_WIDTH: f32 = 312.0;
const MAX_WIDTH: f32 = 420.0;

#[derive(Debug, Clone, PartialEq)]
pub struct MenuAction {
    pub key: &'static str,
    pub label: String,
    pub danger: bool,
    pub tile: bool,
}

impl MenuAction {
    pub fn row(key: &'static str, label: impl Into<String>) -> MenuEntry {
        MenuEntry::Action(Self {
            key,
            label: label.into(),
            danger: false,
            tile: false,
        })
    }

    pub fn tile(key: &'static str, label: impl Into<String>) -> MenuEntry {
        MenuEntry::Action(Self {
            key,
            label: label.into(),
            danger: false,
            tile: true,
        })
    }

    pub fn danger_tile(key: &'static str, label: impl Into<String>) -> MenuEntry {
        MenuEntry::Action(Self {
            key,
            label: label.into(),
            danger: true,
            tile: true,
        })
    }

    fn icon(&self) -> Option<&'static str> {
        if self.key == "pin-toggle" && self.is_unpin() {
            return Some("unpin-top");
        }
        Some(match self.key {
            "paste" | "fifo-paste" => "paste",
            "paste-plain-text" => "text",
            "paste-standard-text" => "code",
            "reverse-paste" => "sort",
            "copy-only" => "copy",
            "copy-as-gif" => "image",
            "favorite" => "star",
            // Item 置顶 matches the hover action; 贴图 keeps the pushpin.
            "pin-toggle" => "pin-top",
            "pin-snipaste" | "pin-pixpin" => "pin",
            "delete" => "delete",
            "edit" => "edit",
            "reveal-file" => "file",
            "reveal-folder" => "folder-open",
            "lan-share" => "cloud",
            "split-lines" | "split-files" | "distinct" => "broom",
            "scroll-top" => "level-up",
            // Favorites 「添加」 menu (DESIGN 2026-10-14) and panel add tiles.
            "add-clipboard" => "clipboard",
            "add-text" => "text",
            "add-batch-text" => "list",
            "add-files" => "file",
            "add-directories" => "folder-open",
            "add-link" => "link",
            "new-folder" | "new-root-folder" => "folder-closed",
            "add-action" | "add-action-hint" => "play",
            _ => return None,
        })
    }

    fn tile_label(&self, compact: bool) -> String {
        if compact {
            return self.label.clone();
        }
        match self.key {
            "paste" => tr("粘贴", "Paste").into(),
            "pin-toggle" if self.is_unpin() => tr("取消置顶", "Unpin").into(),
            "pin-toggle" => tr("置顶", "Pin").into(),
            "lan-share" => tr("局域网", "LAN").into(),
            "paste-plain-text" => tr("纯文本", "Plain text").into(),
            "paste-standard-text" => tr("规范粘贴", "Normalized").into(),
            "fifo-paste" => tr("连续粘贴", "Sequential paste").into(),
            _ => self.label.clone(),
        }
    }

    fn is_unpin(&self) -> bool {
        (self.label.contains("取消") && !self.label.contains('或'))
            || self.label.eq_ignore_ascii_case("unpin")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuEntry {
    Action(MenuAction),
    Separator,
    Label(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Tiles {
        items: Vec<MenuAction>,
        compact: bool,
    },
    Row(MenuAction),
    Separator,
    Label(String),
}

/// Port of the shared frontend layout: primary actions form an icon strip,
/// named sections become text-sized wrapping chips, the rest are rows.
fn layout(entries: Vec<MenuEntry>) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut tiles: Vec<MenuAction> = Vec::new();
    let mut named = false;
    let flush =
        |tiles: &mut Vec<MenuAction>, blocks: &mut Vec<Block>, named: bool| match tiles.len() {
            0 => {}
            1 => blocks.push(Block::Row(tiles.remove(0))),
            count => blocks.push(Block::Tiles {
                items: std::mem::take(tiles),
                compact: named || count <= 3,
            }),
        };
    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                flush(&mut tiles, &mut blocks, named);
                blocks.push(Block::Separator);
                named = false;
            }
            MenuEntry::Label(label) => {
                flush(&mut tiles, &mut blocks, named);
                blocks.push(Block::Label(label));
                named = true;
            }
            MenuEntry::Action(action) if action.tile => tiles.push(action),
            MenuEntry::Action(action) => {
                flush(&mut tiles, &mut blocks, named);
                blocks.push(Block::Row(action));
            }
        }
    }
    flush(&mut tiles, &mut blocks, named);
    blocks
}

fn chars(text: &str) -> f32 {
    text.chars().count() as f32
}

fn chip_width(action: &MenuAction, content_width: f32) -> f32 {
    (chars(&action.tile_label(true)) * 12.0 + 37.0).min(content_width)
}

fn tile_columns(count: usize) -> usize {
    if count > 5 { 3 } else { count }
}

/// Card size in DIP. Chips use these same widths when rendered, so wrapping
/// and the window height match without a measure-then-resize pass.
fn card_size(blocks: &[Block]) -> (f32, f32) {
    let mut width = MIN_WIDTH;
    for block in blocks {
        if let Block::Row(action) = block {
            width = width.max((chars(&action.label) * 12.0 + 58.0).min(MAX_WIDTH));
        }
    }
    let content_width = width - 12.0;
    let mut height = 12.0 + blocks.len().saturating_sub(1) as f32;
    for block in blocks {
        height += match block {
            Block::Separator => 7.0,
            Block::Label(_) => 19.0,
            Block::Tiles {
                items,
                compact: true,
            } => {
                let (mut rows, mut used) = (1.0, 0.0);
                for item in items {
                    let chip = chip_width(item, content_width);
                    if used > 0.0 && used + 4.0 + chip > content_width {
                        rows += 1.0;
                        used = 0.0;
                    }
                    used += if used > 0.0 { 4.0 } else { 0.0 } + chip;
                }
                rows * 26.0 + (rows - 1.0) * 4.0 + 2.0
            }
            Block::Tiles { items, .. } => {
                (items.len().div_ceil(tile_columns(items.len())) as f32) * 46.0 + 2.0
            }
            Block::Row(action) => {
                (chars(&action.label) * 12.0 / (content_width - 40.0))
                    .ceil()
                    .max(1.0)
                    * 18.0
                    + 8.0
            }
        };
    }
    (width, height.max(72.0))
}

/// Physical window bounds: the card's top-left starts at the pointer and the
/// whole popup shifts into the pointer monitor's work area; only a menu
/// taller than the work area is capped (and then scrolls).
fn popup_bounds(
    point: (i32, i32),
    work: (i32, i32, i32, i32),
    scale: f64,
    card: (f32, f32),
) -> ((i32, i32, i32, i32), bool) {
    let margin = (f64::from(SHADOW_MARGIN) * scale).round() as i32;
    let width = ((f64::from(card.0 + 2.0 * SHADOW_MARGIN) * scale).ceil() as i32).min(work.2);
    let natural = (f64::from(card.1 + 2.0 * SHADOW_MARGIN) * scale).ceil() as i32;
    let height = natural.min(work.3);
    let x = (point.0 - margin).clamp(work.0, work.0 + work.2 - width);
    let y = (point.1 - margin).clamp(work.1, work.1 + work.3 - height);
    ((x, y, width, height), natural > work.3)
}

/// Lease ids are shared by every native menu owner (list, copy toast).
pub fn next_lease() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub type SelectHandler = Rc<dyn Fn(u64, &'static str, &mut App)>;

pub struct ContextMenuPopup {
    host: Option<HostState>,
    tokens: DesignTokens,
    dark: bool,
    lease: u64,
    blocks: Vec<Block>,
    card_width: f32,
    capped: bool,
    on_select: SelectHandler,
    history: Option<history::HistoryMenu>,
}

fn color(hex: &str) -> Hsla {
    crate::view::color(hex)
}

impl ContextMenuPopup {
    /// Open (or reuse) the popup at a physical screen point for an acquired lease.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: &HostState,
        owner_hwnd: isize,
        lease: u64,
        point: (i32, i32),
        entries: Vec<MenuEntry>,
        tokens: DesignTokens,
        dark: bool,
        on_select: SelectHandler,
        cx: &mut App,
    ) -> Result<(), String> {
        if !host.native_context_menu_is_current(lease) {
            return Err(tr("菜单已关闭。", "The menu has closed.").into());
        }
        let lease_host = host.clone();
        let _lease_guard = platform::guard_condition_inherited(move || {
            lease_host.native_context_menu_is_current(lease)
        });
        let blocks = layout(entries);
        let card = card_size(&blocks);
        let area = overlay_work_area_from_point(point.0, point.1).ok_or_else(|| {
            tr(
                "无法确定菜单所在屏幕。",
                "Could not determine the menu's monitor.",
            )
        })?;
        let work = (
            area.work.left.round() as i32,
            area.work.top.round() as i32,
            area.work.width.round() as i32,
            area.work.height.round() as i32,
        );
        let (rect, capped) = popup_bounds(point, work, area.dpi_scale_y, card);
        let handle = match existing {
            Some(handle) => *handle,
            None => {
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
                                "ClipHost GPUI · 菜单",
                                "ClipHost GPUI · Menu",
                            ));
                            cx.new(|_| Self {
                                host: Some(host.clone()),
                                tokens: tokens.clone(),
                                dark,
                                lease,
                                blocks: Vec::new(),
                                card_width: card.0,
                                capped,
                                on_select: on_select.clone(),
                                history: None,
                            })
                        },
                    )
                    .map_err(|error| error.to_string())?;
                *existing = Some(handle);
                handle
            }
        };
        handle
            .update(cx, |popup, window, cx| {
                if let Some(history) = popup.history.take() {
                    if let Some(host) = popup.host.as_ref() {
                        host.clear_native_context_menu_keyboard(popup.lease);
                    }
                    drop(history);
                }
                popup.host = Some(host.clone());
                popup.tokens = tokens;
                popup.dark = dark;
                popup.lease = lease;
                popup.blocks = blocks;
                popup.card_width = card.0;
                popup.capped = capped;
                popup.on_select = on_select;
                cx.notify();
                #[cfg(windows)]
                if !host.set_native_context_menu_hwnd(lease, platform::hwnd(window)?.0 as isize) {
                    return Err(tr("菜单已关闭。", "The menu has closed.").into());
                }
                platform::set_geometry(window, Some((rect.0, rect.1)), None)?;
                platform::overlay_set_regions(
                    window,
                    vec![(
                        0.0, 0.0,
                        (f64::from(rect.2) / area.dpi_scale_y) as f32,
                        (f64::from(rect.3) / area.dpi_scale_y) as f32,
                    )],
                )?;
                platform::overlay_show(window, owner_hwnd, rect)
            })
            .map_err(|error| error.to_string())?
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |popup, window, _| {
            if popup.history.is_some() {
                if let Some(host) = popup.host.as_ref() {
                    host.clear_native_context_menu_keyboard(popup.lease);
                }
            }
            popup.lease = 0;
            popup.history = None;
            platform::hide(window)
        });
    }

    fn icon(&self, key: &str, size: f32, tint: Hsla) -> Option<crate::crisp_svg::CrispSvg> {
        self.tokens.icons.get(key).map(|path| {
            svg()
                .path(path.clone())
                .size(px(size))
                .flex_none()
                .text_color(tint)
        })
    }

    fn action(&self, action: &MenuAction, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let key = action.key;
        let lease = self.lease;
        let select = self.on_select.clone();
        let hover = color(&t.tab_active);
        div()
            .id(SharedString::from(key))
            .cursor_pointer()
            .rounded(px(8.0))
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                if lease != 0
                    && this.lease == lease
                    && this
                        .host
                        .as_ref()
                        .is_some_and(|host| host.native_context_menu_is_current(lease))
                {
                    let select = select.clone();
                    // The chooser may hide this window; leave its event dispatch first.
                    cx.defer(move |cx| select(lease, key, cx));
                }
            }))
    }
}

impl Render for ContextMenuPopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.history.is_some() {
            return self.render_history(window, cx);
        }
        window.set_window_title(tr("ClipHost GPUI · 菜单", "ClipHost GPUI · Menu"));
        let t = self.tokens.theme(self.dark).clone();
        let content_width = self.card_width - 12.0;
        let text = color(&t.text);
        let blocks = self.blocks.clone();
        let mut card = rsx! {
            <div id={"context-menu-card"} w={px(self.card_width)} max_h_full p={px(5.0)} flex
                flex_col gap={px(1.0)} rounded={px(12.0)} border_1
                border_color={color(&t.border)} bg={color(&t.surface)} shadow_lg
                text_color={text} />
        }
            .when(self.capped, |card| {
                card.overflow_y_scroll().smooth_scroll("context-menu-card", cx)
            });
        for block in &blocks {
            card = card.child(match block {
                Block::Separator => rsx! {
                    <div h={px(7.0)} flex_none flex items_center>
                        <div w_full h={px(1.0)} bg={color(&t.border)} />
                    </div>
                }
                    .into_any_element(),
                Block::Label(label) => rsx! {
                    <div h={px(19.0)} flex_none px={px(10.0)} pt={px(1.0)} text_size={px(11.0)}
                        line_height={px(16.0)} text_color={color(&t.text_muted)}>
                        {label.clone()}
                    </div>
                }
                    .into_any_element(),
                Block::Row(action) => {
                    let tint = if action.danger { color(&t.error) } else { text };
                    let icon_tint = if action.danger {
                        color(&t.error)
                    } else {
                        color(&t.text_muted)
                    };
                    self.action(action, cx)
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(10.0))
                        .min_h(px(26.0))
                        .px(px(10.0))
                        .py(px(4.0))
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .text_color(tint)
                        .child(
                            rsx! {
                                <div size={px(18.0)} flex_none flex items_center justify_center />
                            }
                                .children(
                                    action
                                        .icon()
                                        .and_then(|key| self.icon(key, 16.0, icon_tint)),
                                ),
                        )
                        .child(rsx! {
                            <div flex_1 min_w_0>
                                {action.label.clone()}
                            </div>
                        })
                        .into_any_element()
                }
                Block::Tiles {
                    items,
                    compact: true,
                } => rsx! {
                    <div flex flex_none flex_wrap gap={px(4.0)} pb={px(2.0)} />
                }
                    .children(items.iter().map(|action| {
                        let tint = if action.danger { color(&t.error) } else { text };
                        let icon_tint = if action.danger {
                            color(&t.error)
                        } else {
                            color(&t.accent)
                        };
                        self.action(action, cx)
                            .w(px(chip_width(action, content_width)))
                            .h(px(26.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .px(px(7.0))
                            .border_1()
                            .border_color(color(&t.border))
                            .bg(color(&t.secondary))
                            .text_size(px(12.0))
                            .line_height(px(17.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(tint)
                            .children(
                                action
                                    .icon()
                                    .and_then(|key| self.icon(key, 16.0, icon_tint)),
                            )
                            .child(
                                rsx! {
                                    <div min_w_0 overflow_hidden>
                                        {action.tile_label(true)}
                                    </div>
                                },
                            )
                    }))
                    .into_any_element(),
                Block::Tiles { items, .. } => {
                    let columns = tile_columns(items.len());
                    let tile_width = (content_width - 2.0 * (columns - 1) as f32) / columns as f32;
                    rsx! {
                        <div flex flex_none flex_wrap gap={px(2.0)} pb={px(2.0)} />
                    }
                        .children(items.iter().map(|action| {
                            let tint = if action.danger { color(&t.error) } else { text };
                            let icon_tint = if action.danger {
                                color(&t.error)
                            } else {
                                color(&t.accent)
                            };
                            self.action(action, cx)
                                .w(px(tile_width))
                                .h(px(46.0))
                                .p(px(4.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap(px(3.0))
                                .text_size(px(12.0))
                                .line_height(px(17.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tint)
                                .children(
                                    action
                                        .icon()
                                        .and_then(|key| self.icon(key, 18.0, icon_tint)),
                                )
                                .child(
                                    rsx! {
                                        <div max_w_full overflow_hidden line_height={relative(1.3)}>
                                            {action.tile_label(false)}
                                        </div>
                                    },
                                )
                        }))
                        .into_any_element()
                }
            });
        }
        rsx! {
            <div size_full p={px(SHADOW_MARGIN)} bg={gpui::transparent_black()}
                font_family={self.tokens.typography.font_family.clone()}>
                {card}
            </div>
        }
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> Vec<MenuEntry> {
        vec![
            MenuAction::tile("paste", "粘贴"),
            MenuAction::tile("copy-only", "复制"),
            MenuAction::tile("favorite", "收藏"),
            MenuAction::tile("pin-toggle", "置顶或取消置顶"),
            MenuAction::danger_tile("delete", "删除"),
            MenuEntry::Separator,
            MenuEntry::Label("粘贴方式".into()),
            MenuAction::tile("paste-plain-text", "纯文本粘贴"),
            MenuAction::tile("paste-standard-text", "规范粘贴"),
            MenuEntry::Separator,
            MenuAction::row("lan-share", "发送到局域网（所有设备）"),
            MenuAction::row("distinct", "去除重复"),
        ]
    }

    #[test]
    fn layout_matches_frontend_tile_and_section_rules() {
        let blocks = layout(primary());
        assert!(matches!(&blocks[0], Block::Tiles { items, compact: false } if items.len() == 5));
        assert_eq!(blocks[1], Block::Separator);
        assert_eq!(blocks[2], Block::Label("粘贴方式".into()));
        assert!(matches!(&blocks[3], Block::Tiles { items, compact: true } if items.len() == 2));
        assert!(matches!(&blocks[5], Block::Row(action) if action.key == "lan-share"));
        // A lone tile degrades into a row.
        let single = layout(vec![MenuAction::tile("paste", "粘贴")]);
        assert!(matches!(&single[0], Block::Row(_)));
        let pin = MenuAction {
            key: "pin-toggle",
            label: "取消置顶".into(),
            danger: false,
            tile: true,
        };
        assert_eq!(pin.tile_label(false), tr("取消置顶", "Unpin"));
        assert_eq!(pin.icon(), Some("unpin-top"));
        let english_pin = MenuAction {
            label: "Unpin".into(),
            ..pin.clone()
        };
        assert_eq!(english_pin.icon(), Some("unpin-top"));
        let mixed_pin = MenuAction {
            label: "Pin or unpin".into(),
            ..pin
        };
        assert_eq!(mixed_pin.icon(), Some("pin-top"));
    }

    #[test]
    fn card_size_matches_the_shared_frontend_estimate() {
        // estimateLaidOutMenuSize for the same entries: width 312,
        // 12 + 6 gaps + 46+2 + 7 + 19 + 26+2 + 7 + 26 + 26.
        let (width, height) = card_size(&layout(primary()));
        assert_eq!(width, 312.0);
        assert_eq!(
            height,
            12.0 + 6.0 + 48.0 + 7.0 + 19.0 + 28.0 + 7.0 + 26.0 + 26.0
        );
        assert_eq!(
            card_size(&layout(vec![MenuAction::row("scroll-top", "回到开头")])).1,
            72.0
        );
    }

    #[test]
    fn english_paste_options_wrap_without_compressing_chips() {
        let blocks = layout(vec![
            MenuEntry::Label("Paste options".into()),
            MenuAction::tile("paste-plain-text", "Paste as plain text"),
            MenuAction::tile("paste-standard-text", "Normalized paste"),
        ]);
        let (width, height) = card_size(&blocks);
        assert_eq!(width, MIN_WIDTH);
        assert_eq!(height, 90.0, "English labels use two full-height chip rows");
    }

    #[test]
    fn popup_shifts_whole_menu_into_the_work_area_and_caps_only_when_taller() {
        let work = (0, 0, 1920, 1040);
        let ((x, y, w, h), capped) = popup_bounds((1900, 1000), work, 1.25, (312.0, 200.0));
        assert_eq!((w, h), (410, 270));
        assert_eq!((x + w, y + h), (1920, 1040), "shifted, not compressed");
        assert!(!capped);
        let ((x, y, _, _), _) = popup_bounds((100, 100), work, 1.0, (312.0, 200.0));
        assert_eq!((x, y), (92, 92), "card corner starts at the pointer");
        let ((_, y, _, h), capped) =
            popup_bounds((100, 100), (0, 0, 800, 300), 1.0, (312.0, 400.0));
        assert_eq!((y, h), (0, 300));
        assert!(capped);
    }
}
