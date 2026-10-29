//! One continuous settings page. Its tabs move the shared viewport; each
//! underlying SettingsForm retains its original configuration and draft.

use super::*;
use std::rc::Rc;

pub(super) const SUPER_MENU_SECTION_IDS: [&str; 5] =
    ["general", "items", "windows", "files", "text-actions"];
const NAV_HEIGHT: f32 = 42.0;
const NAV_GAP: f32 = 18.0;
const NAV_PADDING: f32 = 22.0;
const TAB_PADDING: f32 = 4.0;
const ANCHOR_INSET: f32 = 20.0;
const POSITION_EPSILON: f32 = 1.0;

pub(super) fn super_menu_section_label(id: &str) -> &'static str {
    match id {
        "items" => tr("显示条目", "Visible items"),
        "windows" => tr("窗口菜单", "Window menu"),
        "files" => tr("文件菜单", "File menu"),
        "text-actions" => tr("文本操作", "Text actions"),
        _ => tr("常规", "General"),
    }
}

fn section_index(id: &str) -> Option<usize> {
    SUPER_MENU_SECTION_IDS
        .iter()
        .position(|candidate| *candidate == id)
}

#[derive(Clone, Copy, Debug)]
struct PendingAnchor {
    index: usize,
    target: Option<f32>,
}

struct SectionState {
    active: usize,
    pending: Option<PendingAnchor>,
    bounds: [Option<Bounds<Pixels>>; 5],
    nav_bounds: [Option<Bounds<Pixels>>; 5],
    viewport: Option<Bounds<Pixels>>,
    offset: f32,
    maximum: f32,
    inset: f32,
    nav_scroll: ScrollHandle,
    focus: Vec<FocusHandle>,
    revision: u64,
    synchronized_revision: u64,
    reveal_requested: bool,
}

impl Default for SectionState {
    fn default() -> Self {
        Self {
            active: 0,
            pending: None,
            bounds: [None; 5],
            nav_bounds: [None; 5],
            viewport: None,
            offset: 0.0,
            maximum: 0.0,
            inset: ANCHOR_INSET,
            nav_scroll: ScrollHandle::new(),
            focus: Vec::new(),
            revision: 0,
            synchronized_revision: 0,
            reveal_requested: true,
        }
    }
}

/// Clones are shared with the retained forms, so their heading measurements
/// refer to the same viewport and never require rebuilding a form.
#[derive(Clone, Default)]
pub(super) struct SuperMenuSections(Rc<RefCell<SectionState>>);

fn anchor_target(
    scroll_top: f32,
    heading_top: f32,
    viewport_top: f32,
    inset: f32,
    maximum: f32,
) -> Option<f32> {
    [scroll_top, heading_top, viewport_top, inset, maximum]
        .iter()
        .all(|n| n.is_finite())
        .then(|| (scroll_top + heading_top - viewport_top - inset).clamp(0.0, maximum.max(0.0)))
}

fn scrollspy_index(
    tops: &[Option<f32>; 5],
    viewport_top: f32,
    inset: f32,
    scroll_top: f32,
    maximum: f32,
) -> usize {
    let available = tops.iter().enumerate().filter(|(_, top)| top.is_some());
    if scroll_top > POSITION_EPSILON && maximum > 0.0 && scroll_top >= maximum - 2.0 {
        return available.last().map_or(0, |(index, _)| index);
    }
    let threshold = viewport_top + inset + 2.0;
    let mut active = tops.iter().position(Option::is_some).unwrap_or(0);
    for (index, top) in available {
        if top.is_some_and(|top| top <= threshold) {
            active = index;
        }
    }
    active
}

fn keyboard_index(current: usize, key: &str) -> Option<usize> {
    match key {
        "left" | "ArrowLeft" => Some((current + 4) % 5),
        "right" | "ArrowRight" => Some((current + 1) % 5),
        "home" | "Home" => Some(0),
        "end" | "End" => Some(4),
        "enter" | "Enter" | "space" | " " => Some(current),
        _ => None,
    }
}

fn horizontal_reveal(
    offset: f32,
    maximum: f32,
    left: f32,
    right: f32,
    viewport_left: f32,
    viewport_right: f32,
) -> f32 {
    let desired = if left < viewport_left + NAV_PADDING {
        offset + viewport_left + NAV_PADDING - left
    } else if right > viewport_right - NAV_PADDING {
        offset - (right - viewport_right + NAV_PADDING)
    } else {
        offset
    };
    desired.clamp(-maximum.max(0.0), 0.0)
}

impl SuperMenuSections {
    pub(super) fn focus_handles(&self) -> Vec<FocusHandle> {
        self.0.borrow().focus.clone()
    }

    pub(super) fn active(&self) -> &'static str {
        SUPER_MENU_SECTION_IDS[self.0.borrow().active]
    }

    pub(super) fn pending(&self) -> Option<&'static str> {
        self.0
            .borrow()
            .pending
            .map(|pending| SUPER_MENU_SECTION_IDS[pending.index])
    }

    pub(super) fn request(&self, id: &str) -> bool {
        let Some(index) = section_index(id) else {
            return false;
        };
        let mut state = self.0.borrow_mut();
        state.active = index;
        state.reveal_requested = true;
        state.pending = Some(PendingAnchor {
            index,
            target: None,
        });
        state.revision = state.revision.wrapping_add(1);
        true
    }

    pub(super) fn cancel_jump(&self) {
        let mut state = self.0.borrow_mut();
        if state.pending.take().is_some() {
            state.revision = state.revision.wrapping_add(1);
        }
    }

    /// Discard geometry when this continuous page leaves the render tree.
    /// Active selection and the form entities are deliberately retained.
    pub(super) fn clear_layout(&self) {
        let mut state = self.0.borrow_mut();
        if state.viewport.is_none()
            && state.bounds.iter().all(Option::is_none)
            && state.nav_bounds.iter().all(Option::is_none)
        {
            return;
        }
        state.bounds = [None; 5];
        state.nav_bounds = [None; 5];
        state.viewport = None;
        state.revision = state.revision.wrapping_add(1);
    }

    pub(super) fn record_section(&self, id: &str, bounds: Bounds<Pixels>) -> bool {
        let Some(index) = section_index(id) else {
            return false;
        };
        let mut state = self.0.borrow_mut();
        if state.bounds[index] == Some(bounds) {
            return false;
        }
        state.bounds[index] = Some(bounds);
        state.revision = state.revision.wrapping_add(1);
        true
    }

    pub(super) fn section_marker(&self, id: &str) -> AnyElement {
        let index = section_index(id);
        let shared = self.clone();
        gpui::canvas(
            move |bounds, window, _| {
                if let Some(index) = index {
                    if shared.record_section(SUPER_MENU_SECTION_IDS[index], bounds) {
                        // A section may finish loading after the viewport probe
                        // was prepainted. Observe that final geometry next frame.
                        window.request_animation_frame();
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .right(px(0.0))
        .h(px(0.0))
        .into_any_element()
    }

    pub(super) fn record_viewport(
        &self,
        bounds: Bounds<Pixels>,
        offset: f32,
        maximum: f32,
        inset: f32,
    ) -> bool {
        let mut state = self.0.borrow_mut();
        if state.viewport != Some(bounds) {
            state.reveal_requested = true;
        }
        if state.viewport != Some(bounds)
            || state.offset != offset
            || state.maximum != maximum
            || state.inset != inset
        {
            state.viewport = Some(bounds);
            state.offset = offset;
            state.maximum = maximum;
            state.inset = inset;
            state.revision = state.revision.wrapping_add(1);
        }
        state.revision != state.synchronized_revision
    }

    pub(super) fn diagnostics(&self) -> Value {
        let state = self.0.borrow();
        let bounds =
            |bounds: Option<Bounds<Pixels>>| {
                bounds.map(|bounds| json!({
            "x":f32::from(bounds.left()), "y":f32::from(bounds.top()),
            "width":f32::from(bounds.size.width), "height":f32::from(bounds.size.height),
        }))
            };
        json!({
            "activeSection":SUPER_MENU_SECTION_IDS[state.active],
            "pendingSection":state.pending.map(|pending| SUPER_MENU_SECTION_IDS[pending.index]),
            "pendingScrollTop":state.pending.and_then(|pending| pending.target),
            "scrollBounds":bounds(state.viewport), "scrollOffsetY":state.offset,
            "scrollTop":-state.offset, "maximumScrollTop":state.maximum, "anchorInset":state.inset,
            "navScrollOffsetX":f32::from(state.nav_scroll.offset().x),
            "navScrollBounds":bounds(Some(state.nav_scroll.bounds())),
            "sections":SUPER_MENU_SECTION_IDS.iter().enumerate().map(|(index,id)| json!({
                "id":id, "bounds":bounds(state.bounds[index]), "navBounds":bounds(state.nav_bounds[index]),
            })).collect::<Vec<_>>(),
        })
    }

    fn synchronize(&self) -> (bool, Option<f32>) {
        let mut state = self.0.borrow_mut();
        state.synchronized_revision = state.revision;
        let Some(viewport) = state.viewport else {
            return (false, None);
        };
        if let Some(mut pending) = state.pending {
            let Some(bounds) = state.bounds[pending.index] else {
                return (false, None);
            };
            let Some(target) = anchor_target(
                -state.offset,
                f32::from(bounds.top()),
                f32::from(viewport.top()),
                state.inset,
                state.maximum,
            ) else {
                return (false, None);
            };
            if (-state.offset - target).abs() <= POSITION_EPSILON {
                // This includes clicking the current heading at the top: no
                // animation frame is needed to finish a zero-distance jump.
                state.pending = None;
                return (false, None);
            }
            if pending
                .target
                .is_none_or(|previous| (target - previous).abs() > POSITION_EPSILON)
            {
                pending.target = Some(target);
                state.pending = Some(pending);
                return (false, Some(target));
            }
            return (false, None);
        }
        let tops = state
            .bounds
            .map(|bounds| bounds.map(|bounds| f32::from(bounds.top())));
        let active = scrollspy_index(
            &tops,
            f32::from(viewport.top()),
            state.inset,
            -state.offset,
            state.maximum,
        );
        let changed = state.active != active;
        state.active = active;
        state.reveal_requested |= changed;
        (changed, None)
    }

    fn reveal_active(&self) -> bool {
        let mut state = self.0.borrow_mut();
        if !state.reveal_requested {
            return false;
        }
        let Some(bounds) = state.nav_bounds[state.active] else {
            return false;
        };
        // Reveal on a selection/viewport change. Manual horizontal scrolling
        // must remain under the user's control even when the active tab leaves.
        state.reveal_requested = false;
        let viewport = state.nav_scroll.bounds();
        let offset = state.nav_scroll.offset();
        let next = horizontal_reveal(
            f32::from(offset.x),
            f32::from(state.nav_scroll.max_offset().width),
            f32::from(bounds.left()),
            f32::from(bounds.right()),
            f32::from(viewport.left()),
            f32::from(viewport.right()),
        );
        if (next - f32::from(offset.x)).abs() < 0.5 {
            return false;
        }
        state.nav_scroll.set_offset(point(px(next), offset.y));
        true
    }
}

impl SettingsWindow {
    pub(super) fn request_super_menu_section(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        if self.closed || self.leave.is_some() || self.search_input.read(cx).is_composing()
            || self.visible_forms().iter().any(|form| form.read(cx).composition_active(cx))
        {
            return false;
        }
        if self.super_menu_sections.request(id) {
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn cancel_super_menu_section_jump(&mut self, cx: &mut Context<Self>) {
        let jumping = self.super_menu_sections.pending().is_some();
        self.super_menu_sections.cancel_jump();
        if jumping {
            smooth_wheel::cancel_glide("settings-content", cx);
        }
        cx.notify();
    }

    pub(super) fn render_super_menu_tabs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shared = self.super_menu_sections.clone();
        let (scroll, focus) = {
            let mut state = shared.0.borrow_mut();
            if state.focus.is_empty() {
                state.focus = (0..5).map(|_| cx.focus_handle()).collect();
            }
            (state.nav_scroll.clone(), state.focus.clone())
        };
        let theme = self.tokens.theme(self.dark);
        let accent = color(&theme.accent);
        let text = color(&theme.text);
        let muted = color(&theme.text_muted);
        let mut tabs = div()
            .id("settings-super-menu-tabs")
            .h(px(NAV_HEIGHT))
            .flex_none()
            .min_w_0()
            .flex()
            .gap(px(NAV_GAP))
            .px(px(NAV_PADDING))
            .border_b_1()
            .border_color(color(&theme.border))
            .overflow_x_scroll()
            .track_scroll(&scroll);
        for (index, id) in SUPER_MENU_SECTION_IDS.iter().copied().enumerate() {
            let geometry = shared.clone();
            let active = shared.active() == id;
            let focused = focus[index].is_focused(window);
            let click_focus = focus[index].clone();
            let key_focus = focus.clone();
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("settings-super-menu-tab-{id}")))
                    .relative()
                    .track_focus(&focus[index])
                    .h_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(TAB_PADDING))
                    .text_size(px(12.0))
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(if active {
                        accent
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if active { accent } else { muted })
                    .hover(move |style| style.text_color(text))
                    .when(focused, |tab| {
                        tab.child(
                            div()
                                .absolute()
                                .inset(px(2.0))
                                .rounded(px(3.0))
                                .border_1()
                                .border_color(accent),
                        )
                    })
                    .child(super_menu_section_label(id))
                    .child(
                        gpui::canvas(
                            move |bounds, window, _| {
                                let mut state = geometry.0.borrow_mut();
                                if state.nav_bounds[index] != Some(bounds) {
                                    state.nav_bounds[index] = Some(bounds);
                                    state.revision = state.revision.wrapping_add(1);
                                    window.request_animation_frame();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.request_super_menu_section(id, cx) {
                            click_focus.focus(window);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        if let Some(next) = keyboard_index(index, &event.keystroke.key) {
                            if this.request_super_menu_section(SUPER_MENU_SECTION_IDS[next], cx) {
                                key_focus[next].focus(window);
                            }
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        tabs.into_any_element()
    }

    /// Place in the scrolling content. Read ScrollHandle's fixed viewport,
    /// rather than this canvas's origin, which moves with the document.
    pub(super) fn super_menu_scroll_probe(&self, cx: &Context<Self>) -> AnyElement {
        let shared = self.super_menu_sections.clone();
        let scroll = self.scroll.clone();
        let weak = cx.weak_entity();
        let page = self.active.clone();
        gpui::canvas(
            move |_, window, _| {
                if shared.record_viewport(
                    scroll.bounds(),
                    f32::from(scroll.offset().y),
                    f32::from(scroll.max_offset().height),
                    ANCHOR_INSET,
                ) {
                    let weak = weak.clone();
                    let page = page.clone();
                    window.on_next_frame(move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if this.closed || this.active != page || this.search_query.is_active() {
                                return;
                            }
                            let (changed, target) = this.super_menu_sections.synchronize();
                            if let Some(target) = target {
                                smooth_wheel::glide_to(
                                    "settings-content",
                                    &this.scroll,
                                    target,
                                    window,
                                    cx,
                                );
                            }
                            let revealed = this.super_menu_sections.reveal_active();
                            if changed || revealed {
                                cx.notify();
                            }
                        });
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .w(px(0.0))
        .h(px(0.0))
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_use_the_fixed_viewport_and_clamp_both_edges() {
        assert_eq!(anchor_target(100.0, 420.0, 80.0, 20.0, 800.0), Some(420.0));
        assert_eq!(anchor_target(100.0, 0.0, 80.0, 20.0, 800.0), Some(0.0));
        assert_eq!(anchor_target(100.0, 1200.0, 80.0, 20.0, 800.0), Some(800.0));
        assert_eq!(anchor_target(100.0, f32::NAN, 80.0, 20.0, 800.0), None);
    }

    #[test]
    fn scrollspy_tracks_actual_headings_and_reaches_the_short_last_section() {
        let tops = [
            Some(-20.0),
            Some(100.0),
            Some(300.0),
            Some(600.0),
            Some(900.0),
        ];
        assert_eq!(scrollspy_index(&tops, 80.0, 20.0, 120.0, 1000.0), 1);
        assert_eq!(scrollspy_index(&tops, 80.0, 20.0, 999.0, 1000.0), 4);
        assert_eq!(scrollspy_index(&[None; 5], 80.0, 20.0, 0.0, 0.0), 0);
    }

    #[test]
    fn rapid_clicks_replace_pending_section_and_manual_scroll_releases_it() {
        let state = SuperMenuSections::default();
        assert!(state.request("files"));
        assert!(state.request("items"));
        assert_eq!(state.active(), "items");
        assert_eq!(state.pending(), Some("items"));
        assert!(!state.request("unknown"));
        assert_eq!(state.pending(), Some("items"));
        state.cancel_jump();
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn anchor_waits_for_real_layout_and_finishes_without_motion_at_its_destination() {
        let state = SuperMenuSections::default();
        state.request("general");
        assert_eq!(state.synchronize(), (false, None));
        assert_eq!(state.pending(), Some("general"));
        state.record_viewport(
            Bounds::new(point(px(0.0), px(80.0)), size(px(400.0), px(500.0))),
            0.0,
            800.0,
            20.0,
        );
        state.record_section(
            "general",
            Bounds::new(point(px(20.0), px(100.0)), size(px(300.0), px(0.0))),
        );
        assert_eq!(state.synchronize(), (false, None));
        assert_eq!(state.pending(), None);
        state.clear_layout();
        let revision = state.0.borrow().revision;
        state.clear_layout();
        assert_eq!(
            state.0.borrow().revision,
            revision,
            "hidden pages do not keep invalidating layout"
        );
    }

    #[test]
    fn keyboard_navigation_wraps_without_consuming_tab_or_editing_keys() {
        assert_eq!(keyboard_index(0, "left"), Some(4));
        assert_eq!(keyboard_index(4, "right"), Some(0));
        assert_eq!(keyboard_index(2, "home"), Some(0));
        assert_eq!(keyboard_index(2, "end"), Some(4));
        assert_eq!(keyboard_index(2, "enter"), Some(2));
        assert_eq!(keyboard_index(2, "tab"), None);
        assert_eq!(keyboard_index(2, "backspace"), None);
    }

    #[test]
    fn active_tab_reveals_horizontally_and_keeps_the_navigation_padding() {
        assert_eq!(
            horizontal_reveal(-30.0, 200.0, 4.0, 100.0, 0.0, 240.0),
            -12.0
        );
        assert_eq!(
            horizontal_reveal(0.0, 200.0, 230.0, 310.0, 0.0, 240.0),
            -92.0
        );
        assert_eq!(
            horizontal_reveal(-200.0, 200.0, 20.0, 100.0, 0.0, 240.0),
            -198.0
        );
        assert_eq!(horizontal_reveal(0.0, 0.0, 230.0, 310.0, 0.0, 240.0), 0.0);
    }
}
