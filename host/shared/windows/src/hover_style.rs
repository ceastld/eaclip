//! Cached, premultiplied BGRA frame for the native layered hover popup.

const SHADOW_DIP: f64 = 8.0;
const RADIUS_DIP: f64 = 8.0;
const BORDER_DIP: f64 = 1.0;

fn valid_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 { scale } else { 1.0 }
}

pub(super) fn shadow_padding(scale: f64) -> i32 {
    (SHADOW_DIP * valid_scale(scale)).ceil().max(1.0) as i32
}

pub(super) fn draw_frame(
    width: i32,
    height: i32,
    scale: f64,
    dark: bool,
    pixels: &mut [[u8; 4]],
) {
    pixels.fill([0; 4]);
    if width <= 0 || height <= 0
        || (width as usize).checked_mul(height as usize) != Some(pixels.len())
    {
        return;
    }
    let scale = valid_scale(scale);
    let padding = shadow_padding(scale) as f64;
    let left = padding;
    let top = padding;
    let right = width as f64 - padding;
    let bottom = height as f64 - padding;
    if right <= left || bottom <= top {
        return;
    }
    let radius = (RADIUS_DIP * scale).min((right - left) * 0.5).min((bottom - top) * 0.5);
    let border = BORDER_DIP * scale;
    let body = if dark { [27, 27, 27, 255] } else { [250, 250, 250, 255] };
    let outline = if dark { [82.0, 75.0, 72.0] } else { [222.0, 216.0, 212.0] };
    let shadow_alpha = if dark { 28.0 / 255.0 } else { 22.0 / 255.0 };
    // Stop inside the margin so every outermost pixel stays fully transparent.
    let shadow_span = (padding - 0.5).max(0.5);
    let center_x = (left + right) * 0.5;
    let center_y = (top + bottom) * 0.5;
    let half_width = (right - left) * 0.5;
    let half_height = (bottom - top) * 0.5;
    for y in 0..height as usize {
        let cy = y as f64 + 0.5;
        let row = &mut pixels[y * width as usize..(y + 1) * width as usize];
        let (start, end) = opaque_body_span(cy, left, top, right, bottom, radius, border);
        row[start..end].fill(body);
        for x in (0..start).chain(end..width as usize) {
            let d = rounded_distance(x as f64 + 0.5, cy, center_x, center_y, half_width, half_height, radius);
            let outer = coverage(d);
            let inner = coverage(d + border);
            let stroke = (outer - inner).max(0.0);
            let shadow = shadow_alpha * (1.0 - (d.max(0.0) / shadow_span).min(1.0)).powi(3);
            // Composite the antialiased card over its black shadow. Channel
            // values are already multiplied by coverage, including the fringe.
            let alpha = ((outer + shadow * (1.0 - outer)) * 255.0).round() as u8;
            row[x] = [
                ((body[0] as f64 * inner + outline[0] * stroke).round() as u8).min(alpha),
                ((body[1] as f64 * inner + outline[1] * stroke).round() as u8).min(alpha),
                ((body[2] as f64 * inner + outline[2] * stroke).round() as u8).min(alpha),
                alpha,
            ];
        }
    }
}

fn coverage(distance: f64) -> f64 {
    (0.5 - distance).clamp(0.0, 1.0)
}

fn rounded_distance(x: f64, y: f64, cx: f64, cy: f64, hw: f64, hh: f64, radius: f64) -> f64 {
    let dx = (x - cx).abs() - (hw - radius);
    let dy = (y - cy).abs() - (hh - radius);
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius
}

/// Pixel-center span with full body coverage. Its inset includes the half-pixel
/// antialias fringe; only the two short edge spans need per-pixel distances.
fn opaque_body_span(y: f64, left: f64, top: f64, right: f64, bottom: f64, radius: f64, border: f64) -> (usize, usize) {
    let inset = border + 0.5;
    let (left, top, right, bottom) = (left + inset, top + inset, right - inset, bottom - inset);
    if right < left || bottom < top || y < top || y > bottom {
        return (0, 0);
    }
    let radius = (radius - inset).max(0.0).min((right - left) * 0.5).min((bottom - top) * 0.5);
    let dy = (top + radius - y).max(y - (bottom - radius)).max(0.0);
    let arc = (radius * radius - dy * dy).max(0.0).sqrt();
    let start = (left + radius - arc - 0.5).ceil().max(0.0) as usize;
    let end = ((right - radius + arc - 0.5).floor() + 1.0).max(start as f64) as usize;
    (start, end)
}

/// Overlay scrollbar for scrolled tooltip text, mirroring the main list's
/// tokens `scrollbar` (thumb 4 DIP, inset 3 DIP, min 24 DIP, alpha 0.3,
/// fade in 90 ms, idle 900 ms, fade out 240 ms; DESIGN 2026-10-15).
pub(super) const SCROLLBAR_THUMB_DIP: f64 = 4.0;
pub(super) const SCROLLBAR_INSET_DIP: f64 = 3.0;
pub(super) const SCROLLBAR_MIN_THUMB_DIP: f64 = 24.0;
pub(super) const SCROLLBAR_ALPHA: f64 = 0.3;
const SCROLLBAR_FADE_IN_MS: f64 = 90.0;
const SCROLLBAR_IDLE_MS: f64 = 900.0;
const SCROLLBAR_FADE_OUT_MS: f64 = 240.0;

/// Visibility (0..=1) `since_ms` after the last scroll, and how long until the
/// next repaint is needed (`None` once fully faded).
pub(super) fn scrollbar_visibility(since_ms: f64) -> (f64, Option<u32>) {
    let ease = |t: f64| {
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    if !since_ms.is_finite() || since_ms < 0.0 {
        return (0.0, None);
    }
    if since_ms < SCROLLBAR_FADE_IN_MS {
        (ease(since_ms / SCROLLBAR_FADE_IN_MS), Some(16))
    } else if since_ms < SCROLLBAR_IDLE_MS {
        (1.0, Some((SCROLLBAR_IDLE_MS - since_ms).ceil().max(1.0) as u32))
    } else if since_ms < SCROLLBAR_IDLE_MS + SCROLLBAR_FADE_OUT_MS {
        (1.0 - ease((since_ms - SCROLLBAR_IDLE_MS) / SCROLLBAR_FADE_OUT_MS), Some(16))
    } else {
        (0.0, None)
    }
}

/// Thumb `(top, height)` in a track from `track_top` to `track_bottom` for a
/// viewport showing `viewport` of `viewport + max_offset` pixels.
pub(super) fn scroll_thumb(
    track_top: f64,
    track_bottom: f64,
    viewport: f64,
    max_offset: f64,
    offset: f64,
    min_thumb: f64,
) -> Option<(f64, f64)> {
    let track = track_bottom - track_top;
    if ![track_top, track_bottom, viewport, max_offset, offset, min_thumb]
        .iter()
        .all(|v| v.is_finite())
        || track <= 0.0
        || viewport <= 0.0
        || max_offset <= 0.0
    {
        return None;
    }
    let height = (track * viewport / (viewport + max_offset)).clamp(min_thumb.min(track), track);
    let top = track_top + offset.clamp(0.0, max_offset) / max_offset * (track - height);
    Some((top, height))
}

/// Source-over an antialiased vertical capsule onto premultiplied BGRA rows
/// of `stride` pixels. `rgb` is straight colour, `alpha` its opacity.
pub(super) fn blend_capsule(
    pixels: &mut [[u8; 4]],
    stride: usize,
    rows: usize,
    rect: (f64, f64, f64, f64),
    rgb: [u8; 3],
    alpha: f64,
) {
    let (left, top, right, bottom) = rect;
    if alpha <= 0.0 || right <= left || bottom <= top || stride == 0 {
        return;
    }
    let (cx, cy) = ((left + right) * 0.5, (top + bottom) * 0.5);
    let (hw, hh) = ((right - left) * 0.5, (bottom - top) * 0.5);
    let radius = hw.min(hh);
    let x0 = (left.floor().max(0.0) as usize).min(stride);
    let x1 = (right.ceil().max(0.0) as usize).min(stride);
    let y0 = (top.floor().max(0.0) as usize).min(rows);
    let y1 = (bottom.ceil().max(0.0) as usize).min(rows);
    for y in y0..y1 {
        let Some(row) = pixels.get_mut(y * stride..(y + 1) * stride) else {
            return;
        };
        for x in x0..x1 {
            let d = rounded_distance(x as f64 + 0.5, y as f64 + 0.5, cx, cy, hw, hh, radius);
            let a = coverage(d) * alpha.min(1.0);
            if a <= 0.0 {
                continue;
            }
            let pixel = &mut row[x];
            // BGRA order; destination is premultiplied.
            for (channel, value) in [rgb[2], rgb[1], rgb[0]].into_iter().enumerate() {
                pixel[channel] =
                    (value as f64 * a + pixel[channel] as f64 * (1.0 - a)).round() as u8;
            }
            pixel[3] = (255.0 * a + pixel[3] as f64 * (1.0 - a)).round() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_frame_dpi_corners_are_transparent_and_pixels_are_premultiplied() {
        for scale in [1.0, 1.5, 2.0] {
            for dark in [false, true] {
                let width = (180.0 * scale) as i32;
                let height = (90.0 * scale) as i32;
                let mut pixels = vec![[255; 4]; (width * height) as usize];
                draw_frame(width, height, scale, dark, &mut pixels);
                let at = |x: i32, y: i32| pixels[(y * width + x) as usize];
                assert!(shadow_padding(scale) as f64 >= SHADOW_DIP * scale);
                for x in 0..width {
                    assert_eq!(at(x, 0), [0; 4]);
                    assert_eq!(at(x, height - 1), [0; 4]);
                }
                for y in 0..height {
                    assert_eq!(at(0, y), [0; 4]);
                    assert_eq!(at(width - 1, y), [0; 4]);
                }
                let padding = shadow_padding(scale);
                assert!(at(padding, padding)[3] < 255, "the card corner must be rounded");
                assert!(pixels.iter().any(|pixel| pixel[3] > 0 && pixel[3] < 255));
                assert!(pixels.iter().all(|pixel| pixel[..3].iter().all(|channel| *channel <= pixel[3])));
            }
        }
    }

    #[test]
    fn hover_frame_body_and_inner_border_keep_theme_colors() {
        for scale in [1.0, 2.0] {
            for dark in [false, true] {
                let width = (140.0 * scale) as i32;
                let height = (70.0 * scale) as i32;
                let mut pixels = vec![[0; 4]; (width * height) as usize];
                draw_frame(width, height, scale, dark, &mut pixels);
                let at = |x: i32, y: i32| pixels[(y * width + x) as usize];
                let body = if dark { [27, 27, 27, 255] } else { [250, 250, 250, 255] };
                let border = if dark { [82, 75, 72, 255] } else { [222, 216, 212, 255] };
                assert_eq!(at(width / 2, height / 2), body);
                assert_eq!(at(width / 2, shadow_padding(scale)), border);
                assert_eq!(at(width / 2, shadow_padding(scale) + (3.0 * scale) as i32), body);
            }
        }
    }

    #[test]
    fn tooltip_scrollbar_fades_in_holds_and_fades_out() {
        assert_eq!(super::scrollbar_visibility(0.0).0, 0.0);
        assert_eq!(super::scrollbar_visibility(90.0), (1.0, Some(810)));
        let (fading, next) = super::scrollbar_visibility(1000.0);
        assert!(fading > 0.0 && fading < 1.0 && next == Some(16));
        assert_eq!(super::scrollbar_visibility(1140.0), (0.0, None));
        assert_eq!(super::scrollbar_visibility(-1.0), (0.0, None));
    }

    #[test]
    fn tooltip_scroll_thumb_tracks_offset_and_respects_minimum() {
        // Half the content visible: half-height thumb, at the end when scrolled out.
        assert_eq!(super::scroll_thumb(10.0, 110.0, 100.0, 100.0, 0.0, 24.0), Some((10.0, 50.0)));
        assert_eq!(super::scroll_thumb(10.0, 110.0, 100.0, 100.0, 100.0, 24.0), Some((60.0, 50.0)));
        assert_eq!(super::scroll_thumb(0.0, 100.0, 100.0, 9900.0, 0.0, 24.0), Some((0.0, 24.0)));
        assert_eq!(super::scroll_thumb(0.0, 100.0, 100.0, 0.0, 0.0, 24.0), None);
    }

    #[test]
    fn tooltip_scroll_thumb_blends_premultiplied_inside_capsule_only() {
        let mut pixels = vec![[250, 250, 250, 255]; 10 * 20];
        super::blend_capsule(&mut pixels, 10, 20, (4.0, 2.0, 8.0, 18.0), [0, 0, 0], 0.5);
        assert_eq!(pixels[10 * 10 + 6], [125, 125, 125, 255]);
        assert_eq!(pixels[10 * 10 + 1], [250, 250, 250, 255]);
        assert_eq!(pixels[0], [250, 250, 250, 255]);
    }
}
