//! Natural raster dimensions are physical pixels; GPUI layout uses DIP.
//! Mirrors the list image's Web `zoom: 1 / scale`: natural size and the
//! absolute maximum height shrink, while `max-width: 100%` remains the
//! available logical width. Fixed icon/chip/compact-image boxes do not use
//! this calculation. Original dimensions remain unchanged for metadata.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterLayout {
    pub width_dip: f32,
    pub height_dip: f32,
}

fn positive(value: f32) -> f32 {
    if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    }
}

fn layout_scale(pixels_per_dip: f32) -> f64 {
    if pixels_per_dip.is_finite() && pixels_per_dip > 1.0005 {
        f64::from(pixels_per_dip)
    } else {
        1.0
    }
}

/// The image's CSS zoom factor. Its border, radius and checkerboard lengths
/// also use this factor; surrounding captions and metadata do not.
pub fn image_dpi_compensation(pixels_per_dip: f32) -> f32 {
    (1.0 / layout_scale(pixels_per_dip)) as f32
}

/// Fit a natural image without enlarging its physical pixels.
///
/// `available_width_dip` is the content width after padding/reservations.
/// `max_height_px` is the existing image-height setting before DPI zoom,
/// so the rendered height cap is `max_height_px / pixels_per_dip` DIP.
/// The caller reads the current window scale each render, including after
/// moving between monitors; scale is not part of the decoded image cache.
pub fn natural_image_layout(
    natural_width_px: f32,
    natural_height_px: f32,
    available_width_dip: f32,
    max_height_px: f32,
    pixels_per_dip: f32,
) -> RasterLayout {
    let width = f64::from(positive(natural_width_px));
    let height = f64::from(positive(natural_height_px));
    let available = f64::from(positive(available_width_dip));
    let max_height = f64::from(positive(max_height_px));
    let scale = layout_scale(pixels_per_dip);
    let fit = (available * scale / width)
        .min(max_height / height)
        .min(1.0);
    RasterLayout {
        width_dip: (width * fit / scale) as f32,
        height_dip: (height * fit / scale) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_size(actual: RasterLayout, width: f32, height: f32) {
        assert!((actual.width_dip - width).abs() < 0.001, "{actual:?}");
        assert!((actual.height_dip - height).abs() < 0.001, "{actual:?}");
    }

    #[test]
    fn small_images_keep_their_physical_size_at_100_125_150_percent() {
        for (scale, width, height) in [
            (1.0, 100.0, 50.0),
            (1.25, 80.0, 40.0),
            (1.5, 100.0 / 1.5, 50.0 / 1.5),
        ] {
            assert_size(
                natural_image_layout(100.0, 50.0, 300.0, 160.0, scale),
                width,
                height,
            );
        }
    }

    #[test]
    fn absolute_height_cap_is_compensated_at_100_125_150_percent() {
        for scale in [1.0, 1.25, 1.5] {
            assert_size(
                natural_image_layout(800.0, 400.0, 1000.0, 160.0, scale),
                320.0 / scale,
                160.0 / scale,
            );
        }
    }

    #[test]
    fn percentage_width_cap_is_not_compensated_twice() {
        for scale in [1.0, 1.25, 1.5] {
            assert_size(
                natural_image_layout(1600.0, 200.0, 300.0, 160.0, scale),
                300.0,
                37.5,
            );
        }
    }
}
