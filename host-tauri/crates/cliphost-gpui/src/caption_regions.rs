//! Painted caption hit regions for owned GPUI HWNDs. Win32 reads this cache
//! without entering GPUI's App or consulting the current mouse hover state.
//!
//! The titlebar's parent canvas publishes begin before navigation/button
//! canvases publish exclude in the same prepaint phase. All coordinates are
//! viewport/client logical DIPs; callers clip canvas bounds to the content mask.
//! Call invalidate on geometry/DPI changes and remove on WM_NCDESTROY.

use std::{cell::RefCell, collections::HashMap};

use gpui::{Bounds, Pixels, Size};

#[derive(Clone, Copy, Debug)]
struct Rect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl Rect {
    fn from_bounds(bounds: Bounds<Pixels>) -> Option<Self> {
        let left = f64::from(f32::from(bounds.origin.x));
        let top = f64::from(f32::from(bounds.origin.y));
        let width = f64::from(f32::from(bounds.size.width));
        let height = f64::from(f32::from(bounds.size.height));
        if ![left, top, width, height]
            .iter()
            .all(|value| value.is_finite())
            || width < 0.0
            || height < 0.0
        {
            return None;
        }
        Some(Self {
            left,
            top,
            right: f64::from(f32::from(bounds.right())),
            bottom: f64::from(f32::from(bounds.bottom())),
        })
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let clipped = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (clipped.left < clipped.right && clipped.top < clipped.bottom).then_some(clipped)
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    fn covers(self, x: f64, y: f64) -> bool {
        // GPUI Bounds::contains includes right/bottom edges. Keep those
        // interactive control boundaries out of the native caption too.
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

struct CaptionFrame {
    title: Option<Rect>,
    exclusions: Vec<Rect>,
    width: i32,
    height: i32,
    dpi: u32,
    valid: bool,
}

impl CaptionFrame {
    fn new(bounds: Bounds<Pixels>, viewport: Size<Pixels>, scale: f32) -> Option<Self> {
        let logical_width = f64::from(f32::from(viewport.width));
        let logical_height = f64::from(f32::from(viewport.height));
        let scale = f64::from(scale);
        if ![logical_width, logical_height, scale]
            .iter()
            .all(|value| value.is_finite())
            || logical_width <= 0.0
            || logical_height <= 0.0
            || scale <= 0.0
            || scale > 16.0
        {
            return None;
        }
        let width = (logical_width * scale).round();
        let height = (logical_height * scale).round();
        if width < 1.0
            || height < 1.0
            || width > f64::from(i32::MAX)
            || height > f64::from(i32::MAX)
        {
            return None;
        }
        let dpi = (scale * 96.0).round() as u32;
        if dpi == 0 {
            return None;
        }
        let viewport = Rect {
            left: 0.0,
            top: 0.0,
            right: logical_width,
            bottom: logical_height,
        };
        Some(Self {
            title: Rect::from_bounds(bounds)?.intersect(viewport),
            exclusions: Vec::new(),
            width: width as i32,
            height: height as i32,
            dpi,
            valid: true,
        })
    }

    fn exclude(&mut self, bounds: Bounds<Pixels>) {
        let Some(rect) = Rect::from_bounds(bounds) else {
            // A malformed control cannot silently become a draggable caption.
            self.valid = false;
            return;
        };
        if let Some(rect) = self.title.and_then(|title| title.intersect(rect)) {
            self.exclusions.push(rect);
        }
    }

    fn hit(&self, x: i32, y: i32, width: i32, height: i32, dpi: u32) -> Option<bool> {
        if !self.valid || self.width != width || self.height != height || self.dpi != dpi {
            return None;
        }
        if x < 0 || y < 0 || x >= width || y >= height {
            return Some(false);
        }
        // DPI/viewport equality above retires a previous monitor's layout.
        // Use the actual current DPI to convert the native client point.
        let scale = dpi as f32 / 96.0;
        // GPUI's Windows logical_point uses this same f32 division.
        let (x, y) = (f64::from(x as f32 / scale), f64::from(y as f32 / scale));
        Some(
            self.title.is_some_and(|title| title.contains(x, y))
                && !self.exclusions.iter().any(|rect| rect.covers(x, y)),
        )
    }
}

thread_local! {
    // Callers register only their own HWNDs on the owning UI thread. No Win32
    // calls or user input happen while this map is borrowed.
    static FRAMES: RefCell<HashMap<isize, CaptionFrame>> = RefCell::new(HashMap::new());
}

/// Publish the painted titlebar and start a new exclusion set for this frame.
/// Invalid geometry clears an older frame instead of keeping stale hit areas.
pub(crate) fn begin(hwnd: isize, bounds: Bounds<Pixels>, viewport: Size<Pixels>, scale: f32) {
    if hwnd == 0 {
        return;
    }
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        if let Some(frame) = CaptionFrame::new(bounds, viewport, scale) {
            frames.insert(hwnd, frame);
        } else {
            frames.remove(&hwnd);
        }
    });
}

/// Exclude the current painted navigation/button rectangle, clipped to caption.
pub(crate) fn exclude(hwnd: isize, bounds: Bounds<Pixels>) {
    FRAMES.with(|frames| {
        if let Some(frame) = frames.borrow_mut().get_mut(&hwnd) {
            frame.exclude(bounds);
        }
    });
}

/// None means no current painted cache. Some(true) is caption and Some(false)
/// is ordinary client; the caller checks resize borders before consulting this.
pub(crate) fn hit(
    hwnd: isize,
    client_x: i32,
    client_y: i32,
    client_w: i32,
    client_h: i32,
    dpi: u32,
) -> Option<bool> {
    FRAMES.with(|frames| {
        frames
            .borrow()
            .get(&hwnd)?
            .hit(client_x, client_y, client_w, client_h, dpi)
    })
}

/// Retire painted geometry before the next layout, retaining no active regions.
pub(crate) fn invalidate(hwnd: isize) {
    FRAMES.with(|frames| {
        if let Some(frame) = frames.borrow_mut().get_mut(&hwnd) {
            frame.valid = false;
            frame.exclusions.clear();
        }
    });
}

/// HWND destruction must remove the cache before Windows can reuse its value.
pub(crate) fn remove(hwnd: isize) {
    FRAMES.with(|frames| {
        frames.borrow_mut().remove(&hwnd);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn caption_empty_area_moves_but_navigation_buttons_and_body_stay_client() {
        let hwnd = 11;
        begin(
            hwnd,
            bounds(0., 0., 400., 36.),
            size(px(400.), px(300.)),
            1.25,
        );
        exclude(hwnd, bounds(0., 0., 100., 36.));
        exclude(hwnd, bounds(300., -5., 200., 50.)); // clipped to title/client
        assert_eq!(hit(hwnd, 124, 20, 500, 375, 120), Some(false));
        assert_eq!(hit(hwnd, 125, 20, 500, 375, 120), Some(false));
        assert_eq!(hit(hwnd, 126, 20, 500, 375, 120), Some(true));
        assert_eq!(hit(hwnd, 374, 20, 500, 375, 120), Some(true));
        assert_eq!(hit(hwnd, 375, 20, 500, 375, 120), Some(false));
        assert_eq!(hit(hwnd, 200, 44, 500, 375, 120), Some(true));
        assert_eq!(hit(hwnd, 200, 45, 500, 375, 120), Some(false));
        assert_eq!(hit(hwnd, -1, 20, 500, 375, 120), Some(false));
        assert_eq!(hit(hwnd, 20, -1, 500, 375, 120), Some(false));
        assert_eq!(hit(hwnd, 500, 20, 500, 375, 120), Some(false));
        remove(hwnd);
    }

    #[test]
    fn new_viewport_or_dpi_falls_back_until_a_new_frame_is_painted() {
        let hwnd = 12;
        begin(
            hwnd,
            bounds(0., 0., 400., 36.),
            size(px(400.), px(300.)),
            1.,
        );
        assert_eq!(hit(hwnd, 150, 20, 400, 300, 96), Some(true));
        assert_eq!(hit(hwnd, 150, 20, 401, 300, 96), None);
        assert_eq!(hit(hwnd, 150, 20, 400, 301, 96), None);
        assert_eq!(hit(hwnd, 150, 20, 400, 300, 120), None);
        begin(
            hwnd,
            bounds(0., 0., 400., 36.),
            size(px(400.), px(300.)),
            1.5,
        );
        assert_eq!(hit(hwnd, 150, 20, 600, 450, 144), Some(true));
        invalidate(hwnd);
        assert_eq!(hit(hwnd, 150, 20, 600, 450, 144), None);
        remove(hwnd);
    }

    #[test]
    fn repaint_clears_exclusions_and_destroyed_handle_never_reuses_old_geometry() {
        let hwnd = 13;
        begin(
            hwnd,
            bounds(0., 0., 400., 36.),
            size(px(400.), px(300.)),
            1.,
        );
        exclude(hwnd, bounds(0., 0., 100., 36.));
        assert_eq!(hit(hwnd, 20, 20, 400, 300, 96), Some(false));
        begin(
            hwnd,
            bounds(0., 0., 400., 36.),
            size(px(400.), px(300.)),
            1.,
        );
        assert_eq!(hit(hwnd, 20, 20, 400, 300, 96), Some(true));
        remove(hwnd);
        assert_eq!(hit(hwnd, 20, 20, 400, 300, 96), None);
        exclude(hwnd, bounds(0., 0., 100., 36.));
        assert_eq!(hit(hwnd, 20, 20, 400, 300, 96), None);
        begin(
            hwnd,
            bounds(0., 20., 400., 16.),
            size(px(400.), px(300.)),
            1.,
        );
        assert_eq!(hit(hwnd, 20, 10, 400, 300, 96), Some(false));
        assert_eq!(hit(hwnd, 20, 20, 400, 300, 96), Some(true));
        remove(hwnd);
    }

    #[test]
    fn malformed_paint_cannot_keep_a_previous_draggable_region() {
        let hwnd = 14;
        begin(
            hwnd,
            bounds(0., 0., 400., 36.),
            size(px(400.), px(300.)),
            1.,
        );
        exclude(hwnd, bounds(f32::NAN, 0., 100., 36.));
        assert_eq!(hit(hwnd, 150, 20, 400, 300, 96), None);
        for scale in [0., -1., f32::NAN, f32::INFINITY] {
            begin(
                hwnd,
                bounds(0., 0., 400., 36.),
                size(px(400.), px(300.)),
                scale,
            );
            assert_eq!(hit(hwnd, 150, 20, 400, 300, 96), None);
        }
        begin(hwnd, bounds(0., 0., 400., 36.), size(px(0.), px(300.)), 1.);
        assert_eq!(hit(hwnd, 150, 20, 400, 300, 96), None);
        begin(0, bounds(0., 0., 400., 36.), size(px(400.), px(300.)), 1.);
        assert_eq!(hit(0, 150, 20, 400, 300, 96), None);
        remove(hwnd);
    }

    fn paint_titlebar_search(hwnd: isize, width: f32, height: f32, scale: f32) {
        let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        let l = &tokens.layout;
        let titlebar = tokens.titlebar_search;
        let sizing = crate::titlebar_layout::titlebar_layout(width, &tokens);
        begin(
            hwnd,
            bounds(0., 0., width, l.title_bar_height),
            size(px(width), px(height)),
            scale,
        );
        let button_y = (l.title_bar_height - titlebar.button_size) / 2.0;
        for index in 0..2 {
            exclude(
                hwnd,
                bounds(
                    l.title_bar_padding_x + index as f32 * (titlebar.button_size + l.chip_gap),
                    button_y,
                    titlebar.button_size,
                    titlebar.button_size,
                ),
            );
        }
        let search_left = l.title_bar_padding_x
            + 2.0 * titlebar.button_size
            + l.chip_gap
            + l.title_bar_gap;
        exclude(
            hwnd,
            bounds(
                search_left,
                (l.title_bar_height - l.search_height) / 2.0,
                sizing.search_width,
                l.search_height,
            ),
        );
        let controls_left = width - l.title_bar_padding_x
            - 3.0 * titlebar.button_size
            - 2.0 * l.chrome_button_gap;
        for index in 0..3 {
            exclude(
                hwnd,
                bounds(
                    controls_left + index as f32 * (titlebar.button_size + l.chrome_button_gap),
                    button_y,
                    titlebar.button_size,
                    titlebar.button_size,
                ),
            );
        }
    }

    #[test]
    fn titlebar_search_excludes_whole_search_and_buttons_but_keeps_gaps_draggable_at_each_dpi() {
        let hwnd = 15;
        let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        for width in [300., 320., 420., 560.] {
            let sizing = crate::titlebar_layout::titlebar_layout(width, &tokens);
            let search_left = 62.0;
            let search_right = search_left + sizing.search_width;
            let controls_left = width - 82.0;
            for scale in [1.0, 1.25, 1.5] {
                paint_titlebar_search(hwnd, width, 640., scale);
                let physical_width = (width * scale).round() as i32;
                let physical_height = (640.0 * scale).round() as i32;
                let dpi = (96.0 * scale).round() as u32;
                let hit_at = |x: f32, y: f32| {
                    hit(
                        hwnd,
                        (x * scale).round() as i32,
                        (y * scale).round() as i32,
                        physical_width,
                        physical_height,
                        dpi,
                    )
                };
                for (x, y) in [
                    (18., 18.),
                    (44., 18.),
                    (search_left + 1.0, 18.), // Left padding before the icon.
                    (search_left + 8.0, 18.), // Search icon, outside the text input.
                    (search_left + 30.0, 5.), // Search frame's top padding.
                    (search_right - 1.0, 18.), // Filter and right padding.
                    (controls_left + 12.0, 18.),
                    (controls_left + 38.0, 18.),
                    (controls_left + 64.0, 18.),
                ] {
                    assert_eq!(
                        hit_at(x, y),
                        Some(false),
                        "Client at {width}/{scale}: {x},{y}"
                    );
                }
                for x in [
                    31.0, // Between navigation buttons.
                    59.0, // Navigation/search gap.
                    search_right + 3.0, // Search/drag gap.
                    search_right + 6.0 + sizing.drag_width / 2.0,
                    controls_left - 3.0, // Drag/window-controls gap.
                    controls_left + 25.0, // Between window buttons.
                ] {
                    assert_eq!(
                        hit_at(x, 18.),
                        Some(true),
                        "Caption at {width}/{scale}: {x}"
                    );
                }
                assert_eq!(hit_at(search_left + 30.0, 40.), Some(false));
            }
        }
        remove(hwnd);
    }

    #[test]
    fn folded_titlebar_repaint_preserves_search_client_and_empty_caption_regions() {
        let hwnd = 16;
        paint_titlebar_search(hwnd, 420., 640., 1.25);
        assert_eq!(hit(hwnd, 330, 22, 525, 800, 120), Some(true));
        // The expanded layout must retire until the folded viewport repaints.
        assert_eq!(hit(hwnd, 330, 22, 525, 45, 120), None);
        paint_titlebar_search(hwnd, 420., 36., 1.25);
        assert_eq!(hit(hwnd, 80, 22, 525, 45, 120), Some(false));
        assert_eq!(hit(hwnd, 330, 22, 525, 45, 120), Some(true));
        assert_eq!(hit(hwnd, 330, 44, 525, 45, 120), Some(true));
        assert_eq!(hit(hwnd, 330, 45, 525, 45, 120), Some(false));
        remove(hwnd);
    }

    #[test]
    fn titlebar_caption_cache_keeps_resize_edges_available_for_the_existing_priority_rule() {
        use cliphost_core::window_chrome::{
            hit_test_client_area, HTCLIENT, HTLEFT, HTTOP, HTTOPLEFT,
        };
        let hwnd = 17;
        paint_titlebar_search(hwnd, 420., 640., 1.0);
        // Caption caches paint geometry only. The native handler checks the
        // existing 3-DIP clipboard resize edge before consulting these hits.
        for (x, y, resize) in [(1, 1, HTTOPLEFT), (100, 1, HTTOP), (1, 18, HTLEFT)] {
            assert_eq!(hit(hwnd, x, y, 420, 640, 96), Some(true));
            assert_eq!(hit_test_client_area(x, y, 420, 640, 3), resize);
        }
        assert_eq!(hit_test_client_area(80, 18, 420, 640, 3), HTCLIENT);
        assert_eq!(hit(hwnd, 80, 18, 420, 640, 96), Some(false));
        remove(hwnd);
    }
}
