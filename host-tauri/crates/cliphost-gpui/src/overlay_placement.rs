//! Painted QP geometry only. Core keeps placement, reveal and input ownership.
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn valid(self) -> bool {
        [self.left, self.top, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            && self.width > 0.0
            && self.height > 0.0
    }
    fn union(self, other: Self) -> Self {
        let left = self.left.min(other.left);
        let top = self.top.min(other.top);
        Self {
            left,
            top,
            width: (self.left + self.width).max(other.left + other.width) - left,
            height: (self.top + self.height).max(other.top + other.height) - top,
        }
    }
    fn intersection(self, other: Self) -> Option<Self> {
        let left = self.left.max(other.left);
        let top = self.top.max(other.top);
        let rect = Self {
            left,
            top,
            width: (self.left + self.width).min(other.left + other.width) - left,
            height: (self.top + self.height).min(other.top + other.height) - top,
        };
        (self.valid() && other.valid() && rect.valid()).then_some(rect)
    }
    pub fn rounded(self) -> [i32; 4] {
        [
            self.left.round() as i32,
            self.top.round() as i32,
            self.width.round() as i32,
            self.height.round() as i32,
        ]
    }
}

impl From<Bounds<Pixels>> for Rect {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            left: bounds.left().into(),
            top: bounds.top().into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Part {
    Panel,
    ActiveRow,
    Actions,
    Body,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Frame {
    pub panel: Option<Rect>,
    pub active_row: Option<Rect>,
    pub actions: Option<Rect>,
    pub body: Option<Rect>,
}

impl Frame {
    pub fn record(&mut self, part: Part, rect: Rect) {
        if !rect.valid() {
            return;
        }
        *match part {
            Part::Panel => &mut self.panel,
            Part::ActiveRow => &mut self.active_row,
            Part::Actions => &mut self.actions,
            Part::Body => &mut self.body,
        } = Some(rect);
    }
    pub fn cluster(&self, has_preview: bool, has_actions: bool) -> Option<Rect> {
        let mut bounds = self.panel?;
        if has_preview {
            bounds = bounds.union(self.body?);
        }
        if has_preview && has_actions {
            bounds = bounds.union(self.actions?);
        }
        Some(bounds)
    }
    pub fn visible_cluster(
        &self,
        has_preview: bool,
        has_actions: bool,
        viewport: [f32; 2],
    ) -> Option<Rect> {
        // Wait for required paints, but never claim the reserved/off-window
        // area as a wheel hit. Keep raw row/card boxes for vertical alignment.
        self.cluster(has_preview, has_actions)?;
        let viewport = Rect {
            left: 0.0,
            top: 0.0,
            width: viewport[0],
            height: viewport[1],
        };
        [
            self.panel,
            self.body.filter(|_| has_preview),
            self.actions.filter(|_| has_preview && has_actions),
        ]
        .into_iter()
        .flatten()
        .filter_map(|rect| rect.intersection(viewport))
        .reduce(Rect::union)
    }
    pub fn preview_offset(&self, has_actions: bool) -> Option<f32> {
        let panel = self.panel?;
        let row = self.active_row?;
        let mut preview = self.body?;
        if has_actions {
            preview = preview.union(self.actions?);
        }
        Some(
            (row.top - panel.top + row.height / 2.0 - preview.height / 2.0)
                .clamp(0.0, (panel.height - preview.height).max(0.0)),
        )
    }
}

/// Delegate layout/events unchanged, then report the child's real painted box.
/// No guessed row heights, synthetic screen coordinates or I/O in render.
pub(super) fn observe(
    child: AnyElement,
    report: Rc<dyn Fn(Rect, &mut Window, &mut App)>,
) -> AnyElement {
    PaintedBounds { child, report }.into_any_element()
}

struct PaintedBounds {
    child: AnyElement,
    report: Rc<dyn Fn(Rect, &mut Window, &mut App)>,
}
impl IntoElement for PaintedBounds {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for PaintedBounds {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        (self.report)(bounds.into(), window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(left: f32, top: f32, width: f32, height: f32) -> Rect {
        Rect {
            left,
            top,
            width,
            height,
        }
    }
    #[test]
    fn hit_cluster_uses_visible_cards_on_either_side_and_waits_for_required_paint() {
        let mut frame = Frame::default();
        frame.record(Part::Panel, rect(400.0, 16.0, 280.0, 300.0));
        assert!(frame.cluster(true, true).is_none());
        frame.record(Part::Body, rect(220.0, 72.0, 172.0, 90.0));
        frame.record(Part::Actions, rect(264.0, 44.0, 128.0, 24.0));
        assert_eq!(
            frame.cluster(true, true).unwrap(),
            rect(220.0, 16.0, 460.0, 300.0)
        );
        frame.body = Some(rect(688.0, 72.0, 172.0, 90.0));
        frame.actions = Some(rect(688.0, 44.0, 128.0, 24.0));
        assert_eq!(
            frame.cluster(true, true).unwrap(),
            rect(400.0, 16.0, 460.0, 300.0)
        );
        assert_eq!(frame.cluster(false, false).unwrap(), frame.panel.unwrap());
    }
    #[test]
    fn preview_tracks_measured_visible_row_and_clamps_at_both_panel_edges() {
        let mut frame = Frame {
            panel: Some(rect(10.0, 20.0, 280.0, 300.0)),
            body: Some(rect(300.0, 44.0, 200.0, 96.0)),
            actions: Some(rect(300.0, 20.0, 128.0, 20.0)),
            ..Default::default()
        };
        frame.active_row = Some(rect(10.0, 145.0, 280.0, 40.0));
        assert_eq!(frame.preview_offset(true), Some(85.0));
        frame.active_row = Some(rect(10.0, -10.0, 280.0, 40.0));
        assert_eq!(frame.preview_offset(true), Some(0.0));
        frame.active_row = Some(rect(10.0, 305.0, 280.0, 40.0));
        assert_eq!(frame.preview_offset(true), Some(180.0));
        frame.body = Some(rect(300.0, 20.0, 200.0, 400.0));
        assert_eq!(frame.preview_offset(false), Some(0.0));
    }
    #[test]
    fn zero_or_nonfinite_paint_cannot_signal_ready() {
        let mut frame = Frame::default();
        frame.record(Part::Panel, rect(0.0, 0.0, 0.0, 10.0));
        frame.record(Part::Panel, rect(f32::NAN, 0.0, 280.0, 300.0));
        assert!(frame.cluster(false, false).is_none());
    }

    #[test]
    fn high_dpi_narrow_viewport_hits_only_visible_cards_without_changing_row_alignment() {
        let mut frame = Frame {
            panel: Some(rect(10.0, 20.0, 280.0, 300.0)),
            active_row: Some(rect(10.0, 145.0, 280.0, 40.0)),
            body: Some(rect(300.0, 44.0, 200.0, 96.0)),
            actions: Some(rect(300.0, 20.0, 128.0, 20.0)),
        };
        assert_eq!(
            frame.visible_cluster(true, true, [400.0, 200.0]),
            Some(rect(10.0, 20.0, 390.0, 180.0))
        );
        assert_eq!(frame.preview_offset(true), Some(85.0));
        frame.panel = Some(rect(-300.0, 20.0, 280.0, 300.0));
        frame.body = Some(rect(500.0, 44.0, 200.0, 96.0));
        frame.actions = Some(rect(500.0, 20.0, 128.0, 20.0));
        assert!(frame.visible_cluster(true, true, [400.0, 200.0]).is_none());
    }
}
