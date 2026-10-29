//! Cached, premultiplied BGRA pixels for the native shadow companion window.
//!
//! The owner retains its original bounds. Its rounded interior is transparent in
//! this bitmap, so the shadow never paints a border over the WebView content.

const MAX_PIXEL_BYTES: usize = 32 * 1024 * 1024;
const ALPHA_STEPS: usize = 1024;
const EDGE_ALPHA: f64 = 51.0;
const COVERAGE_SAMPLES: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CornerPatch {
    pub size: i32,
    /// Row-major RGB pixels in screen orientation, with no mirrored storage.
    pub pixels: Vec<[u8; 3]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CornerColors {
    /// Top left, top right, bottom left, bottom right, respectively.
    pub corners: [CornerPatch; 4],
}

struct Geometry {
    width: i32,
    height: i32,
    margin: i32,
    radius: f64,
    corner_size: usize,
    pixel_count: usize,
}

impl Geometry {
    fn new(width: i32, height: i32, dpi: u32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let dpi = u64::from(if dpi == 0 { 96 } else { dpi });
        let margin = i32::try_from((15 * dpi).div_ceil(96)).ok()?;
        // Preserve fractional radii while leaving tiny owners a real interior.
        let radius = (5.0 * dpi as f64 / 96.0).min(f64::from(width.min(height) / 2));
        let width = width.checked_add(margin.checked_mul(2)?)?;
        let height = height.checked_add(margin.checked_mul(2)?)?;
        let pixel_count = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?;
        if pixel_count.checked_mul(size_of::<u32>())? > MAX_PIXEL_BYTES {
            return None;
        }
        Some(Self {
            width,
            height,
            margin,
            radius,
            corner_size: radius.ceil() as usize,
            pixel_count,
        })
    }
}

/// The side of each RGB corner ROI, including fractional physical radii.
pub fn corner_size(width: i32, height: i32, dpi: u32) -> i32 {
    Geometry::new(width, height, dpi).map_or(0, |geometry| geometry.corner_size as i32)
}

/// Left/right insets for the owner's top corner rows; mirror them at the bottom.
///
/// Every pixel retained by this region is completely inside the rounded shape.
/// The native companion supplies the partially covered pixels outside it.
pub fn rounded_insets(width: i32, height: i32, dpi: u32) -> Vec<i32> {
    let Some(geometry) = Geometry::new(width, height, dpi) else {
        return Vec::new();
    };
    let mut insets = Vec::new();
    if insets.try_reserve_exact(geometry.corner_size).is_err() {
        return insets;
    }
    for y in 0..geometry.corner_size {
        let dy = geometry.radius - y as f64;
        let horizontal = (geometry.radius * geometry.radius - dy * dy)
            .max(0.0)
            .sqrt();
        insets.push((geometry.radius - horizontal).ceil() as i32);
    }
    insets
}

#[derive(Debug)]
pub struct Raster {
    pub width: i32,
    pub height: i32,
    pub margin: i32,
    #[allow(dead_code)] // Retained for the shadow-only native probes.
    pub radius: i32,
    /// Premultiplied BGRA. The shadow itself is black; optional AA uses corner RGB.
    pub pixels: Vec<u32>,
}

/// Render the shadow around an owner with physical-pixel `width` and `height`.
///
/// Matches the old WPF geometry: 5 DIP corners, a 15 DIP soft shadow span, and
/// 0.4 mask opacity with no directional offset. A blurred mask has half of its
/// opacity at the shape boundary, giving an outside peak alpha of 51/255.
/// Invalid geometry, allocation failure, or a buffer above 32 MiB returns None.
#[allow(dead_code)] // Shadow-only probes do not supply owner corner colors.
pub fn render(width: i32, height: i32, dpi: u32) -> Option<Raster> {
    render_internal(width, height, dpi, None)
}

/// Add antialiased rounded-owner fringe to the compatible shadow-only bitmap.
/// Patches must have `corner_size(width, height, dpi)` square RGB pixels each.
pub fn render_with_corners(
    width: i32,
    height: i32,
    dpi: u32,
    corners: &CornerColors,
) -> Option<Raster> {
    render_internal(width, height, dpi, Some(corners))
}

fn render_internal(
    width: i32,
    height: i32,
    dpi: u32,
    corners: Option<&CornerColors>,
) -> Option<Raster> {
    let geometry = Geometry::new(width, height, dpi)?;
    let patch_size = geometry.corner_size;
    if let Some(corners) = corners {
        if corners.corners.iter().any(|patch| {
            patch.size != patch_size as i32 || patch.pixels.len() != patch_size * patch_size
        }) {
            return None;
        }
    }

    let mut pixels = Vec::new();
    pixels.try_reserve_exact(geometry.pixel_count).ok()?;
    pixels.resize(geometry.pixel_count, 0);

    let mut alpha = [0u8; ALPHA_STEPS + 1];
    let tail = (-4.5_f64).exp();
    for (step, value) in alpha.iter_mut().enumerate() {
        let distance = step as f64 / ALPHA_STEPS as f64;
        let falloff = ((-4.5 * distance * distance).exp() - tail) / (1.0 - tail);
        *value = (EDGE_ALPHA * falloff).round() as u8;
    }
    let shadow_alpha_at = |distance: f64| -> u8 {
        // Leave the last pixel ring fully transparent even at very small DPI.
        if distance >= f64::from(geometry.margin) - 0.5 {
            return 0;
        }
        let step =
            (distance.max(0.0) / f64::from(geometry.margin) * ALPHA_STEPS as f64).round() as usize;
        alpha[step.min(ALPHA_STEPS)]
    };
    let shadow_at = |distance: f64| -> u32 {
        if distance <= 0.0 {
            0
        } else {
            u32::from(shadow_alpha_at(distance)) << 24
        }
    };

    let rw = geometry.width as usize;
    let rh = geometry.height as usize;
    let margin = geometry.margin as usize;
    let corner = margin + patch_size;

    // Straight bands need neither square roots nor per-pixel exponentials.
    for y in 0..margin {
        let pixel = shadow_at((margin - y) as f64 - 0.5);
        pixels[y * rw + corner..(y + 1) * rw - corner].fill(pixel);
        let bottom = rh - 1 - y;
        pixels[bottom * rw + corner..(bottom + 1) * rw - corner].fill(pixel);
    }
    for x in 0..margin {
        let pixel = shadow_at((margin - x) as f64 - 0.5);
        for y in corner..rh - corner {
            pixels[y * rw + x] = pixel;
            pixels[y * rw + rw - 1 - x] = pixel;
        }
    }

    // Only evaluate one corner quadrant and mirror its result to the other
    // corners. Pixels inside the rounded owner remain zero from initialization.
    for y in 0..corner {
        let dy = (margin as f64 + geometry.radius - y as f64 - 0.5).max(0.0);
        for x in 0..corner {
            let dx = (margin as f64 + geometry.radius - x as f64 - 0.5).max(0.0);
            let pixel = shadow_at((dx * dx + dy * dy).sqrt() - geometry.radius);
            pixels[y * rw + x] = pixel;
            pixels[y * rw + rw - 1 - x] = pixel;
            pixels[(rh - 1 - y) * rw + x] = pixel;
            pixels[(rh - 1 - y) * rw + rw - 1 - x] = pixel;
        }
    }

    if let Some(corners) = corners {
        for y in 0..patch_size {
            for x in 0..patch_size {
                let Some(coverage) = fringe_coverage(x, y, geometry.radius) else {
                    continue;
                };
                if coverage == 0 {
                    continue;
                }
                let dx = (geometry.radius - x as f64 - 0.5).max(0.0);
                let dy = (geometry.radius - y as f64 - 0.5).max(0.0);
                let shadow_alpha = shadow_alpha_at((dx * dx + dy * dy).sqrt() - geometry.radius);
                let targets = [
                    ((margin + y) * rw + margin + x, y * patch_size + x),
                    (
                        (margin + y) * rw + rw - 1 - margin - x,
                        y * patch_size + patch_size - 1 - x,
                    ),
                    (
                        (rh - 1 - margin - y) * rw + margin + x,
                        (patch_size - 1 - y) * patch_size + x,
                    ),
                    (
                        (rh - 1 - margin - y) * rw + rw - 1 - margin - x,
                        (patch_size - 1 - y) * patch_size + patch_size - 1 - x,
                    ),
                ];
                for (corner, (target, source)) in targets.into_iter().enumerate() {
                    pixels[target] = fringe_pixel(
                        corners.corners[corner].pixels[source],
                        coverage,
                        shadow_alpha,
                    );
                }
            }
        }
    }

    Some(Raster {
        width: geometry.width,
        height: geometry.height,
        margin: margin as i32,
        // This integer field is kept for callers of the original shadow-only API.
        // Region geometry must use rounded_insets, which retains the raw radius.
        radius: geometry.radius.round() as i32,
        pixels,
    })
}

/// None means the opaque owner already supplies this entire pixel.
fn fringe_coverage(x: usize, y: usize, radius: f64) -> Option<u32> {
    let radius_squared = radius * radius;
    let far_x = (radius - x as f64).max(0.0);
    let far_y = (radius - y as f64).max(0.0);
    if far_x * far_x + far_y * far_y <= radius_squared {
        return None;
    }
    let near_x = (radius - x as f64 - 1.0).max(0.0);
    let near_y = (radius - y as f64 - 1.0).max(0.0);
    if near_x * near_x + near_y * near_y >= radius_squared {
        return Some(0);
    }
    let mut covered = 0;
    for sample_y in 0..COVERAGE_SAMPLES {
        let dy = (radius - y as f64 - (f64::from(sample_y) + 0.5) / f64::from(COVERAGE_SAMPLES))
            .max(0.0);
        for sample_x in 0..COVERAGE_SAMPLES {
            let dx =
                (radius - x as f64 - (f64::from(sample_x) + 0.5) / f64::from(COVERAGE_SAMPLES))
                    .max(0.0);
            if dx * dx + dy * dy <= radius_squared {
                covered += 1;
            }
        }
    }
    Some(covered)
}

fn fringe_pixel(rgb: [u8; 3], coverage: u32, shadow_alpha: u8) -> u32 {
    let samples = COVERAGE_SAMPLES * COVERAGE_SAMPLES;
    let alpha =
        (coverage * 255 + (samples - coverage) * u32::from(shadow_alpha) + samples / 2) / samples;
    let channel = |value: u8| (u32::from(value) * coverage + samples / 2) / samples;
    (alpha << 24) | (channel(rgb[0]) << 16) | (channel(rgb[1]) << 8) | channel(rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(raster: &Raster, x: i32, y: i32) -> u32 {
        raster.pixels[(y * raster.width + x) as usize]
    }

    #[test]
    fn only_the_outer_soft_shadow_is_painted() {
        let raster = render(120, 80, 96).unwrap();
        assert_eq!(
            (raster.width, raster.height, raster.margin, raster.radius),
            (150, 110, 15, 5)
        );
        let middle_y = raster.height / 2;
        for x in raster.margin..raster.width - raster.margin {
            assert_eq!(pixel(&raster, x, middle_y), 0);
        }
        assert!(pixel(&raster, raster.margin - 1, middle_y) >> 24 > 0);
        assert!(pixel(&raster, raster.margin, raster.margin) >> 24 > 0);
        for x in 0..raster.width {
            assert_eq!(pixel(&raster, x, 0), 0);
            assert_eq!(pixel(&raster, x, raster.height - 1), 0);
        }
        for y in 0..raster.height {
            assert_eq!(pixel(&raster, 0, y), 0);
            assert_eq!(pixel(&raster, raster.width - 1, y), 0);
        }
        assert!(raster
            .pixels
            .iter()
            .all(|value| value & 0x00ff_ffff == 0 && value >> 24 <= 51));
    }

    #[test]
    fn dpi_scales_geometry_and_invalid_or_oversized_buffers_are_rejected() {
        let standard = render(120, 80, 96).unwrap();
        let scaled = render(240, 160, 192).unwrap();
        assert_eq!((scaled.margin, scaled.radius), (30, 10));
        let standard_alpha = pixel(&standard, 7, standard.height / 2) >> 24;
        let scaled_alpha = pixel(&scaled, 14, scaled.height / 2) >> 24;
        assert!(standard_alpha.abs_diff(scaled_alpha) <= 2);
        // At 125% the 5 DIP radius is 6.25 physical pixels, not rounded to 6.
        let insets = rounded_insets(120, 80, 120);
        assert_eq!(corner_size(120, 80, 120), 7);
        assert_eq!((insets.len(), insets[0]), (7, 7));
        let radius = 6.25_f64;
        for (y, &inset) in insets.iter().enumerate() {
            let dy = radius - y as f64;
            let inside_dx = (radius - f64::from(inset)).max(0.0);
            assert!(inside_dx * inside_dx + dy * dy <= radius * radius);
            if inset > 0 {
                let outside_dx = radius - f64::from(inset - 1);
                assert!(outside_dx * outside_dx + dy * dy > radius * radius);
            }
        }
        assert_eq!(render(1, 1, 96).unwrap().radius, 0);
        assert!(rounded_insets(1, 1, 96).is_empty());
        assert!(render(0, 80, 96).is_none());
        assert!(render(-1, 80, 96).is_none());
        assert!(render(4096, 4096, 96).is_none());
        assert!(render(i32::MAX, 1, 96).is_none());
        assert!(render(120, 80, u32::MAX).is_none());
    }

    #[test]
    fn corner_coverage_is_partial_symmetric_and_confined_to_the_four_rois() {
        let side = corner_size(120, 80, 120);
        let patches = CornerColors {
            corners: std::array::from_fn(|_| CornerPatch {
                size: side,
                pixels: vec![[64, 128, 192]; (side * side) as usize],
            }),
        };
        let shadow = render(120, 80, 120).unwrap();
        let aa = render_with_corners(120, 80, 120, &patches).unwrap();
        let edge = pixel(&aa, aa.margin + 1, aa.margin + 2);
        assert!(edge >> 24 > 51 && edge >> 24 < 255);
        assert!(edge & 0x00ff_ffff != 0);
        let mut changes = 0;
        for y in 0..aa.height {
            for x in 0..aa.width {
                let value = pixel(&aa, x, y);
                assert_eq!(value, pixel(&aa, aa.width - 1 - x, y));
                assert_eq!(value, pixel(&aa, x, aa.height - 1 - y));
                if value != pixel(&shadow, x, y) {
                    changes += 1;
                    let near_x = (x >= aa.margin && x < aa.margin + side)
                        || (x >= aa.width - aa.margin - side && x < aa.width - aa.margin);
                    let near_y = (y >= aa.margin && y < aa.margin + side)
                        || (y >= aa.height - aa.margin - side && y < aa.height - aa.margin);
                    assert!(
                        near_x && near_y,
                        "AA changed a pixel outside its corner ROI"
                    );
                }
                assert!(value & 255 <= value >> 24);
                assert!((value >> 8) & 255 <= value >> 24);
                assert!((value >> 16) & 255 <= value >> 24);
            }
        }
        assert!(changes > 0);
        assert_eq!(pixel(&aa, aa.width / 2, aa.height / 2), 0);

        // Real screen patches retain their orientation at the mirrored corners.
        let mut screen_patches = patches.clone();
        for corner in &mut screen_patches.corners {
            corner.pixels.fill([0, 0, 0]);
        }
        screen_patches.corners[1].pixels[(2 * side + side - 2) as usize] = [255, 0, 0];
        let screen_aa = render_with_corners(120, 80, 120, &screen_patches).unwrap();
        let right_edge = pixel(
            &screen_aa,
            screen_aa.width - 1 - screen_aa.margin - 1,
            screen_aa.margin + 2,
        );
        assert!(right_edge & 0x00ff_0000 > 0);
        assert_eq!(right_edge & 0x0000_ffff, 0);
        screen_patches.corners[0].pixels.pop();
        assert!(render_with_corners(120, 80, 120, &screen_patches).is_none());
    }
}
