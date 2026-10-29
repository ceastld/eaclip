//! Shared vertical scrollbar for native lists and ordinary scroll containers.
//! Virtual lists map the thumb to estimated content height (unmeasured rows
//! count at the average measured height, like a browser's scroll height);
//! creating a scrollbar never forces the entire list to render or decode.
//!
//! Overlay style (DESIGN 2026-10-14, tokens `scrollbar`): no gutter; a thin
//! pill appears while scrolling or while the pointer is in the edge strip,
//! widens there, and fades out after an idle delay. Moving over rows alone
//! does not reveal it (user feedback: the bar was distracting).

use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    canvas, div, fill, point, prelude::*, px, size, AnyElement, App, Bounds, DispatchPhase,
    ElementId, HitboxBehavior, Hsla, ListOffset, ListState, MouseButton, MouseMoveEvent,
    MouseUpEvent, Pixels, ScrollDelta, ScrollHandle, ScrollWheelEvent, Window,
};

use crate::{motion, preview::ScrollbarTokens};

/// Mirrors tokens `scrollbar.minThumb` (checked by a unit test).
const MIN_THUMB: f32 = 24.0;
const THUMB_INSET: f32 = 2.0;
const MAX_VISIBLE_SCAN: usize = 512;
const EPSILON: f32 = 0.5;

#[derive(Clone)]
pub(crate) enum ScrollbarTarget {
    List(ListState),
    Scroll(ScrollHandle),
}

#[derive(Clone, Copy)]
pub(crate) struct ScrollbarStyle {
    /// Foreground the thumb and track are derived from (theme text colour),
    /// so the bar reads on plain, custom and wallpaper surfaces alike.
    pub(crate) ink: Hsla,
    pub(crate) metrics: ScrollbarTokens,
    /// Design snapshots keep the bar shown at rest.
    pub(crate) always_visible: bool,
}

/// Mouse-wheel notches glide to their target instead of jumping (DESIGN
/// 2026-10-14). Fractional deltas from precision touchpads pass through.
///
/// The glide keeps the distance still to travel and moves relative to the
/// current row anchor each frame. Absolute pixel targets drifted whenever a
/// row above was (re)measured — every arriving thumbnail did that — and the
/// glide then stopped mid-notch.
#[derive(Clone, Copy, Debug)]
struct SmoothScroll {
    /// Signed distance left to travel; positive scrolls toward the end.
    remaining: f32,
    last_tick: Instant,
    /// Anchor this glide last applied; any other change cancels it.
    last_top: Option<ListOffset>,
}

fn same_top(a: ListOffset, b: ListOffset) -> bool {
    a.item_ix == b.item_ix && (f32::from(a.offset_in_item) - f32::from(b.offset_in_item)).abs() <= EPSILON
}

/// Whether a wheel delta (in lines) is a whole mouse notch worth animating.
pub(crate) fn is_notched_wheel(lines_y: f32) -> bool {
    lines_y.is_finite() && lines_y.abs() >= 0.999
}

const EXPAND_KEY: u8 = 0;
const VISIBLE_KEY: u8 = 1;

/// Whether the overlay bar should be shown at `now`, and when an idle fade
/// must be scheduled because nothing else will repaint the window.
pub(crate) fn overlay_visibility(
    now: Instant,
    last_activity: Option<Instant>,
    idle: Duration,
    engaged: bool,
) -> (bool, Option<Instant>) {
    if engaged {
        return (true, None);
    }
    match last_activity {
        Some(at) if now.saturating_duration_since(at) < idle => (true, Some(at + idle)),
        _ => (false, None),
    }
}

pub(crate) type ScrollbarChanged = Rc<dyn Fn(&mut Window, &mut App)>;

/// Approximate the exclusive end of the actual viewport using only rows that
/// were already measured. Useful after programmatic scrollbar changes, which
/// do not emit ListState's wheel-only ListScrollEvent.
pub(crate) fn visible_end(list: &ListState) -> usize {
    let count = list.item_count();
    let first = list.logical_scroll_top().item_ix.min(count);
    if first == count {
        return count;
    }
    let viewport = list.viewport_bounds();
    let mut end = (first + 1).min(count);
    if viewport.size.height <= px(0.0) {
        return end;
    }
    for index in first..count.min(first.saturating_add(MAX_VISIBLE_SCAN)) {
        let Some(bounds) = list.bounds_for_item(index) else {
            break;
        };
        if bounds.top() >= viewport.bottom() {
            break;
        }
        if bounds.bottom() > viewport.top() {
            end = index + 1;
        }
    }
    end
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    height: f32,
    maximum: f32,
    offset: f32,
    page: f32,
    thumb_height: f32,
    thumb_top: f32,
}

impl Geometry {
    fn new(height: f32, maximum: f32, offset: f32, page: f32) -> Option<Self> {
        if [height, maximum, offset, page]
            .iter()
            .any(|value| !value.is_finite())
            || height <= 0.0
            || maximum <= f32::EPSILON
            || page <= 0.0
        {
            return None;
        }
        let thumb_height = (height * page / (page + maximum)).clamp(MIN_THUMB.min(height), height);
        let offset = offset.clamp(0.0, maximum);
        let thumb_top = offset / maximum * (height - thumb_height);
        Some(Self {
            height,
            maximum,
            offset,
            page,
            thumb_height,
            thumb_top,
        })
    }

    fn travel(self) -> f32 {
        (self.height - self.thumb_height).max(0.0)
    }

    fn contains_thumb(self, y: f32) -> bool {
        y >= self.thumb_top && y <= self.thumb_top + self.thumb_height
    }

    fn offset_for_top(self, top: f32) -> f32 {
        if self.travel() <= f32::EPSILON {
            0.0
        } else {
            (top / self.travel()).clamp(0.0, 1.0) * self.maximum
        }
    }

    fn page_toward(self, y: f32) -> f32 {
        (self.offset
            + if y < self.thumb_top {
                -self.page
            } else {
                self.page
            })
        .clamp(0.0, self.maximum)
    }
}

#[derive(Clone, Copy, Debug)]
enum Mapping {
    Pixels,
    /// Virtual list pixels with unmeasured rows at the average row height.
    Estimated,
}

#[derive(Clone, Copy, Debug)]
struct Metrics {
    geometry: Geometry,
    mapping: Mapping,
}

#[derive(Clone)]
struct Drag {
    generation: u64,
    grab_y: f32,
    metrics: Metrics,
    viewport_width: f32,
    count: usize,
}

impl Drag {
    fn accepts(&self, target: &ScrollbarTarget, bounds: Bounds<Pixels>) -> bool {
        target.count() >= self.count
            && (self.viewport_width - target.viewport_width()).abs() <= EPSILON
            && (self.metrics.geometry.height - f32::from(bounds.size.height)).abs() <= EPSILON
    }
}

struct State {
    generation: Option<u64>,
    bounds: Bounds<Pixels>,
    metrics: Option<Metrics>,
    drag: Option<Drag>,
    hovered: bool,
    /// Pointer within the bar's hit strip (widens the thumb).
    near: bool,
    motion: motion::Tweens<u8>,
    /// Last scroll offset seen while painting; changes count as activity.
    last_offset: Option<f32>,
    last_activity: Option<Instant>,
    /// Idle-fade wake already scheduled for this deadline.
    wake_at: Option<Instant>,
    visible: bool,
    smooth: Option<SmoothScroll>,
    /// A glide frame is scheduled; it clears this when the glide ends.
    ticking: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            generation: None,
            bounds: Bounds::default(),
            metrics: None,
            drag: None,
            hovered: false,
            near: false,
            motion: motion::Tweens::default(),
            last_offset: None,
            last_activity: None,
            wake_at: None,
            visible: false,
            smooth: None,
            ticking: false,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct ScrollbarState(Rc<RefCell<State>>);

impl ScrollbarState {
    pub(crate) fn is_dragging(&self) -> bool {
        self.0.borrow().drag.is_some()
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.0.borrow_mut();
        state.drag = None;
        state.hovered = false;
        state.metrics = None;
        state.smooth = None;
        state.motion.clear();
    }

    /// Rows were re-measured: drop the painted thumb geometry until the next
    /// paint. A running glide continues; it moves relative to its row anchor.
    pub(crate) fn invalidate_measurements(&self) {
        self.0.borrow_mut().metrics = None;
    }

    fn update_metrics(&self, target: &ScrollbarTarget, bounds: Bounds<Pixels>) -> Option<Metrics> {
        let mut state = self.0.borrow_mut();
        if state
            .drag
            .as_ref()
            .is_some_and(|drag| !drag.accepts(target, bounds))
        {
            state.drag = None;
        }
        let metrics = match &state.drag {
            Some(drag) => Some(drag.metrics),
            None => target.metrics(f32::from(bounds.size.height)),
        };
        if let Some(metrics) = metrics {
            let offset = metrics.geometry.offset;
            if state
                .last_offset
                .is_some_and(|last| (last - offset).abs() > EPSILON)
            {
                state.last_activity = Some(Instant::now());
            }
            state.last_offset = Some(offset);
        }
        state.bounds = bounds;
        state.metrics = metrics;
        metrics
    }

    /// The parent is relative; the bar overlays its right edge without a
    /// gutter. It paints only when content overflows and recently changed or
    /// is engaged by the pointer.
    pub(crate) fn render(
        &self,
        id: impl Into<ElementId>,
        target: ScrollbarTarget,
        generation: u64,
        style: ScrollbarStyle,
        changed: ScrollbarChanged,
    ) -> AnyElement {
        {
            let mut state = self.0.borrow_mut();
            if state.generation != Some(generation) {
                state.drag = None;
                state.generation = Some(generation);
                state.metrics = None;
                state.hovered = false;
                state.near = false;
                state.last_offset = None;
                state.last_activity = None;
                state.wake_at = None;
                state.visible = false;
                state.smooth = None;
                state.motion.clear();
            }
        }
        let down_state = self.clone();
        let down_target = target.clone();
        let down_changed = changed.clone();
        let wheel_state = self.clone();
        let wheel_target = target.clone();
        let wheel_changed = changed.clone();
        let paint_state = self.clone();
        let measure_target = target.clone();
        let paint_target = target.clone();
        let paint_changed = changed.clone();
        let width = if style.metrics.hit_width.is_finite() {
            style.metrics.hit_width.max(1.0)
        } else {
            12.0
        };
        let element_id: ElementId = id.into();
        rsx! {
            <div id={element_id} absolute right_0 top_0 bottom_0 w={px(width)} />
        }
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let mut state = down_state.0.borrow_mut();
                if state.generation != Some(generation) {
                    return;
                }
                // A faded-out bar lets clicks reach the row underneath.
                if !state.visible {
                    return;
                }
                let Some(metrics) = state.metrics else {
                    return;
                };
                let y = f32::from(event.position.y - state.bounds.top());
                let mut scroll = None;
                state.smooth = None;
                state.last_activity = Some(Instant::now());
                if metrics.geometry.contains_thumb(y) {
                    state.drag = Some(Drag {
                        generation,
                        grab_y: y - metrics.geometry.thumb_top,
                        metrics,
                        viewport_width: down_target.viewport_width(),
                        count: down_target.count(),
                    });
                } else {
                    scroll = Some(metrics.geometry.page_toward(y));
                }
                drop(state);
                window.prevent_default();
                cx.stop_propagation();
                if let Some(offset) = scroll {
                    down_target.apply(metrics, offset);
                }
                window.refresh();
                down_changed(window, cx);
            })
            .on_scroll_wheel(move |event, window, cx| {
                let state = wheel_state.0.borrow();
                if state.generation != Some(generation) {
                    return;
                }
                let Some(metrics) = state.metrics else {
                    return;
                };
                drop(state);
                if wheel_target.wheel(metrics, event.delta, window.line_height()) {
                    window.prevent_default();
                    cx.stop_propagation();
                    window.refresh();
                    wheel_changed(window, cx);
                }
            })
            .child(
                canvas(
                    move |bounds, window, _| {
                        if paint_state.0.borrow().generation != Some(generation) {
                            return (None, None);
                        }
                        let viewport = Some(window.insert_hitbox(
                            measure_target.viewport_bounds(),
                            HitboxBehavior::Normal,
                        ));
                        let metrics = paint_state.update_metrics(&measure_target, bounds);
                        (metrics, viewport)
                    },
                    {
                        let state = self.clone();
                        move |bounds,
                              (metrics, viewport): (Option<Metrics>, Option<gpui::Hitbox>),
                              window,
                              cx| {
                            if let Some(metrics) = metrics {
                                let now = Instant::now();
                                let m = style.metrics;
                                let mut inner = state.0.borrow_mut();
                                if inner.generation == Some(generation) {
                                    let mouse = window.mouse_position();
                                    inner.near = bounds.contains(&mouse);
                                    inner.hovered = inner.near
                                        && metrics
                                            .geometry
                                            .contains_thumb(f32::from(mouse.y - bounds.top()));
                                    let dragging = inner.drag.is_some();
                                    let engaged = style.always_visible || inner.near || dragging;
                                    let idle =
                                        Duration::from_millis(m.idle_hide_ms.max(0.0) as u64);
                                    let (visible, wake) =
                                        overlay_visibility(now, inner.last_activity, idle, engaged);
                                    inner.visible = visible;
                                    let shown = inner.motion.value(
                                        VISIBLE_KEY,
                                        if visible { 1.0 } else { 0.0 },
                                        if visible {
                                            motion::SCROLLBAR_FADE_IN
                                        } else {
                                            motion::SCROLLBAR_FADE_OUT
                                        },
                                        motion::EASE,
                                        now,
                                    );
                                    let widen = inner.near || dragging;
                                    let expand = inner.motion.value(
                                        EXPAND_KEY,
                                        if widen { 1.0 } else { 0.0 },
                                        motion::HOVER,
                                        motion::EASE,
                                        now,
                                    );
                                    if inner.motion.any_active(now) {
                                        window.request_animation_frame();
                                    }
                                    // Nothing else repaints an idle window; wake once to fade.
                                    if let Some(deadline) = wake {
                                        if inner.wake_at != Some(deadline) {
                                            inner.wake_at = Some(deadline);
                                            let delay = deadline.saturating_duration_since(now);
                                            window
                                                .spawn(cx, async move |cx| {
                                                    cx.background_executor().timer(delay).await;
                                                    let _ = cx.update(|window, _| window.refresh());
                                                })
                                                .detach();
                                        }
                                    }
                                    drop(inner);
                                    if shown > 0.001 {
                                        let alpha = if dragging {
                                            m.thumb_drag_alpha
                                        } else {
                                            motion::lerp(m.thumb_alpha, m.thumb_hover_alpha, expand)
                                        };
                                        if expand > 0.001 {
                                            let track = Bounds::new(
                                                point(bounds.left() + px(2.0), bounds.top()),
                                                size(
                                                    (bounds.size.width - px(4.0)).max(px(1.0)),
                                                    bounds.size.height,
                                                ),
                                            );
                                            window.paint_quad(
                                                fill(
                                                    track,
                                                    style
                                                        .ink
                                                        .opacity(m.track_alpha * expand * shown),
                                                )
                                                // A radius past half the size draws nothing.
                                                .corner_radii(track.size.width / 2.0),
                                            );
                                        }
                                        let thumb_width = motion::lerp(
                                            m.thumb_width,
                                            m.thumb_expanded_width,
                                            expand,
                                        );
                                        let inset_right =
                                            motion::lerp(m.thumb_inset_right, 2.0, expand);
                                        let thumb_bounds = Bounds::new(
                                            point(
                                                bounds.right() - px(inset_right + thumb_width),
                                                bounds.top()
                                                    + px(metrics.geometry.thumb_top + THUMB_INSET),
                                            ),
                                            size(
                                                px(thumb_width.max(1.0)),
                                                px((metrics.geometry.thumb_height
                                                    - THUMB_INSET * 2.0)
                                                    .max(1.0)),
                                            ),
                                        );
                                        window.paint_quad(
                                            fill(thumb_bounds, style.ink.opacity(alpha * shown))
                                                .corner_radii(thumb_bounds.size.width / 2.0),
                                        );
                                    }
                                }
                            }
                            // These listeners run in capture phase even outside the
                            // track. GPUI's existing native mouse capture retains the
                            // gesture; releasing over a row never activates that row.
                            let move_state = state.clone();
                            let move_target = paint_target.clone();
                            let move_changed = paint_changed.clone();
                            window.on_mouse_event(
                                move |event: &MouseMoveEvent, phase, window, cx| {
                                    if phase != DispatchPhase::Capture {
                                        return;
                                    }
                                    let held = event.pressed_button == Some(MouseButton::Left);
                                    if move_state.move_pointer(
                                        generation,
                                        &move_target,
                                        f32::from(event.position.y),
                                        held,
                                        false,
                                    ) {
                                        cx.stop_propagation();
                                        window.refresh();
                                        move_changed(window, cx);
                                    } else {
                                        let mut inner = move_state.0.borrow_mut();
                                        let near = inner.bounds.contains(&event.position);
                                        if inner.near != near {
                                            inner.near = near;
                                            window.refresh();
                                        }
                                        let hovered = inner.metrics.is_some_and(|metrics| {
                                            inner.bounds.contains(&event.position)
                                                && metrics.geometry.contains_thumb(f32::from(
                                                    event.position.y - inner.bounds.top(),
                                                ))
                                        });
                                        if inner.hovered != hovered {
                                            inner.hovered = hovered;
                                            window.refresh();
                                        }
                                    }
                                },
                            );
                            let wheel_state = state.clone();
                            let wheel_target = paint_target.clone();
                            let wheel_changed = paint_changed.clone();
                            window.on_mouse_event(
                                move |event: &ScrollWheelEvent, phase, window, cx| {
                                    if phase != DispatchPhase::Capture {
                                        return;
                                    }
                                    // Popups and dialogs occlude the list and keep their wheel.
                                    if !viewport
                                        .as_ref()
                                        .is_some_and(|hitbox| hitbox.should_handle_scroll(window))
                                    {
                                        return;
                                    }
                                    // Div scroll areas glide in the bubble phase instead
                                    // (`smooth_wheel`), so nested wheel consumers win.
                                    let ScrollbarTarget::List(list) = &wheel_target else {
                                        return;
                                    };
                                    let ScrollDelta::Lines(lines) = event.delta else {
                                        return;
                                    };
                                    if !is_notched_wheel(lines.y)
                                        || !wheel_state.notch(
                                            generation,
                                            list,
                                            -lines.y * motion::WHEEL_LINE_PX,
                                            &wheel_changed,
                                            window,
                                        )
                                    {
                                        return;
                                    }
                                    cx.stop_propagation();
                                    window.refresh();
                                    wheel_changed(window, cx);
                                },
                            );
                            let up_state = state.clone();
                            let up_target = paint_target.clone();
                            let up_changed = paint_changed.clone();
                            window.on_mouse_event(
                                move |event: &MouseUpEvent, phase, window, cx| {
                                    if phase == DispatchPhase::Capture
                                        && event.button == MouseButton::Left
                                        && up_state.move_pointer(
                                            generation,
                                            &up_target,
                                            f32::from(event.position.y),
                                            true,
                                            true,
                                        )
                                    {
                                        cx.stop_propagation();
                                        window.refresh();
                                        up_changed(window, cx);
                                    }
                                },
                            );
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    fn move_pointer(
        &self,
        generation: u64,
        target: &ScrollbarTarget,
        pointer_y: f32,
        held: bool,
        finish: bool,
    ) -> bool {
        let mut state = self.0.borrow_mut();
        let Some(drag) = state.drag.as_ref().cloned() else {
            return false;
        };
        if drag.generation != generation || state.generation != Some(generation) {
            state.drag = None;
            return false;
        }
        if !drag.accepts(target, state.bounds) {
            state.drag = None;
            return false;
        }
        if !held || finish {
            state.drag = None;
        }
        let y = pointer_y - f32::from(state.bounds.top()) - drag.grab_y;
        let offset = drag.metrics.geometry.offset_for_top(y);
        if pointer_y.is_finite() && held {
            if let Some(active) = &mut state.drag {
                active.metrics.geometry.offset = offset;
                active.metrics.geometry.thumb_top =
                    offset / active.metrics.geometry.maximum * active.metrics.geometry.travel();
            }
            state.last_activity = Some(Instant::now());
        }
        drop(state);
        if pointer_y.is_finite() && held {
            target.apply(drag.metrics, offset);
        }
        true
    }

    /// Add a wheel notch of `distance` pixels to the list glide (positive
    /// scrolls toward the end). Returns false when this bar does not own it.
    fn notch(
        &self,
        generation: u64,
        list: &ListState,
        distance: f32,
        changed: &ScrollbarChanged,
        window: &mut Window,
    ) -> bool {
        let mut state = self.0.borrow_mut();
        if state.generation != Some(generation) || state.drag.is_some() || !distance.is_finite() {
            return false;
        }
        let now = Instant::now();
        state.last_activity = Some(now);
        if motion::reduced_motion() {
            state.smooth = None;
            drop(state);
            list.scroll_by_measured(px(distance));
            return true;
        }
        state.smooth = Some(match state.smooth {
            // A reversed wheel drops the rest of the old glide, like Chromium.
            Some(glide) if glide.remaining * distance >= 0.0 => SmoothScroll {
                remaining: glide.remaining + distance,
                ..glide
            },
            glide => SmoothScroll {
                remaining: distance,
                last_tick: glide.map_or(now, |glide| glide.last_tick),
                last_top: glide.and_then(|glide| glide.last_top),
            },
        });
        if !state.ticking {
            state.ticking = true;
            let (state, list, changed) = (self.clone(), list.clone(), changed.clone());
            window.on_next_frame(move |window, cx| state.glide(generation, list, changed, window, cx));
        }
        true
    }

    /// One glide frame. It runs before the frame's layout, so the list paints
    /// the new offset in the same frame instead of one frame late.
    fn glide(
        self,
        generation: u64,
        list: ListState,
        changed: ScrollbarChanged,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut state = self.0.borrow_mut();
        let glide = state
            .smooth
            .take()
            .filter(|_| state.generation == Some(generation) && state.drag.is_none())
            // Keyboard reveal, a drag or a refresh moved the list: yield to it.
            .filter(|glide| glide.last_top.is_none_or(|top| same_top(top, list.logical_scroll_top())));
        let Some(mut glide) = glide else {
            state.ticking = false;
            return;
        };
        let now = Instant::now();
        let step = motion::smooth_scroll_step(
            0.0,
            glide.remaining,
            now.saturating_duration_since(glide.last_tick),
        );
        let moved = f32::from(list.scroll_by_measured(px(step)));
        glide.remaining -= moved;
        glide.last_tick = now;
        glide.last_top = Some(list.logical_scroll_top());
        state.last_activity = Some(now);
        // Stopped by either end (rows ahead are measured by the overdraw).
        if moved.abs() < 0.01 || glide.remaining.abs() < 0.5 {
            state.ticking = false;
        } else {
            state.smooth = Some(glide);
            let (next, list, changed) = (self.clone(), list.clone(), changed.clone());
            window.on_next_frame(move |window, cx| next.glide(generation, list, changed, window, cx));
        }
        drop(state);
        if moved != 0.0 {
            window.refresh();
            changed(window, cx);
        }
    }
}

impl ScrollbarTarget {
    fn wheel(&self, metrics: Metrics, delta: ScrollDelta, line_height: Pixels) -> bool {
        match self {
            Self::List(list) => {
                // GPUI List converts line-wheel units with a fixed 20 DIP step.
                // Unknown rows have zero summary height; moving through measured
                // rows only keeps a large delta from skipping to the tail.
                let y = f32::from(delta.pixel_delta(px(20.0)).y);
                if !y.is_finite() || y == 0.0 {
                    return false;
                }
                list.scroll_by_measured(px(-y));
                true
            }
            Self::Scroll(scroll) => {
                // Match a vertical-only Div: line height comes from text style,
                // with the same horizontal-delta fallback as its wheel handler.
                let delta = delta.pixel_delta(line_height);
                let y = f32::from(if delta.y == px(0.0) { delta.x } else { delta.y });
                if !y.is_finite() || y == 0.0 {
                    return false;
                }
                let offset =
                    (f32::from(scroll.offset().y) + y).clamp(-metrics.geometry.maximum, 0.0);
                scroll.set_offset(point(scroll.offset().x, px(offset)));
                true
            }
        }
    }

    fn viewport_bounds(&self) -> Bounds<Pixels> {
        match self {
            Self::List(list) => list.viewport_bounds(),
            Self::Scroll(scroll) => scroll.bounds(),
        }
    }

    fn viewport_width(&self) -> f32 {
        match self {
            Self::List(list) => f32::from(list.viewport_bounds().size.width),
            Self::Scroll(scroll) => f32::from(scroll.bounds().size.width),
        }
    }

    fn count(&self) -> usize {
        match self {
            Self::List(list) => list.item_count(),
            Self::Scroll(scroll) => scroll.children_count(),
        }
    }

    fn metrics(&self, track_height: f32) -> Option<Metrics> {
        match self {
            Self::Scroll(scroll) => {
                let height = f32::from(scroll.bounds().size.height);
                Geometry::new(
                    track_height,
                    f32::from(scroll.max_offset().height),
                    -f32::from(scroll.offset().y),
                    height,
                )
                .map(|geometry| Metrics {
                    geometry,
                    mapping: Mapping::Pixels,
                })
            }
            Self::List(list) => {
                if list.item_count() == 0 {
                    return None;
                }
                let estimate = list.estimated_scroll()?;
                let page = f32::from(estimate.viewport_height);
                Geometry::new(
                    track_height,
                    f32::from(estimate.content_height) - page,
                    f32::from(estimate.offset),
                    page,
                )
                .map(|geometry| Metrics {
                    geometry,
                    mapping: Mapping::Estimated,
                })
            }
        }
    }

    fn apply(&self, metrics: Metrics, offset: f32) {
        let offset = offset.clamp(0.0, metrics.geometry.maximum);
        match (self, metrics.mapping) {
            (Self::Scroll(scroll), Mapping::Pixels) => {
                scroll.set_offset(point(scroll.offset().x, px(-offset)));
            }
            (Self::List(list), Mapping::Estimated) => {
                // The thumb's end always reaches the real last row, even when
                // the estimate grew during a drag; GPUI fills the final
                // viewport upward from it.
                if offset >= metrics.geometry.maximum - EPSILON {
                    list.scroll_to(ListOffset {
                        item_ix: list.item_count().saturating_sub(1),
                        offset_in_item: px(0.0),
                    });
                } else {
                    list.scroll_to_estimated_offset(px(offset));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dragging_list(count: usize) -> (ScrollbarState, ListState) {
        let state = ScrollbarState::default();
        let list = ListState::new(count, gpui::ListAlignment::Top, px(100.0));
        let metrics = Metrics {
            geometry: Geometry::new(300.0, count as f32 - 10.0, 0.0, 10.0).unwrap(),
            mapping: Mapping::Estimated,
        };
        {
            let mut inner = state.0.borrow_mut();
            inner.generation = Some(1);
            inner.bounds = Bounds::new(point(px(0.0), px(20.0)), size(px(12.0), px(300.0)));
            inner.metrics = Some(metrics);
            inner.drag = Some(Drag {
                generation: 1,
                grab_y: 5.0,
                metrics,
                viewport_width: 0.0,
                count,
            });
        }
        (state, list)
    }

    #[test]
    fn scrollbar_drag_survives_pagination_and_async_row_measurements() {
        // Mid-track mapping needs measured rows (covered by the GPUI list
        // tests); a layout-free list still exercises the drag lifecycle.
        let (state, list) = dragging_list(100);
        let target = ScrollbarTarget::List(list.clone());
        let bounds = state.0.borrow().bounds;
        assert!(state.move_pointer(1, &target, 400.0, true, false));
        let before_append = list.logical_scroll_top();
        assert_eq!(before_append.item_ix, 99);
        let before_metrics = state.update_metrics(&target, bounds).unwrap();
        list.splice(100..100, 100);
        state.invalidate_measurements();
        let after_metrics = state.update_metrics(&target, bounds).unwrap();
        assert!(state.is_dragging());
        assert_eq!(
            after_metrics.geometry.maximum,
            before_metrics.geometry.maximum
        );
        assert_eq!(
            after_metrics.geometry.thumb_top,
            before_metrics.geometry.thumb_top
        );
        assert_eq!(
            after_metrics.geometry.thumb_height,
            before_metrics.geometry.thumb_height
        );
        assert_eq!(list.logical_scroll_top().item_ix, before_append.item_ix);
        let anchor = list.logical_scroll_top();
        list.splice(anchor.item_ix..anchor.item_ix + 1, 1);
        list.scroll_to(anchor);
        state.invalidate_measurements();
        assert!(state.update_metrics(&target, bounds).is_some());
        // The thumb's end reaches the rows appended during the drag.
        assert!(state.move_pointer(1, &target, 400.0, true, false));
        assert_eq!(list.logical_scroll_top().item_ix, 199);
        assert!(state.move_pointer(1, &target, 50.0, true, true));
        assert!(!state.is_dragging());
        assert!(!state.move_pointer(1, &target, 250.0, true, false));

        let next_metrics = Metrics {
            geometry: Geometry::new(300.0, 190.0, 0.0, 10.0).unwrap(),
            mapping: Mapping::Estimated,
        };
        state.0.borrow_mut().drag = Some(Drag {
            generation: 1,
            grab_y: 5.0,
            metrics: next_metrics,
            viewport_width: 0.0,
            count: 200,
        });
        assert!(state.move_pointer(1, &target, 400.0, true, false));
        assert_eq!(list.logical_scroll_top().item_ix, 199);
        assert!(state.move_pointer(1, &target, 25.0, true, true));
        assert!(!state.is_dragging());
    }

    #[test]
    fn scrollbar_drag_still_cancels_on_removed_rows_resize_and_lost_button() {
        let (state, list) = dragging_list(100);
        let target = ScrollbarTarget::List(list.clone());
        let bounds = state.0.borrow().bounds;
        list.splice(50..100, 0);
        state.update_metrics(&target, bounds);
        assert!(!state.is_dragging());
        assert!(!state.move_pointer(1, &target, 200.0, true, false));

        let (state, list) = dragging_list(100);
        let target = ScrollbarTarget::List(list);
        let mut bounds = state.0.borrow().bounds;
        bounds.size.height = px(250.0);
        state.update_metrics(&target, bounds);
        assert!(!state.is_dragging());

        let (state, list) = dragging_list(100);
        let target = ScrollbarTarget::List(list.clone());
        assert!(state.move_pointer(1, &target, 200.0, false, false));
        assert!(!state.is_dragging());
        assert_eq!(list.logical_scroll_top().item_ix, 0);
    }

    #[test]
    fn scrollbar_geometry_omits_short_and_invalid_ranges() {
        for args in [
            (0.0, 10.0, 0.0, 5.0),
            (260.0, 0.0, 0.0, 260.0),
            (260.0, -1.0, 0.0, 260.0),
            (260.0, 100.0, f32::NAN, 260.0),
        ] {
            assert!(Geometry::new(args.0, args.1, args.2, args.3).is_none());
        }
    }

    #[test]
    fn scrollbar_geometry_reaches_both_ends_and_keeps_minimum_thumb() {
        let top = Geometry::new(260.0, 2340.0, 0.0, 260.0).unwrap();
        let bottom = Geometry::new(260.0, 2340.0, 2340.0, 260.0).unwrap();
        assert_eq!(top.thumb_top, 0.0);
        assert!((bottom.thumb_top + bottom.thumb_height - 260.0).abs() < 0.001);
        assert!((bottom.offset_for_top(bottom.thumb_top) - 2340.0).abs() < 0.001);
        assert_eq!(top.offset_for_top(-500.0), 0.0);
        assert_eq!(top.offset_for_top(5000.0), 2340.0);
        assert_eq!(
            Geometry::new(260.0, 100_000.0, 0.0, 260.0)
                .unwrap()
                .thumb_height,
            MIN_THUMB
        );
    }

    #[test]
    fn scrollbar_track_pages_without_moving_selection() {
        let metrics = Geometry::new(260.0, 1000.0, 500.0, 260.0).unwrap();
        assert_eq!(metrics.page_toward(0.0), 240.0);
        assert_eq!(metrics.page_toward(260.0), 760.0);
        assert_eq!(
            Geometry::new(260.0, 1000.0, 0.0, 260.0)
                .unwrap()
                .page_toward(-20.0),
            0.0
        );
    }

    #[test]
    fn scrollbar_unmeasured_tail_still_reports_the_last_loaded_index() {
        let list = ListState::new(20_000, gpui::ListAlignment::Top, px(100.0));
        assert_eq!(visible_end(&list), 1);
        list.scroll_to(ListOffset {
            item_ix: 19_999,
            offset_in_item: px(0.0),
        });
        assert_eq!(visible_end(&list), 20_000);
        list.reset(0);
        assert_eq!(visible_end(&list), 0);
    }

    #[test]
    fn scrollbar_handle_maps_signed_offsets_and_hides_nonoverflowing_content() {
        let scroll = ScrollHandle::new();
        assert!(ScrollbarTarget::Scroll(scroll.clone())
            .metrics(200.0)
            .is_none());
        scroll.set_offset(point(px(0.0), px(-200.0)));
        assert_eq!(scroll.offset().y, px(-200.0));
        let geometry = Geometry::new(200.0, 800.0, 200.0, 200.0).unwrap();
        ScrollbarTarget::Scroll(scroll.clone()).apply(
            Metrics {
                geometry,
                mapping: Mapping::Pixels,
            },
            400.0,
        );
        assert_eq!(scroll.offset().y, px(-400.0));
    }

    #[test]
    fn scrollbar_track_wheel_matches_list_and_div_delta_conventions() {
        let geometry = Geometry::new(200.0, 800.0, 200.0, 200.0).unwrap();
        let metrics = Metrics {
            geometry,
            mapping: Mapping::Pixels,
        };
        let scroll = ScrollHandle::new();
        scroll.set_offset(point(px(0.0), px(-200.0)));
        let target = ScrollbarTarget::Scroll(scroll.clone());
        assert!(target.wheel(metrics, ScrollDelta::Lines(point(0.0, -2.0)), px(15.0)));
        assert_eq!(scroll.offset().y, px(-230.0));
        assert!(target.wheel(
            metrics,
            ScrollDelta::Pixels(point(px(10.0), px(0.0))),
            px(15.0)
        ));
        assert_eq!(scroll.offset().y, px(-220.0));
        let list = ListState::new(100, gpui::ListAlignment::Top, px(100.0));
        assert!(ScrollbarTarget::List(list.clone()).wheel(
            metrics,
            ScrollDelta::Lines(point(0.0, -2.0)),
            px(15.0)
        ));
        // Before layout the measured pixel maximum is zero. A large wheel
        // delta must preserve the top instead of traversing zero-height rows.
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        assert!(!target.wheel(metrics, ScrollDelta::Lines(point(0.0, 0.0)), px(15.0)));
    }

    #[test]
    fn scrollbar_generation_cancel_retires_a_drag() {
        let state = ScrollbarState::default();
        let list = ListState::new(500, gpui::ListAlignment::Top, px(100.0));
        let metrics = Metrics {
            geometry: Geometry::new(300.0, 490.0, 0.0, 10.0).unwrap(),
            mapping: Mapping::Estimated,
        };
        state.0.borrow_mut().generation = Some(1);
        state.0.borrow_mut().drag = Some(Drag {
            generation: 1,
            grab_y: 5.0,
            metrics,
            viewport_width: 10.0,
            count: 500,
        });
        assert!(state.is_dragging());
        assert!(!state.move_pointer(2, &ScrollbarTarget::List(list.clone()), 250.0, true, false));
        assert!(!state.is_dragging());
        assert_eq!(list.logical_scroll_top().item_ix, 0);
    }

    #[test]
    fn overlay_shows_while_engaged_or_recently_active_then_idles_out() {
        let start = Instant::now();
        let idle = Duration::from_millis(900);
        assert_eq!(overlay_visibility(start, None, idle, false), (false, None));
        assert_eq!(overlay_visibility(start, None, idle, true), (true, None));
        let at = start + Duration::from_millis(100);
        assert_eq!(
            overlay_visibility(at, Some(start), idle, false),
            (true, Some(start + idle))
        );
        assert_eq!(
            overlay_visibility(start + idle, Some(start), idle, false),
            (false, None)
        );
    }

    #[test]
    fn whole_notches_animate_and_fractional_touchpad_deltas_pass_through() {
        assert!(is_notched_wheel(3.0));
        assert!(is_notched_wheel(-1.0));
        assert!(!is_notched_wheel(0.4));
        assert!(!is_notched_wheel(f32::NAN));
    }

    #[test]
    fn smooth_steps_approach_and_snap_to_the_target() {
        let frame = Duration::from_millis(16);
        let mut offset = 0.0;
        let mut frames = 0;
        while offset != 100.0 {
            let next = motion::smooth_scroll_step(offset, 100.0, frame);
            assert!(next > offset && next <= 100.0);
            offset = next;
            frames += 1;
            assert!(frames < 60, "never settled");
        }
        // Keep smoothing, but finish within 150 ms instead of trailing the
        // wheel for almost 400 ms. This is a response budget, not an FPS test.
        assert!((2..=9).contains(&frames), "{frames} frames");
        assert!(motion::smooth_scroll_step(0.0, 100.0, Duration::from_millis(60)) >= 90.0);
    }

    #[test]
    fn slow_frames_do_not_extend_the_wheel_tail() {
        // The same elapsed time must make the same progress, whether the
        // renderer delivered five frames or stalled on one image upload.
        let mut regular = 0.0;
        for _ in 0..5 {
            regular = motion::smooth_scroll_step(regular, 1000.0, Duration::from_millis(20));
        }
        let stalled = motion::smooth_scroll_step(0.0, 1000.0, Duration::from_millis(100));
        assert!((regular - stalled).abs() < 0.01);
        assert_eq!(motion::smooth_scroll_step(0.0, 1000.0, Duration::from_secs(2)), 1000.0);
        assert_eq!(motion::smooth_scroll_step(100.0, 0.0, Duration::ZERO), 100.0);
    }

    #[test]
    fn repeated_wheel_input_preserves_distance_without_a_long_backlog() {
        let frame = Duration::from_millis(16);
        let mut offset = 0.0;
        let mut target = 0.0;
        for _ in 0..60 {
            target += 100.0;
            offset = motion::smooth_scroll_step(offset, target, frame);
        }
        // Even an unusually fast continuous wheel stays within two notches.
        assert!(target - offset < 200.0);
        for _ in 0..10 {
            offset = motion::smooth_scroll_step(offset, target, frame);
        }
        assert_eq!(offset, target);
    }

    #[test]
    fn overlay_constants_follow_the_shared_tokens() {
        let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        assert_eq!(MIN_THUMB, tokens.scrollbar.min_thumb);
        assert_eq!(
            motion::SCROLLBAR_FADE_IN.as_millis() as f32,
            tokens.scrollbar.fade_in_ms
        );
        assert_eq!(
            motion::SCROLLBAR_FADE_OUT.as_millis() as f32,
            tokens.scrollbar.fade_out_ms
        );
    }
}
