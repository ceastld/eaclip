//! Smooth mouse-wheel glide for ordinary `overflow_y_scroll` containers
//! (settings, help, dialogs, popups). WebView2 animates each wheel notch;
//! GPUI's Div jumps one text line per notch. Lists keep the capture-phase
//! glide in `scrollbar.rs`; this helper runs in the bubble phase so nested
//! wheel consumers (sliders, multiline inputs, inner scroll areas) still win.
//!
//! Display only: precision touchpads (pixel deltas) and reduced motion keep
//! GPUI's direct behavior, and any other offset change (keyboard reveal,
//! `scroll_to_item`, scrollbar drag) cancels the glide.

use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Instant};

use gpui::{
    point, px, App, Context, ElementId, EntityId, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement, Window,
};

use crate::{motion, scrollbar::is_notched_wheel};

#[derive(Clone, Copy, Debug)]
struct Glide {
    target: f32,
    /// Offset this glide last applied; any other change cancels it.
    last_set: f32,
    last_tick: Instant,
}

#[derive(Default)]
struct State {
    glide: Option<Glide>,
    ticking: bool,
    /// Line height Div's own wheel listener used. It reads the element's
    /// text style at paint time, which listeners cannot see; learned from
    /// notches that arrive mid-glide, when the previous offset is exact.
    line_height: Option<f32>,
}

/// Distance from the top before Div's own listener moved by one line step.
/// Mid-glide the previous offset is the one the glide applied, so the step
/// it took is learned; otherwise the learned (or window) line height is used.
fn offset_before_builtin(
    after: f32,
    lines_y: f32,
    gliding_from: Option<f32>,
    learned: &mut Option<f32>,
    fallback_line_height: f32,
) -> f32 {
    if let Some(previous) = gliding_from {
        let step = (previous - after) / lines_y;
        if step.is_finite() && (4.0..=128.0).contains(&step) {
            *learned = Some(step);
        }
        return previous;
    }
    after + lines_y * learned.unwrap_or(fallback_line_height)
}

/// Where the glide starts: the offset before Div's own step, kept in range.
/// The window line height is only a guess at that step (Div uses the
/// container's inherited text style). Out of range, the glide would start
/// where layout clamps and cancel itself on its first tick, swallowing every
/// notch at the top or bottom (custom category dialog, 12px text).
fn notch_origin(
    after: f32,
    lines_y: f32,
    gliding_from: Option<f32>,
    learned: &mut Option<f32>,
    fallback_line_height: f32,
    maximum: f32,
) -> f32 {
    offset_before_builtin(after, lines_y, gliding_from, learned, fallback_line_height)
        .clamp(0.0, maximum.max(0.0))
}

type Shared = Rc<RefCell<State>>;

/// Where a notch should glide to, as a positive distance from the top.
/// `None` when the container cannot move that way (lets the wheel chain to
/// an outer scroll area, like the browser at a scroll boundary).
pub(crate) fn notch_target(base: f32, current: f32, maximum: f32, lines_y: f32) -> Option<f32> {
    if ![base, current, maximum, lines_y]
        .iter()
        .all(|v| v.is_finite())
        || maximum < 0.0
    {
        return None;
    }
    let target = (base - lines_y * motion::WHEEL_LINE_PX).clamp(0.0, maximum);
    let moves = (target - base).abs() > f32::EPSILON || (target - current).abs() > 0.5;
    moves.then_some(target)
}

/// Slots kept per view; keys that include an open generation (popups,
/// menu columns) would otherwise accumulate for a long-lived view.
const MAX_SLOTS_PER_VIEW: usize = 32;

#[derive(Clone)]
struct Slot {
    state: Shared,
    handle: ScrollHandle,
    used: u64,
}

thread_local! {
    /// Glide state per owning entity and container key. Entries are dropped
    /// with their entity, so render code needs only `cx`, not `window`.
    static SLOTS: RefCell<(u64, HashMap<(EntityId, ElementId), Slot>)> =
        RefCell::new((0, HashMap::new()));
}

fn slot<V: 'static>(key: ElementId, cx: &mut Context<V>) -> (Shared, ScrollHandle) {
    let entity = cx.entity_id();
    let (slot, first) = SLOTS.with(|slots| {
        let (clock, slots) = &mut *slots.borrow_mut();
        *clock += 1;
        if let Some(slot) = slots.get_mut(&(entity, key.clone())) {
            slot.used = *clock;
            return (slot.clone(), false);
        }
        let mine: Vec<_> = slots
            .iter()
            .filter(|((id, _), _)| *id == entity)
            .map(|(key, slot)| (slot.used, key.clone()))
            .collect();
        if mine.len() >= MAX_SLOTS_PER_VIEW {
            if let Some((_, oldest)) = mine.into_iter().min_by_key(|(used, _)| *used) {
                slots.remove(&oldest);
            }
        }
        let first = !slots.keys().any(|(id, _)| *id == entity);
        let slot = Slot {
            state: Shared::default(),
            handle: ScrollHandle::new(),
            used: *clock,
        };
        slots.insert((entity, key), slot.clone());
        (slot, first)
    });
    if first {
        cx.on_release(move |_, _| {
            SLOTS.with(|slots| slots.borrow_mut().1.retain(|(id, _), _| *id != entity));
        })
        .detach();
    }
    (slot.state, slot.handle)
}

pub(crate) trait SmoothWheelExt: StatefulInteractiveElement + Sized {
    /// Animate whole wheel notches on this `overflow_y_scroll` element.
    /// Call on the element that tracks `handle`, before any other wheel
    /// listener on it (later listeners run first and may stop the event);
    /// `key` must be unique within the owning view.
    fn smooth_wheel<V: 'static>(
        self,
        key: impl Into<ElementId>,
        handle: &ScrollHandle,
        cx: &mut Context<V>,
    ) -> Self {
        let (state, _) = slot(key.into(), cx);
        let handle = handle.clone();
        self.on_scroll_wheel(move |event, window, cx| wheel(&state, &handle, event, window, cx))
    }

    /// `smooth_wheel` for a container without a scroll handle of its own:
    /// keeps one per view and key, and tracks it.
    /// Include an open generation in `key` for containers that come and go,
    /// so a reopened popup starts at the top like a fresh element would.
    fn smooth_scroll<V: 'static>(self, key: impl Into<ElementId>, cx: &mut Context<V>) -> Self {
        let key = key.into();
        let (_, handle) = slot(key.clone(), cx);
        self.track_scroll(&handle).smooth_wheel(key, &handle, cx)
    }
}

impl<E: StatefulInteractiveElement> SmoothWheelExt for E {}

fn wheel(
    state: &Shared,
    handle: &ScrollHandle,
    event: &ScrollWheelEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let ScrollDelta::Lines(lines) = event.delta else {
        return;
    };
    if lines.x != 0.0 || !is_notched_wheel(lines.y) {
        return;
    }
    // Div's own scroll listener runs first in the bubble phase and has already
    // moved by one text line per notch; take that jump back before gliding.
    let offset = handle.offset();
    let maximum = f32::from(handle.max_offset().height);
    let (current, target) = {
        let mut guard = state.borrow_mut();
        let inner = &mut *guard;
        let current = notch_origin(
            -f32::from(offset.y),
            lines.y,
            inner.glide.map(|glide| glide.last_set),
            &mut inner.line_height,
            f32::from(window.line_height()),
            maximum,
        );
        let base = inner.glide.map_or(current, |glide| glide.target);
        (current, notch_target(base, current, maximum, lines.y))
    };
    let Some(target) = target else {
        return;
    };
    cx.stop_propagation();
    start(state, handle, current, target, window);
}

/// Browser-style page step: 87.5% of the viewport (Chromium's paging fraction).
pub(crate) fn page_step(viewport_height: f32) -> f32 {
    if viewport_height.is_finite() && viewport_height > 0.0 {
        viewport_height * 0.875
    } else {
        0.0
    }
}

/// Glide the container registered under `key` by `delta` pixels (positive
/// scrolls down), continuing any glide in progress. Returns whether it moved.
pub(crate) fn glide_by<V: 'static>(
    key: impl Into<ElementId>,
    handle: &ScrollHandle,
    delta: f32,
    window: &mut Window,
    cx: &mut Context<V>,
) -> bool {
    let (state, _) = slot(key.into(), cx);
    let current = -f32::from(handle.offset().y);
    let maximum = f32::from(handle.max_offset().height);
    if !delta.is_finite() || !current.is_finite() || !maximum.is_finite() || maximum <= 0.0 {
        return false;
    }
    let base = state.borrow().glide.map_or(current, |glide| glide.target);
    let target = (base + delta).clamp(0.0, maximum);
    if (target - base).abs() < 0.5 && (target - current).abs() < 0.5 {
        return false;
    }
    start(&state, handle, current, target, window);
    true
}

/// Move to an absolute positive distance from the top. A new anchor replaces
/// the previous destination, including a wheel glide that is still running.
pub(crate) fn glide_to<V: 'static>(
    key: impl Into<ElementId>,
    handle: &ScrollHandle,
    target: f32,
    window: &mut Window,
    cx: &mut Context<V>,
) -> bool {
    let current = -f32::from(handle.offset().y);
    let Some(target) = absolute_target(target, f32::from(handle.max_offset().height)) else {
        return false;
    };
    if !current.is_finite() {
        return false;
    }
    let (state, _) = slot(key.into(), cx);
    // Reset last_set as well as the destination: another writer may have
    // scrolled since the old glide. The already scheduled tick can be reused.
    state.borrow_mut().glide = None;
    if (target - current).abs() < 0.5 {
        handle.set_offset(point(handle.offset().x, px(-target)));
        window.refresh();
        return false;
    }
    start(&state, handle, current, target, window);
    true
}

pub(crate) fn cancel_glide<V: 'static>(key: impl Into<ElementId>, cx: &mut Context<V>) {
    let (state, _) = slot(key.into(), cx);
    state.borrow_mut().glide = None;
}

fn absolute_target(target: f32, maximum: f32) -> Option<f32> {
    (target.is_finite() && maximum.is_finite() && maximum >= 0.0)
        .then(|| target.clamp(0.0, maximum))
}

/// Begin (or retarget) a glide from `current` toward `target`.
fn start(state: &Shared, handle: &ScrollHandle, current: f32, target: f32, window: &mut Window) {
    let offset = handle.offset();
    let mut inner = state.borrow_mut();
    if motion::reduced_motion() {
        inner.glide = None;
        handle.set_offset(point(offset.x, px(-target)));
        window.refresh();
        return;
    }
    handle.set_offset(point(offset.x, px(-current)));
    let now = Instant::now();
    inner.glide = Some(match inner.glide {
        Some(glide) => Glide { target, ..glide },
        None => Glide {
            target,
            last_set: current,
            last_tick: now,
        },
    });
    if !inner.ticking {
        inner.ticking = true;
        let (state, handle) = (state.clone(), handle.clone());
        window.on_next_frame(move |window, cx| tick(state, handle, window, cx));
    }
    window.refresh();
}

fn tick(state: Shared, handle: ScrollHandle, window: &mut Window, _cx: &mut App) {
    let mut inner = state.borrow_mut();
    let Some(mut glide) = inner.glide.take() else {
        inner.ticking = false;
        return;
    };
    let offset = handle.offset();
    let current = -f32::from(offset.y);
    let maximum = f32::from(handle.max_offset().height);
    // Something else moved the content (keyboard reveal, scrollbar, reset): yield.
    if !current.is_finite() || !maximum.is_finite() || (current - glide.last_set).abs() > 1.0 {
        inner.ticking = false;
        return;
    }
    let now = Instant::now();
    let target = glide.target.clamp(0.0, maximum.max(0.0));
    let next = motion::smooth_scroll_step(
        current,
        target,
        now.saturating_duration_since(glide.last_tick),
    );
    handle.set_offset(point(offset.x, px(-next)));
    window.refresh();
    if (target - next).abs() > f32::EPSILON {
        glide.target = target;
        glide.last_set = next;
        glide.last_tick = now;
        inner.glide = Some(glide);
        drop(inner);
        window.on_next_frame(move |window, cx| tick(state, handle, window, cx));
    } else {
        inner.ticking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notch_moves_one_webview_step_and_clamps() {
        let step = motion::WHEEL_LINE_PX;
        // GPUI wheel-down is negative lines.
        assert_eq!(notch_target(0.0, 0.0, 500.0, -1.0), Some(step));
        assert_eq!(notch_target(100.0, 90.0, 500.0, 3.0), Some(0.0));
        assert_eq!(notch_target(480.0, 470.0, 500.0, -1.0), Some(500.0));
    }

    #[test]
    fn boundary_notch_chains_to_parent() {
        assert_eq!(notch_target(0.0, 0.0, 500.0, 1.0), None);
        assert_eq!(notch_target(500.0, 500.0, 500.0, -1.0), None);
        assert_eq!(notch_target(0.0, 0.0, 0.0, -1.0), None);
        // Still gliding back to the edge: keep the wheel.
        assert_eq!(notch_target(0.0, 40.0, 500.0, 1.0), Some(0.0));
    }

    #[test]
    fn edge_notch_with_smaller_container_line_still_moves() {
        // 12px text: Div stepped 19px, the window guess is 26px.
        let mut learned = None;
        let origin = notch_origin(19.0, -1.0, None, &mut learned, 26.0, 85.6);
        assert_eq!(origin, 0.0);
        assert_eq!(notch_target(origin, origin, 85.6, -1.0), Some(motion::WHEEL_LINE_PX));
        // Scrolling up from the bottom: Div stepped back 19px.
        let origin = notch_origin(85.6 - 19.0, 1.0, None, &mut learned, 26.0, 85.6);
        assert_eq!(origin, 85.6);
        assert!(notch_target(origin, origin, 85.6, 1.0).is_some_and(|target| target < 85.6));
    }

    #[test]
    fn builtin_line_step_is_undone_and_learned() {
        let mut learned = None;
        // Not gliding: fall back to the window line height.
        assert_eq!(
            offset_before_builtin(80.0, -1.0, None, &mut learned, 20.0),
            60.0
        );
        // Mid-glide the exact previous offset reveals Div's 22px step.
        assert_eq!(
            offset_before_builtin(122.0, -1.0, Some(100.0), &mut learned, 20.0),
            100.0
        );
        assert_eq!(learned, Some(22.0));
        assert_eq!(
            offset_before_builtin(78.0, 1.0, None, &mut learned, 20.0),
            100.0
        );
        // Nonsense steps (another writer moved the content) are not learned.
        offset_before_builtin(900.0, -1.0, Some(100.0), &mut learned, 20.0);
        assert_eq!(learned, Some(22.0));
    }

    #[test]
    fn page_step_matches_chromium_fraction() {
        assert_eq!(page_step(400.0), 350.0);
        assert_eq!(page_step(0.0), 0.0);
        assert_eq!(page_step(f32::NAN), 0.0);
    }

    #[test]
    fn invalid_geometry_is_ignored() {
        assert_eq!(notch_target(f32::NAN, 0.0, 500.0, -1.0), None);
        assert_eq!(notch_target(0.0, 0.0, -1.0, -1.0), None);
    }

    #[test]
    fn absolute_anchor_destinations_replace_instead_of_accumulating() {
        assert_eq!(absolute_target(400.0, 800.0), Some(400.0));
        assert_eq!(absolute_target(120.0, 800.0), Some(120.0));
        assert_eq!(absolute_target(900.0, 800.0), Some(800.0));
        assert_eq!(absolute_target(-30.0, 800.0), Some(0.0));
        assert_eq!(absolute_target(30.0, 0.0), Some(0.0));
        assert_eq!(absolute_target(f32::NAN, 800.0), None);
        assert_eq!(absolute_target(30.0, f32::INFINITY), None);
        assert_eq!(absolute_target(30.0, -1.0), None);
    }
}
