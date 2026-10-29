//! Drop-in replacement for `gpui::svg()` that snaps icons to device pixels.
//!
//! GPUI 0.2.2 rasterises an SVG at twice its scaled size and centres the
//! sprite on the element bounds without rounding. At fractional DPI (125%)
//! a 14 px icon becomes a 17.5 px sprite placed on a half pixel, so every
//! stroke is resampled across two columns and looks blurred. Rounding the
//! painted size and origin to whole device pixels keeps the 2x tile an even
//! number of texels and lands it on the pixel grid; layout and hit testing
//! still use the unsnapped bounds.

use gpui::{
    point, px, size, App, Bounds, Element, ElementId, GlobalElementId, Hitbox,
    InspectorElementId, InteractiveElement, Interactivity, IntoElement, LayoutId, Pixels,
    SharedString, StyleRefinement, Styled, TransformationMatrix, Window,
};

pub(crate) struct CrispSvg {
    interactivity: Interactivity,
    path: Option<SharedString>,
}

/// Same builder surface as `gpui::svg()` (path, size, text colour, hover).
#[track_caller]
pub(crate) fn svg() -> CrispSvg {
    CrispSvg {
        interactivity: Interactivity::new(),
        path: None,
    }
}

impl CrispSvg {
    pub(crate) fn path(mut self, path: impl Into<SharedString>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// Device-pixel aligned bounds in logical pixels. The size is nudged just
/// below the rounded value because `paint_svg` takes `ceil(size * 2)`; float
/// noise above the integer would otherwise add one texel and a half-pixel
/// offset again.
pub(crate) fn snapped_bounds(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    if !scale.is_finite() || scale <= 0.0 {
        return bounds;
    }
    let width = (f32::from(bounds.size.width) * scale).round().max(1.0);
    let height = (f32::from(bounds.size.height) * scale).round().max(1.0);
    let center = bounds.center();
    let left = (f32::from(center.x) * scale - width / 2.0).round();
    let top = (f32::from(center.y) * scale - height / 2.0).round();
    const NUDGE: f32 = 0.001;
    Bounds {
        origin: point(px(left / scale), px(top / scale)),
        size: size(px((width - NUDGE) / scale), px((height - NUDGE) / scale)),
    }
}

impl Element for CrispSvg {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.interactivity.source_location()
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.interactivity.request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |style, window, cx| window.request_layout(style, None, cx),
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            bounds.size,
            window,
            cx,
            |_, _, hitbox, _, _| hitbox,
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let path = self.path.clone();
        self.interactivity.paint(
            global_id,
            inspector_id,
            bounds,
            hitbox.as_ref(),
            window,
            cx,
            |style, window, cx| {
                if let Some((path, color)) = path.zip(style.text.color) {
                    let snapped = snapped_bounds(bounds, window.scale_factor());
                    let _ = window.paint_svg(
                        snapped,
                        path,
                        TransformationMatrix::unit(),
                        color,
                        cx,
                    );
                }
            },
        )
    }
}

impl IntoElement for CrispSvg {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for CrispSvg {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.interactivity.base_style
    }
}

impl InteractiveElement for CrispSvg {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f32, y: f32, side: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(side), px(side)),
        }
    }

    /// Mirrors `Window::paint_svg`: tile = ceil(scaled * 2), origin = centre - tile / 4.
    fn painted(snapped: Bounds<Pixels>, scale: f32) -> (f32, f32) {
        let tile = (f32::from(snapped.size.width) * scale * 2.0).ceil();
        let centre = (f32::from(snapped.origin.x) + f32::from(snapped.size.width) / 2.0) * scale;
        (tile, centre - tile / 4.0)
    }

    #[test]
    fn fractional_scale_lands_on_whole_device_pixels() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for side in [14.0, 15.0, 16.0] {
                for x in [0.0, 3.3, 8.5, 101.75] {
                    let snapped = snapped_bounds(bounds(x, x, side), scale);
                    let (tile, left) = painted(snapped, scale);
                    assert_eq!(tile % 2.0, 0.0, "scale {scale} side {side} x {x}");
                    assert!((left - left.round()).abs() < 0.01, "scale {scale} side {side} x {x}: {left}");
                }
            }
        }
    }

    #[test]
    fn snapping_keeps_the_icon_centred_within_half_a_device_pixel() {
        let original = bounds(10.3, 4.0, 16.0);
        let snapped = snapped_bounds(original, 1.25);
        let delta = (f32::from(snapped.center().x) - f32::from(original.center().x)).abs() * 1.25;
        assert!(delta <= 0.5 + 0.01, "{delta}");
    }
}
