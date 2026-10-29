//! Quick Paste's floating row cards and detached action/preview surfaces.
//! Dimensions and endpoint palettes follow app.css:11859-12604. Existing
//! ThemeTokens supply the resolved base palette, including Custom alpha.

use gpui::{
    canvas, linear_color_stop, linear_gradient, point, prelude::*, px, quad, transparent_black,
    AnyElement, BorderStyle, Bounds, BoxShadow, Edges, Hsla, Path, Pixels, Rgba, Window,
};

use crate::{motion::mix_hsla, panel::css_mix, preview::ThemeTokens, view::color};

#[derive(Clone, Copy)]
pub(super) struct RowColors {
    pub border: Hsla,
    pub index_background: Hsla,
    pub index_text: Hsla,
    pub lead_border: Hsla,
    /// CSS's outside 1px selection ring.
    pub outline: Hsla,
}

/// `active_progress` is the caller's retained FOCUS transition, in 0..=1.
pub(super) fn row_colors(theme: &ThemeTokens, active_progress: f32, pinned: bool) -> RowColors {
    let progress = active_progress.clamp(0.0, 1.0);
    let tertiary = color(&theme.tertiary);
    let accent = color(&theme.accent);
    let border = color(&theme.border);
    let lead_border = color(&theme.border_secondary);
    let text = color(&theme.text);
    let gold = color("#d4a017");
    let (index_normal, index_active, index_text_normal, index_text_active) = if pinned {
        (
            css_mix(tertiary, gold, 0.22),
            // CSS three-way mix: gold 34%, accent 18%, tertiary 48%.
            css_mix(css_mix(tertiary, accent, 18.0 / 66.0), gold, 0.34),
            css_mix(text, color("#f0c14b"), 0.88),
            css_mix(text, color("#ffe08a"), 0.90),
        )
    } else {
        (
            tertiary,
            css_mix(tertiary, accent, 0.24),
            color(&theme.text_secondary),
            text,
        )
    };
    RowColors {
        border: mix_hsla(border, css_mix(border, accent, 0.68), progress),
        index_background: mix_hsla(index_normal, index_active, progress),
        index_text: mix_hsla(index_text_normal, index_text_active, progress),
        lead_border: mix_hsla(lead_border, css_mix(lead_border, accent, 0.45), progress),
        outline: faded(accent, 0.58 * progress),
    }
}

/// Paint-only children preserve row layout, virtual-list identity and input.
/// GPUI's two-stop shader changes diagonal angles according to aspect ratio.
/// Paths below split at the CSS stop, and remap both angle and stop positions
/// for each path's actual bounds. Translucent backgrounds paint each region
/// once; opaque backgrounds also fill the AA join with the middle-stop color.
pub(super) fn row_surface(theme: &ThemeTokens, active_progress: f32) -> AnyElement {
    let progress = active_progress.clamp(0.0, 1.0);
    let surface = color(&theme.surface);
    let accent = color(&theme.accent);
    let text = color(&theme.text);
    let border = row_colors(theme, progress, false).border;
    let stops = [0.30, 0.18, 0.10]
        .map(|weight| mix_hsla(surface, css_mix(surface, accent, weight), progress));
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            // Absolute children occupy the parent's padding box in GPUI;
            // include the retained 1px layout border in the painted surface.
            let bounds = bounds.dilate(px(1.0));
            if progress == 0.0 {
                window.paint_quad(gpui::fill(bounds, surface).corner_radii(px(10.0)));
            } else {
                paint_css_gradient(window, bounds, 10.0, stops);
            }
            window.paint_quad(
                gpui::outline(bounds, border, BorderStyle::Solid).corner_radii(px(10.0)),
            );
            let inset = bounds.dilate(px(-1.0));
            // Normal inset 0 1px 0 text-primary 4%, interpolated away on select.
            window.paint_quad(quad(
                inset,
                px(9.0),
                transparent_black(),
                Edges {
                    top: px(1.0),
                    ..Edges::default()
                },
                faded(text, 0.04 * (1.0 - progress)),
                BorderStyle::Solid,
            ));
            window.paint_quad(
                gpui::outline(inset, faded(accent, 0.24 * progress), BorderStyle::Solid)
                    .corner_radii(px(9.0)),
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

pub(super) fn pinned_index_ring(_theme: &ThemeTokens, active_progress: f32) -> AnyElement {
    let progress = active_progress.clamp(0.0, 1.0);
    let inside = mix_hsla(
        faded(color("#d4a017"), 0.45),
        faded(color("#f0c14b"), 0.55),
        progress,
    );
    let outside = faded(color("#d4a017"), 0.40 * progress);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            // This child paints after the badge fill, so a shadow would also
            // tint its interior. An outside-only outline is the zero-blur ring.
            window.paint_quad(
                gpui::outline(bounds.dilate(px(1.0)), outside, BorderStyle::Solid)
                    .corner_radii(px(7.0)),
            );
            window.paint_quad(
                gpui::outline(bounds, inside, BorderStyle::Solid).corner_radii(px(6.0)),
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

/// CSS outline-offset:2px + 2px dashed stroke, entirely outside the list.
pub(super) fn cancel_outline(theme: &ThemeTokens) -> AnyElement {
    let border = faded(color(&theme.text_muted), 0.55);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(
                gpui::outline(bounds.dilate(px(4.0)), border, BorderStyle::Dashed)
                    .corner_radii(px(14.0))
                    .border_widths(px(2.0)),
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

pub(super) fn swatch_inset(theme: &ThemeTokens, radius: f32) -> AnyElement {
    let border = faded(color(&theme.text), 0.08);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(
                gpui::outline(bounds.dilate(px(-1.0)), border, BorderStyle::Solid)
                    .corner_radii(px((radius - 1.0).max(0.0))),
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

#[derive(Clone, Copy, Debug)]
struct GradientLine {
    center: [f32; 2],
    direction: [f32; 2],
    span: f32,
}

impl GradientLine {
    fn new(rect: [f32; 4]) -> Self {
        let radians = (165.0f32 - 90.0).to_radians();
        let direction = [radians.cos(), radians.sin()];
        Self {
            center: [rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0],
            direction,
            span: rect[2] * direction[0].abs() + rect[3] * direction[1].abs(),
        }
    }

    fn project(self, p: [f32; 2]) -> f32 {
        0.5 + ((p[0] - self.center[0]) * self.direction[0]
            + (p[1] - self.center[1]) * self.direction[1])
            / self.span
    }

    /// Compensate gpui 0.2.2 windows/shaders.hlsl:346-369. Stops may be
    /// outside 0..1 because the shader normalizes to one bounding-box axis;
    /// CSS normalizes to its complete corner-to-corner gradient line.
    fn shader_mapping(self, rect: [f32; 4], stops: [f32; 2]) -> (f32, [f32; 2]) {
        let [dx, dy] = self.direction;
        let [_, _, width, height] = rect;
        let angle = if width > height {
            (dy * width / height).atan2(dx)
        } else {
            dy.atan2(dx * height / width)
        }
        .to_degrees()
            + 90.0;
        let axis = if dx.abs() > dy.abs() { width } else { height };
        let center_t = self.project([rect[0] + width / 2.0, rect[1] + height / 2.0]);
        (
            angle,
            stops.map(|t| 0.5 + (t - center_t) * self.span / axis),
        )
    }
}

#[derive(Clone, Copy)]
struct Curve {
    from: [f32; 2],
    to: [f32; 2],
    control: Option<[f32; 2]>,
}

impl Curve {
    fn point(self, t: f32) -> [f32; 2] {
        let control = self.control.unwrap_or(self.from);
        std::array::from_fn(|i| {
            if self.control.is_some() {
                (1.0 - t).powi(2) * self.from[i]
                    + 2.0 * t * (1.0 - t) * control[i]
                    + t * t * self.to[i]
            } else {
                self.from[i] + (self.to[i] - self.from[i]) * t
            }
        })
    }

    fn portion(self, low: f32, high: f32) -> Self {
        let from = self.point(low);
        let control = self.control.map(|control| {
            std::array::from_fn(|i| {
                from[i]
                    + (high - low)
                        * ((1.0 - low) * (control[i] - self.from[i])
                            + low * (self.to[i] - control[i]))
            })
        });
        Self {
            from,
            to: self.point(high),
            control,
        }
    }
}

fn rounded_curves(rect: [f32; 4], radius: f32) -> Vec<Curve> {
    let [left, top, width, height] = rect;
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let centers = [
        [left + width - radius, top + radius],
        [left + width - radius, top + height - radius],
        [left + radius, top + height - radius],
        [left + radius, top + radius],
    ];
    let mut curves = Vec::with_capacity(36);
    let mut from = [left + radius, top];
    // Eight quadratic pieces per quarter circle: radial error <0.00012 DIP
    // at the CSS 10 DIP radius, with native path anti-aliasing at the edge.
    for (corner, center) in centers.into_iter().enumerate() {
        let start = -90.0 + corner as f32 * 90.0;
        let first_angle = start.to_radians();
        let first = [
            center[0] + radius * first_angle.cos(),
            center[1] + radius * first_angle.sin(),
        ];
        curves.push(Curve {
            from,
            to: first,
            control: None,
        });
        from = first;
        for piece in 0..8 {
            let a = (start + piece as f32 * 11.25).to_radians();
            let b = (start + (piece + 1) as f32 * 11.25).to_radians();
            let middle = (a + b) / 2.0;
            let tangent_radius = radius / ((b - a) / 2.0).cos();
            let control = [
                center[0] + tangent_radius * middle.cos(),
                center[1] + tangent_radius * middle.sin(),
            ];
            let to = [center[0] + radius * b.cos(), center[1] + radius * b.sin()];
            curves.push(Curve {
                from,
                to,
                control: Some(control),
            });
            from = to;
        }
    }
    curves
}

fn roots(a: f32, b: f32, c: f32, values: &mut Vec<f32>) {
    if a.abs() < 0.000001 {
        if b.abs() > 0.000001 {
            let t = -c / b;
            if t > 0.0 && t < 1.0 {
                values.push(t);
            }
        }
        return;
    }
    let discriminant = b * b - 4.0 * a * c;
    if discriminant < 0.0 {
        return;
    }
    for t in [
        (-b - discriminant.sqrt()) / (2.0 * a),
        (-b + discriminant.sqrt()) / (2.0 * a),
    ] {
        if t > 0.0 && t < 1.0 {
            values.push(t);
        }
    }
}

/// Intersect the convex rounded outline with both gradient half planes.
/// Splitting a quadratic through a clipping plane uses its actual roots and
/// de Casteljau control point; it never makes square corners at the CSS stop.
fn clipped_curves(curves: &[Curve], line: GradientLine, low: f32, high: f32) -> Vec<Curve> {
    let mut result = Vec::new();
    for curve in curves {
        let from = line.project(curve.from);
        let to = line.project(curve.to);
        let control = curve.control.map(|control| line.project(control));
        let (a, b) = control.map_or((0.0, to - from), |control| {
            (from - 2.0 * control + to, 2.0 * (control - from))
        });
        let mut values = vec![0.0, 1.0];
        roots(a, b, from - low, &mut values);
        roots(a, b, from - high, &mut values);
        values.sort_by(f32::total_cmp);
        values.dedup_by(|a, b| (*a - *b).abs() < 0.000001);
        for pair in values.windows(2) {
            let mid = line.project(curve.point((pair[0] + pair[1]) / 2.0));
            if mid >= low - 0.000001 && mid <= high + 0.000001 {
                result.push(curve.portion(pair[0], pair[1]));
            }
        }
    }
    result
}

fn curve_path(curves: &[Curve]) -> Option<(Path<Pixels>, [f32; 4])> {
    let first = curves.first()?.from;
    let mut path = Path::new(point(px(first[0]), px(first[1])));
    let mut current = first;
    let mut bounds = [first[0], first[1], first[0], first[1]];
    for curve in curves {
        if current != curve.from {
            path.line_to(point(px(curve.from[0]), px(curve.from[1])));
        }
        if let Some(control) = curve.control {
            path.curve_to(
                point(px(curve.to[0]), px(curve.to[1])),
                point(px(control[0]), px(control[1])),
            );
        } else {
            path.line_to(point(px(curve.to[0]), px(curve.to[1])));
        }
        for p in [curve.from, curve.to].into_iter().chain(curve.control) {
            bounds[0] = bounds[0].min(p[0]);
            bounds[1] = bounds[1].min(p[1]);
            bounds[2] = bounds[2].max(p[0]);
            bounds[3] = bounds[3].max(p[1]);
        }
        current = curve.to;
    }
    path.line_to(point(px(first[0]), px(first[1])));
    let rect = [
        bounds[0],
        bounds[1],
        bounds[2] - bounds[0],
        bounds[3] - bounds[1],
    ];
    (rect[2] > 0.0 && rect[3] > 0.0).then_some((path, rect))
}

#[derive(Clone, Copy)]
struct GradientBand {
    low: f32,
    high: f32,
    from: Hsla,
    to: Hsla,
}

/// Native two-stop gradients interpolate straight RGBA. CSS interpolates
/// premultiplied alpha, so translucent bands subdivide until the maximum
/// composited RGB error is below 1/1024. Opaque themes need only two paths.
fn gradient_bands(
    low: f32,
    high: f32,
    from: Hsla,
    to: Hsla,
    depth: u8,
    bands: &mut Vec<GradientBand>,
) {
    let a: Rgba = from.into();
    let b: Rgba = to.into();
    let error = (a.a - b.a).abs()
        * (a.r - b.r)
            .abs()
            .max((a.g - b.g).abs())
            .max((a.b - b.b).abs())
        / 4.0;
    if error > 1.0 / 1024.0 && depth < 8 {
        let middle = (low + high) / 2.0;
        let color = css_mix(from, to, 0.5);
        gradient_bands(low, middle, from, color, depth + 1, bands);
        gradient_bands(middle, high, color, to, depth + 1, bands);
    } else {
        bands.push(GradientBand {
            low,
            high,
            from,
            to,
        });
    }
}

fn paint_css_gradient(window: &mut Window, bounds: Bounds<Pixels>, radius: f32, stops: [Hsla; 3]) {
    let rect = [
        bounds.left().into(),
        bounds.top().into(),
        bounds.size.width.into(),
        bounds.size.height.into(),
    ];
    if rect[2] <= 0.0 || rect[3] <= 0.0 {
        return;
    }
    if let Some(underlay) = opaque_gradient_underlay(stops) {
        // Independent MSAA Path coverage leaves a visible seam at 55% on a
        // transparent HWND. The join's exact stop color supplies those missed
        // samples; opaque paths cover it elsewhere. Never layer this under
        // translucent input, which would change its source-over alpha.
        window.paint_quad(gpui::fill(bounds, underlay).corner_radii(px(radius)));
    }
    let line = GradientLine::new(rect);
    let curves = rounded_curves(rect, radius);
    let mut bands = Vec::new();
    gradient_bands(0.0, 0.55, stops[0], stops[1], 0, &mut bands);
    gradient_bands(0.55, 1.0, stops[1], stops[2], 0, &mut bands);
    for band in bands {
        let curves = clipped_curves(&curves, line, band.low, band.high);
        if let Some((path, rect)) = curve_path(&curves) {
            // DirectxRenderer::draw_paths passes clipped_bounds(), including
            // a partially scrolled row's content mask, to gradient_color.
            let clipped = Bounds::new(
                point(px(rect[0]), px(rect[1])),
                gpui::size(px(rect[2]), px(rect[3])),
            )
            .intersect(&window.content_mask().bounds);
            let clipped_rect = [
                clipped.left().into(),
                clipped.top().into(),
                clipped.size.width.into(),
                clipped.size.height.into(),
            ];
            if clipped_rect[2] <= 0.0 || clipped_rect[3] <= 0.0 {
                continue;
            }
            let (angle, percentages) = line.shader_mapping(clipped_rect, [band.low, band.high]);
            window.paint_path(
                path,
                linear_gradient(
                    angle,
                    linear_color_stop(band.from, percentages[0]),
                    linear_color_stop(band.to, percentages[1]),
                ),
            );
        }
    }
}

fn opaque_gradient_underlay(stops: [Hsla; 3]) -> Option<Hsla> {
    stops.iter().all(|color| color.a >= 1.0).then_some(stops[1])
}

#[derive(Clone, Copy)]
pub(super) struct ActionColors {
    pub background: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub shortcut_background: Hsla,
    pub shortcut_border: Hsla,
}

/// The later clickable:hover rule overrides armed button fill/border; the
/// armed shortcut and glow remain. `.btn--clickable {color:inherit}` makes
/// actual normal/armed text inherit --text-primary rather than --text-secondary.
pub(super) fn action_colors(
    theme: &ThemeTokens,
    armed_progress: f32,
    hover_progress: f32,
) -> ActionColors {
    let armed = armed_progress.clamp(0.0, 1.0);
    let hover = hover_progress.clamp(0.0, 1.0);
    let surface = color(&theme.surface);
    let secondary = color(&theme.secondary);
    let border = color(&theme.border_secondary);
    let accent = color(&theme.accent);
    ActionColors {
        background: mix_hsla(
            mix_hsla(
                faded(secondary, 0.82),
                css_mix(surface, accent, 0.22),
                armed,
            ),
            css_mix(secondary, accent, 0.30),
            hover,
        ),
        border: mix_hsla(
            mix_hsla(faded(border, 0.70), css_mix(border, accent, 0.62), armed),
            css_mix(border, accent, 0.40),
            hover,
        ),
        text: color(&theme.text),
        shortcut_background: mix_hsla(
            css_mix(surface, accent, 0.30),
            css_mix(surface, accent, 0.55),
            armed,
        ),
        shortcut_border: mix_hsla(
            css_mix(border, accent, 0.35),
            css_mix(border, accent, 0.70),
            armed,
        ),
    }
}

/// Multiply the existing alpha, preserving Custom transparent color inputs.
fn faded(mut value: Hsla, strength: f32) -> Hsla {
    value.a *= strength.clamp(0.0, 1.0);
    value
}

pub(super) fn preview_border(theme: &ThemeTokens) -> Hsla {
    css_mix(color(&theme.border_secondary), color(&theme.accent), 0.32)
}

pub(super) fn preview_shadows(theme: &ThemeTokens) -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: gpui::black().opacity(0.18),
            offset: point(px(0.0), px(10.0)),
            blur_radius: px(28.0),
            spread_radius: px(0.0),
        },
        outer_ring(faded(color(&theme.accent), 0.12), 1.0),
    ]
}

/// CSS outer ring; row_surface paints the distinct inside highlights.
pub(super) fn row_shadows(theme: &ThemeTokens, active_progress: f32) -> Vec<BoxShadow> {
    vec![outer_ring(
        row_colors(theme, active_progress, false).outline,
        1.0,
    )]
}

pub(super) fn action_shadows(theme: &ThemeTokens, armed_progress: f32) -> Vec<BoxShadow> {
    let armed = armed_progress.clamp(0.0, 1.0);
    let accent = color(&theme.accent);
    vec![
        outer_ring(faded(accent, 0.38 * armed), 2.0),
        BoxShadow {
            color: faded(accent, 0.18 * armed),
            offset: point(px(0.0), px(4.0)),
            blur_radius: px(14.0),
            spread_radius: px(0.0),
        },
    ]
}

fn outer_ring(color: Hsla, width: f32) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.0), px(0.0)),
        blur_radius: px(0.0),
        spread_radius: px(width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{DesignTokens, TOKENS_JSON};
    use gpui::Rgba;

    fn theme() -> ThemeTokens {
        DesignTokens::from_json(TOKENS_JSON).unwrap().themes.light
    }

    fn assert_rgba(actual: Hsla, expected: Rgba) {
        let actual: Rgba = actual.into();
        for (a, b) in [
            (actual.r, expected.r),
            (actual.g, expected.g),
            (actual.b, expected.b),
            (actual.a, expected.a),
        ] {
            assert!((a - b).abs() < 0.00001, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn css_gradient_uses_165_degree_corner_span_and_the_55_percent_stop() {
        let line = GradientLine::new([20.0, 30.0, 380.0, 40.0]);
        assert!(line.project([20.0, 30.0]).abs() < 0.00001);
        assert!((line.project([400.0, 70.0]) - 1.0).abs() < 0.00001);
        assert!(
            (line.direction[1] / line.direction[0] - 75.0f32.to_radians().tan()).abs() < 0.00001
        );
        // The selected midpoint is displaced along the real CSS direction,
        // rather than the almost horizontal native two-stop gradient.
        let stop = [
            line.center[0] + line.direction[0] * line.span * 0.05,
            line.center[1] + line.direction[1] * line.span * 0.05,
        ];
        assert!((line.project(stop) - 0.55).abs() < 0.00001);
        let surface = color("#ff0000");
        let accent = color("#0000ff");
        let colors = [0.30, 0.18, 0.10].map(|weight| css_mix(surface, accent, weight));
        let mut bands = Vec::new();
        gradient_bands(0.0, 0.55, colors[0], colors[1], 0, &mut bands);
        gradient_bands(0.55, 1.0, colors[1], colors[2], 0, &mut bands);
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].high, 0.55);
        assert_eq!(bands[1].low, 0.55);
        assert_rgba(
            bands[0].to,
            Rgba {
                r: 0.82,
                g: 0.0,
                b: 0.18,
                a: 1.0,
            },
        );
        assert_eq!(bands[0].to, bands[1].from);
    }

    fn native_shader_t(rect: [f32; 4], angle: f32, p: [f32; 2]) -> f32 {
        let radians = (angle - 90.0).to_radians();
        let [_, _, width, height] = rect;
        let mut d = [radians.cos(), radians.sin()];
        if width > height {
            d[1] *= height / width;
        } else {
            d[0] *= width / height;
        }
        let length = (d[0] * d[0] + d[1] * d[1]).sqrt();
        let axis = if d[0].abs() > d[1].abs() {
            width
        } else {
            height
        };
        0.5 + ((p[0] - rect[0] - width / 2.0) * d[0] + (p[1] - rect[1] - height / 2.0) * d[1])
            / length
            / axis
    }

    #[test]
    fn shader_compensation_preserves_css_geometry_in_both_clipped_paths() {
        for rect in [
            [8.0, 12.0, 380.0, 40.0],
            [37.0, 18.0, 380.0, 58.0],
            [11.0, 5.0, 100.0, 58.0],
        ] {
            let line = GradientLine::new(rect);
            let curves = rounded_curves(rect, 10.0);
            for stops in [[0.0, 0.55], [0.55, 1.0]] {
                let pieces = clipped_curves(&curves, line, stops[0], stops[1]);
                let (_, path_bounds) = curve_path(&pieces).unwrap();
                let (angle, native_stops) = line.shader_mapping(path_bounds, stops);
                for curve in &pieces {
                    for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
                        let p = curve.point(t);
                        let actual = (native_shader_t(path_bounds, angle, p) - native_stops[0])
                            / (native_stops[1] - native_stops[0]);
                        let expected = (line.project(p) - stops[0]) / (stops[1] - stops[0]);
                        assert!(
                            (actual - expected).abs() < 0.00002,
                            "{actual} != {expected}"
                        );
                        let css_t = line.project(p);
                        assert!(css_t >= stops[0] - 0.00002 && css_t <= stops[1] + 0.00002);
                    }
                }
                assert!(pieces.iter().any(|curve| curve.control.is_some()));
            }
        }
    }

    #[test]
    fn quadratic_rounding_stays_within_subpixel_circle_error() {
        for curve in rounded_curves([0.0, 0.0, 380.0, 40.0], 10.0) {
            if curve.control.is_none() {
                continue;
            }
            let mid = curve.point(0.5);
            let center = [
                if mid[0] < 190.0 { 10.0 } else { 370.0 },
                if mid[1] < 20.0 { 10.0 } else { 30.0 },
            ];
            let radius = ((mid[0] - center[0]).powi(2) + (mid[1] - center[1]).powi(2)).sqrt();
            assert!((radius - 10.0).abs() < 0.00014, "radius = {radius}");
        }
    }

    #[test]
    fn custom_alpha_bands_bound_composited_error_without_overlapping_fills() {
        for alpha in [0.0, 0.01, 0.10, 0.5, 1.0] {
            for progress in [0.01, 0.5, 1.0] {
                let surface = Hsla {
                    a: alpha,
                    ..color("#f00020")
                };
                let accent = color("#00a0ff");
                let stops = [0.30, 0.18, 0.10]
                    .map(|weight| mix_hsla(surface, css_mix(surface, accent, weight), progress));
                let mut bands = Vec::new();
                gradient_bands(0.0, 0.55, stops[0], stops[1], 0, &mut bands);
                gradient_bands(0.55, 1.0, stops[1], stops[2], 0, &mut bands);
                assert_eq!(bands.first().unwrap().low, 0.0);
                assert_eq!(bands.last().unwrap().high, 1.0);
                for pair in bands.windows(2) {
                    assert_eq!(pair[0].high, pair[1].low);
                    assert_eq!(pair[0].to, pair[1].from);
                }
                for band in bands {
                    let a: Rgba = band.from.into();
                    let b: Rgba = band.to.into();
                    let expected: Rgba = css_mix(band.from, band.to, 0.5).into();
                    let alpha = (a.a + b.a) / 2.0;
                    for (first, second, expected) in [
                        (a.r, b.r, expected.r),
                        (a.g, b.g, expected.g),
                        (a.b, b.b, expected.b),
                    ] {
                        assert!(
                            ((first + second) / 2.0 * alpha - expected * alpha).abs()
                                <= 1.0 / 1024.0 + 0.000001
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn seam_underlay_is_exact_middle_color_and_rejects_any_translucent_stop() {
        let stops = [color("#3060a0"), color("#8090c0"), color("#b0c0d0")];
        assert_eq!(opaque_gradient_underlay(stops), Some(stops[1]));
        for index in 0..3 {
            let mut translucent = stops;
            translucent[index].a = 0.999;
            assert!(opaque_gradient_underlay(translucent).is_none());
            translucent[index].a = 0.5;
            assert!(opaque_gradient_underlay(translucent).is_none());
        }
    }

    #[test]
    fn pinned_index_preserves_three_css_weights_and_custom_alpha() {
        let mut theme = theme();
        theme.tertiary = "#0000FF80".into();
        theme.accent = "#FF000040".into();
        let selected = row_colors(&theme, 1.0, true);
        let alpha = 0.48 * 128.0 / 255.0 + 0.18 * 64.0 / 255.0 + 0.34;
        assert_rgba(
            selected.index_background,
            Rgba {
                r: (0.18 * 64.0 / 255.0 + 0.34 * 212.0 / 255.0) / alpha,
                g: 0.34 * 160.0 / 255.0 / alpha,
                b: (0.48 * 128.0 / 255.0 + 0.34 * 23.0 / 255.0) / alpha,
                a: alpha,
            },
        );
    }

    #[test]
    fn hover_overrides_armed_fill_but_keeps_shortcut_and_glow() {
        let theme = theme();
        let armed = action_colors(&theme, 1.0, 0.0);
        let armed_hover = action_colors(&theme, 1.0, 1.0);
        let hover = action_colors(&theme, 0.0, 1.0);
        assert_eq!(armed_hover.background, hover.background);
        assert_eq!(armed_hover.border, hover.border);
        assert_eq!(armed_hover.shortcut_background, armed.shortcut_background);
        assert_eq!(hover.text, color(&theme.text));
    }

    #[test]
    fn preview_and_action_shadows_never_increase_custom_alpha() {
        let mut theme = theme();
        theme.accent = "#12345680".into();
        let alpha = 128.0 / 255.0;
        assert!((preview_shadows(&theme)[1].color.a - alpha * 0.12).abs() < 0.00001);
        assert!((action_shadows(&theme, 1.0)[0].color.a - alpha * 0.38).abs() < 0.00001);
        assert!((action_shadows(&theme, 1.0)[1].color.a - alpha * 0.18).abs() < 0.00001);
        assert_eq!(row_shadows(&theme, 0.0)[0].color.a, 0.0);
    }
}
