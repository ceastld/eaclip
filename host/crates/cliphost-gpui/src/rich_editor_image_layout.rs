//! Editor image geometry. Intrinsic dimensions come from the loaded GPUI image;
//! this module never reads a path, decodes an image, or changes source content.

use crate::editor_html::{HtmlImageDimension, HtmlImageLayout};
use gpui::Size;

const IM_MAX_IMAGE_HEIGHT: f32 = 90.;

/// Resolves HTML replaced-element dimensions in logical pixels. The editor's
/// available width always caps the result. A declared axis keeps its own size
/// when a maximum constrains the other axis; only two auto axes scale together.
/// Unsupported CSS and missing intrinsic/percentage bases return `None`, so a
/// caller can keep its loading or unresolved-content presentation.
pub(crate) fn resolve_html_image_size(
    layout: HtmlImageLayout,
    natural: Option<Size<f32>>,
    available_width: f32,
    containing_height: Option<f32>,
) -> Option<Size<f32>> {
    let available_width = nonnegative(available_width)?;
    let width = resolve_axis(layout.width, Some(available_width))?;
    let height = resolve_axis(layout.height, containing_height)?;
    let max_width = resolve_axis(layout.max_width, Some(available_width))?
        .map_or(available_width, |maximum| maximum.min(available_width));
    let max_height = resolve_axis(layout.max_height, containing_height)?;
    let natural = natural.filter(valid_size);

    let result = match (width, height) {
        (Some(width), Some(height)) => Size {
            width: width.min(max_width),
            height: cap(height, max_height),
        },
        (Some(width), None) => {
            let natural = natural?;
            let width = width.min(max_width);
            Size {
                width,
                height: cap(
                    proportional(width, natural.width, natural.height)?,
                    max_height,
                ),
            }
        }
        (None, Some(height)) => {
            let natural = natural?;
            let height = cap(height, max_height);
            Size {
                width: proportional(height, natural.height, natural.width)?.min(max_width),
                height,
            }
        }
        (None, None) => fit_natural(natural?, max_width, max_height)?,
    };
    valid_size(&result).then_some(result)
}

/// IM images use their real intrinsic size, width:auto/height:auto, and the
/// frontend's 90px max-height. Border/margins belong to the caller's atom frame.
pub(crate) fn resolve_im_image_size(natural: Size<f32>, available_width: f32) -> Option<Size<f32>> {
    fit_natural(
        natural,
        nonnegative(available_width)?,
        Some(IM_MAX_IMAGE_HEIGHT),
    )
}

fn resolve_axis(
    dimension: HtmlImageDimension,
    percentage_basis: Option<f32>,
) -> Option<Option<f32>> {
    match dimension {
        HtmlImageDimension::Auto => Some(None),
        HtmlImageDimension::Pixels(value) => nonnegative(value).map(Some),
        HtmlImageDimension::Percent(value) => {
            let value = nonnegative(value)?;
            let basis = nonnegative(percentage_basis?)?;
            finite_size(f64::from(basis) * f64::from(value) / 100.).map(Some)
        }
        HtmlImageDimension::Unresolved => None,
    }
}

fn nonnegative(value: f32) -> Option<f32> {
    (value.is_finite() && value >= 0.).then_some(value)
}

fn finite_size(value: f64) -> Option<f32> {
    (value.is_finite() && value >= 0. && value <= f64::from(f32::MAX)).then_some(value as f32)
}

fn valid_size(size: &Size<f32>) -> bool {
    nonnegative(size.width).is_some() && nonnegative(size.height).is_some()
}

fn cap(value: f32, maximum: Option<f32>) -> f32 {
    maximum.map_or(value, |maximum| value.min(maximum))
}

fn proportional(axis: f32, natural_axis: f32, natural_other: f32) -> Option<f32> {
    // A zero natural denominator has no usable ratio. Two explicitly zero
    // axes, or two intrinsic zero axes, do not reach this division.
    (natural_axis > 0.)
        .then(|| f64::from(axis) * f64::from(natural_other) / f64::from(natural_axis))
        .and_then(finite_size)
}

fn fit_natural(natural: Size<f32>, max_width: f32, max_height: Option<f32>) -> Option<Size<f32>> {
    if !valid_size(&natural) {
        return None;
    }
    let mut scale = 1_f64;
    if natural.width > 0. {
        scale = scale.min(f64::from(max_width) / f64::from(natural.width));
    }
    if let Some(max_height) = max_height {
        if natural.height > 0. {
            scale = scale.min(f64::from(max_height) / f64::from(natural.height));
        }
    }
    Some(Size {
        width: finite_size(f64::from(natural.width) * scale)?,
        height: finite_size(f64::from(natural.height) * scale)?,
    })
}

/// Distances above and below a row's common baseline. A small middle-aligned
/// object may have a negative `below`; the base-font strut seeds the final row.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct RowMetrics {
    pub above: f32,
    pub below: f32,
}

impl RowMetrics {
    pub(crate) fn new(above: f32, below: f32) -> Self {
        Self { above, below }
    }

    pub(crate) fn include(&mut self, piece: Self) {
        self.above = self.above.max(piece.above);
        self.below = self.below.max(piece.below);
    }

    pub(crate) fn height(self) -> f32 {
        self.above + self.below
    }

    pub(crate) fn baseline(self, row_top: f32) -> f32 {
        row_top + self.above
    }

    pub(crate) fn piece_origin(self, row_top: f32, anchor: f32) -> f32 {
        self.baseline(row_top) - anchor
    }
}

/// The actual GPUI shaped-line baseline, including half of the font leading.
/// Use this unshifted anchor for the text piece's origin, retaining its existing
/// paint offset when painting superscript or subscript.
pub(crate) fn alignment_baseline(line_height: f32, ascent: f32, descent: f32) -> f32 {
    (line_height - ascent - descent) / 2. + ascent
}

/// Seeds a row or adds a text piece without flattening its baseline shift.
pub(crate) fn text_row_metrics(
    line_height: f32,
    ascent: f32,
    descent: f32,
    paint_offset: f32,
) -> RowMetrics {
    let anchor = alignment_baseline(line_height, ascent, descent);
    RowMetrics::new(anchor - paint_offset, line_height - anchor + paint_offset)
}

/// CSS vertical-align:middle puts the object's center half an x-height above
/// the parent baseline. HTML's default baseline image uses `new(height, 0.)`.
pub(crate) fn half_xheight(height: f32, x_height: f32) -> RowMetrics {
    RowMetrics::new((height + x_height) / 2., (height - x_height) / 2.)
}

#[cfg(test)]
mod tests {
    use super::*;
    use HtmlImageDimension::{Auto, Percent, Pixels, Unresolved};

    fn dimensions(width: HtmlImageDimension, height: HtmlImageDimension) -> HtmlImageLayout {
        HtmlImageLayout {
            width,
            height,
            ..Default::default()
        }
    }

    fn size(width: f32, height: f32) -> Size<f32> {
        Size { width, height }
    }

    #[test]
    fn both_declared_axes_need_no_intrinsic_ratio_and_keep_declared_height() {
        assert_eq!(
            resolve_html_image_size(dimensions(Pixels(32.), Pixels(20.)), None, 600., None),
            Some(size(32., 20.))
        );
        assert_eq!(
            resolve_html_image_size(
                dimensions(Pixels(32.), Pixels(20.)),
                Some(size(400., 100.)),
                16.,
                None
            ),
            Some(size(16., 20.))
        );
    }

    #[test]
    fn width_only_uses_the_loaded_ratio_after_width_constraint() {
        assert_eq!(
            resolve_html_image_size(
                dimensions(Pixels(80.), Auto),
                Some(size(200., 100.)),
                40.,
                None
            ),
            Some(size(40., 20.))
        );
        assert_eq!(
            resolve_html_image_size(dimensions(Pixels(80.), Auto), None, 40., None),
            None
        );
    }

    #[test]
    fn height_only_uses_ratio_but_width_cap_preserves_explicit_height() {
        assert_eq!(
            resolve_html_image_size(
                dimensions(Auto, Pixels(20.)),
                Some(size(200., 100.)),
                600.,
                None
            ),
            Some(size(40., 20.))
        );
        assert_eq!(
            resolve_html_image_size(
                dimensions(Auto, Pixels(20.)),
                Some(size(200., 100.)),
                25.,
                None
            ),
            Some(size(25., 20.))
        );
    }

    #[test]
    fn natural_html_size_scales_down_without_upscaling() {
        assert_eq!(
            resolve_html_image_size(
                HtmlImageLayout::default(),
                Some(size(400., 100.)),
                100.,
                None
            ),
            Some(size(100., 25.))
        );
        assert_eq!(
            resolve_html_image_size(HtmlImageLayout::default(), Some(size(40., 10.)), 100., None),
            Some(size(40., 10.))
        );
    }

    #[test]
    fn width_percent_uses_the_whole_available_width_not_intrinsic_width() {
        assert_eq!(
            resolve_html_image_size(
                dimensions(Percent(50.), Auto),
                Some(size(400., 100.)),
                240.,
                None
            ),
            Some(size(120., 30.))
        );
        assert_eq!(
            resolve_html_image_size(dimensions(Percent(200.), Pixels(20.)), None, 240., None),
            Some(size(240., 20.))
        );
    }

    #[test]
    fn height_percent_requires_a_definite_containing_height() {
        assert_eq!(
            resolve_html_image_size(dimensions(Pixels(30.), Percent(25.)), None, 200., Some(80.)),
            Some(size(30., 20.))
        );
        assert_eq!(
            resolve_html_image_size(dimensions(Pixels(30.), Percent(25.)), None, 200., None),
            None
        );
    }

    #[test]
    fn max_dimensions_cap_each_declared_axis_independently() {
        let layout = HtmlImageLayout {
            width: Pixels(80.),
            height: Pixels(40.),
            max_width: Percent(50.),
            max_height: Pixels(15.),
        };
        assert_eq!(
            resolve_html_image_size(layout, None, 100., None),
            Some(size(50., 15.))
        );
    }

    #[test]
    fn explicit_axis_maximum_recomputes_auto_axis_before_auto_axis_cap() {
        let layout = HtmlImageLayout {
            width: Pixels(80.),
            height: Auto,
            max_width: Pixels(40.),
            max_height: Pixels(15.),
        };
        assert_eq!(
            resolve_html_image_size(layout, Some(size(200., 100.)), 100., None),
            Some(size(40., 15.))
        );
        let layout = HtmlImageLayout {
            width: Auto,
            height: Pixels(40.),
            max_width: Pixels(60.),
            max_height: Pixels(20.),
        };
        assert_eq!(
            resolve_html_image_size(layout, Some(size(200., 100.)), 100., None),
            Some(size(40., 20.))
        );
    }

    #[test]
    fn both_auto_axes_fit_both_maxima_without_distorting_ratio() {
        let layout = HtmlImageLayout {
            max_width: Pixels(100.),
            max_height: Pixels(10.),
            ..Default::default()
        };
        assert_eq!(
            resolve_html_image_size(layout, Some(size(400., 100.)), 600., None),
            Some(size(40., 10.))
        );
    }

    #[test]
    fn zero_dimensions_are_real_sizes_and_never_divide_by_zero() {
        assert_eq!(
            resolve_html_image_size(dimensions(Pixels(0.), Pixels(20.)), None, 600., None),
            Some(size(0., 20.))
        );
        assert_eq!(
            resolve_html_image_size(dimensions(Pixels(20.), Pixels(0.)), None, 600., None),
            Some(size(20., 0.))
        );
        assert_eq!(
            resolve_html_image_size(
                dimensions(Pixels(0.), Auto),
                Some(size(200., 100.)),
                600.,
                None
            ),
            Some(size(0., 0.))
        );
        assert_eq!(
            resolve_html_image_size(
                dimensions(Pixels(10.), Auto),
                Some(size(0., 100.)),
                600.,
                None
            ),
            None
        );
        assert_eq!(
            resolve_html_image_size(HtmlImageLayout::default(), Some(size(0., 0.)), 600., None),
            Some(size(0., 0.))
        );
    }

    #[test]
    fn unresolved_dimensions_do_not_invent_a_pixel_value() {
        assert_eq!(
            resolve_html_image_size(
                dimensions(Unresolved, Pixels(20.)),
                Some(size(100., 50.)),
                600.,
                None
            ),
            None
        );
        assert_eq!(
            resolve_html_image_size(
                HtmlImageLayout {
                    max_height: Unresolved,
                    ..Default::default()
                },
                Some(size(100., 50.)),
                600.,
                None
            ),
            None
        );
    }

    #[test]
    fn invalid_values_and_overflow_do_not_escape_as_geometry() {
        for value in [-1., f32::NAN, f32::INFINITY] {
            assert_eq!(
                resolve_html_image_size(dimensions(Pixels(value), Pixels(20.)), None, 600., None),
                None
            );
            assert_eq!(
                resolve_html_image_size(dimensions(Pixels(20.), Pixels(20.)), None, value, None),
                None
            );
            assert_eq!(resolve_im_image_size(size(value, 20.), 600.), None);
        }
        assert_eq!(
            resolve_html_image_size(
                dimensions(Pixels(f32::MAX), Auto),
                Some(size(1., f32::MAX)),
                f32::MAX,
                None
            ),
            None
        );
    }

    #[test]
    fn large_intermediate_products_keep_a_finite_final_ratio() {
        assert_eq!(
            resolve_html_image_size(
                dimensions(Pixels(1e30), Auto),
                Some(size(1e30, 1e30)),
                1e30,
                None
            ),
            Some(size(1e30, 1e30))
        );
    }

    #[test]
    fn im_images_fit_real_ratio_into_width_and_ninety_pixel_height() {
        assert_eq!(
            resolve_im_image_size(size(400., 200.), 600.),
            Some(size(180., 90.))
        );
        assert_eq!(
            resolve_im_image_size(size(400., 200.), 100.),
            Some(size(100., 50.))
        );
        assert_eq!(
            resolve_im_image_size(size(20., 10.), 600.),
            Some(size(20., 10.))
        );
        assert_eq!(
            resolve_im_image_size(size(400., 200.), 0.),
            Some(size(0., 0.))
        );
        assert_eq!(
            resolve_im_image_size(size(0., 200.), 600.),
            Some(size(0., 90.))
        );
    }

    #[test]
    fn gpui_font_leading_seeds_an_exact_line_box_and_baseline() {
        let metrics = text_row_metrics(20., 12., 4., 0.);
        assert_eq!(alignment_baseline(20., 12., 4.), 14.);
        assert_eq!(metrics, RowMetrics::new(14., 6.));
        assert_eq!(metrics.height(), 20.);
        assert_eq!(metrics.baseline(100.), 114.);
        assert_eq!(metrics.piece_origin(100., 14.), 100.);
    }

    #[test]
    fn html_baseline_image_grows_above_baseline_and_keeps_text_descent() {
        let mut row = text_row_metrics(20., 12., 4., 0.);
        row.include(RowMetrics::new(40., 0.));
        assert_eq!(row, RowMetrics::new(40., 6.));
        assert_eq!(row.height(), 46.);
        assert_eq!(row.piece_origin(100., 40.), 100.);
        assert_eq!(row.piece_origin(100., 14.), 126.);
    }

    #[test]
    fn im_middle_uses_parent_x_height_and_keeps_small_object_negative_below() {
        assert_eq!(half_xheight(40., 8.), RowMetrics::new(24., 16.));
        assert_eq!(half_xheight(2., 8.), RowMetrics::new(5., -3.));
        let mut row = text_row_metrics(20., 12., 4., 0.);
        row.include(half_xheight(40., 8.));
        assert_eq!(row.height(), 40.);
        let image_top = row.piece_origin(100., 24.);
        assert_eq!(image_top + 20., row.baseline(100.) - 4.);
    }

    #[test]
    fn mixed_font_text_shares_baseline_without_bottom_aligning_boxes() {
        let mut row = text_row_metrics(20., 12., 4., 0.);
        let heading = text_row_metrics(34., 23., 5., 0.);
        row.include(heading);
        assert_eq!(row, RowMetrics::new(26., 8.));
        assert_eq!(
            row.piece_origin(100., 14.) + 14.,
            row.piece_origin(100., 26.) + 26.
        );
    }

    #[test]
    fn super_and_subscript_offsets_expand_extents_without_being_cancelled() {
        let mut row = text_row_metrics(20., 12., 4., 0.);
        row.include(text_row_metrics(20., 12., 4., -4.));
        row.include(text_row_metrics(20., 12., 4., 4.));
        assert_eq!(row, RowMetrics::new(18., 10.));
        let text_origin = row.piece_origin(100., alignment_baseline(20., 12., 4.));
        assert_eq!(text_origin - 4. + 14., row.baseline(100.) - 4.);
        assert_eq!(text_origin + 4. + 14., row.baseline(100.) + 4.);
        assert_eq!(text_origin - 4., 100.);
        assert_eq!(text_origin + 4. + 20., 100. + row.height());
    }

    #[test]
    fn row_merging_is_order_independent_for_text_and_objects() {
        let seed = text_row_metrics(20., 12., 4., 0.);
        let mut first = seed;
        first.include(RowMetrics::new(40., 0.));
        first.include(half_xheight(60., 8.));
        let mut second = seed;
        second.include(half_xheight(60., 8.));
        second.include(RowMetrics::new(40., 0.));
        assert_eq!(first, second);
        assert_eq!(first, RowMetrics::new(40., 26.));
    }
}
