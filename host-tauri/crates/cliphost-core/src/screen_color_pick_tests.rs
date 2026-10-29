use super::*;

fn bounds(x: i32, y: i32, width: i32, height: i32) -> VirtualBounds {
    VirtualBounds {
        x,
        y,
        width,
        height,
    }
}

fn rect(rect: PixelRect) -> (i32, i32, i32, i32) {
    (rect.x, rect.y, rect.width, rect.height)
}

/// A 3 x 2 BGRA buffer: every pixel is (r, g, b) = (10 * i, 20 * i, 30 * i) + 1.
fn buffer() -> Vec<u8> {
    (0..6u8)
        .flat_map(|i| [30 * i + 1, 20 * i + 1, 10 * i + 1, 0xff])
        .collect()
}

#[test]
fn pixel_at_converts_bgra_to_rgb_and_rejects_everything_outside() {
    let data = buffer();
    assert_eq!(pixel_at(&data, 3, 2, 0, 0), Some((1, 1, 1)));
    assert_eq!(pixel_at(&data, 3, 2, 2, 0), Some((21, 41, 61)));
    assert_eq!(pixel_at(&data, 3, 2, 0, 1), Some((31, 61, 91)));
    assert_eq!(pixel_at(&data, 3, 2, 2, 1), Some((51, 101, 151)));
    for (x, y) in [(-1, 0), (0, -1), (3, 0), (0, 2), (i32::MAX, i32::MAX)] {
        assert_eq!(pixel_at(&data, 3, 2, x, y), None, "{x},{y}");
    }
    assert_eq!(pixel_at(&data, 0, 2, 0, 0), None);
    assert_eq!(pixel_at(&data, 3, -2, 0, 0), None);
    // A buffer shorter than its stated size never panics.
    assert_eq!(pixel_at(&data[..23], 3, 2, 2, 1), None);
    assert_eq!(pixel_at(&[], 3, 2, 0, 0), None);
}

#[test]
fn snapshot_point_follows_a_negative_virtual_origin() {
    // A monitor left of and above the primary one: the virtual screen starts negative.
    let screen = bounds(-1920, -300, 3840, 1380);
    assert_eq!(snapshot_point((-1920, -300), screen), Some((0, 0)));
    assert_eq!(snapshot_point((0, 0), screen), Some((1920, 300)));
    assert_eq!(snapshot_point((1919, 1079), screen), Some((3839, 1379)));
    assert_eq!(snapshot_point((-1921, 0), screen), None);
    assert_eq!(snapshot_point((0, -301), screen), None);
    assert_eq!(snapshot_point((1920, 0), screen), None);
    assert_eq!(snapshot_point((0, 1080), screen), None);
}

#[test]
fn magnifier_sits_down_and_right_of_the_cursor_by_default() {
    let screen = bounds(0, 0, 1920, 1080);
    let placed = magnifier_rect((100, 200), screen, 1.0);
    assert_eq!(
        rect(placed),
        (124, 224, MAGNIFIER_WIDTH, MAGNIFIER_HEIGHT)
    );
}

#[test]
fn magnifier_flips_to_the_other_side_at_the_right_and_bottom_edges() {
    let screen = bounds(0, 0, 1920, 1080);
    let size = 1.0;
    // Right edge: left of the cursor instead.
    let right = magnifier_rect((1900, 200), screen, size);
    assert_eq!(right.x, 1900 - MAGNIFIER_OFFSET - MAGNIFIER_WIDTH);
    assert_eq!(right.y, 224);
    // Bottom edge: above the cursor instead.
    let bottom = magnifier_rect((100, 1070), screen, size);
    assert_eq!(bottom.y, 1070 - MAGNIFIER_OFFSET - MAGNIFIER_HEIGHT);
    assert_eq!(bottom.x, 124);
    // Corner flips both ways.
    let corner = magnifier_rect((1919, 1079), screen, size);
    assert_eq!(corner.x, 1919 - MAGNIFIER_OFFSET - MAGNIFIER_WIDTH);
    assert_eq!(corner.y, 1079 - MAGNIFIER_OFFSET - MAGNIFIER_HEIGHT);
}

#[test]
fn magnifier_stays_inside_when_neither_side_fits_or_the_origin_is_negative() {
    let size = 1.0;
    // Neither side fits: the flipped position is clamped back onto the screen.
    let tiny = bounds(0, 0, 150, 160);
    let squeezed = magnifier_rect((75, 80), tiny, size);
    assert_eq!((squeezed.x, squeezed.y), (0, 0));
    // Negative virtual origin: all arithmetic is in screen coordinates.
    let screen = bounds(-1920, -200, 3840, 1280);
    let left_monitor = magnifier_rect((-1910, -190), screen, size);
    assert_eq!((left_monitor.x, left_monitor.y), (-1886, -166));
    let right_edge = magnifier_rect((1915, 1075), screen, size);
    assert_eq!(right_edge.x, 1915 - MAGNIFIER_OFFSET - MAGNIFIER_WIDTH);
    assert_eq!(right_edge.y, 1075 - MAGNIFIER_OFFSET - MAGNIFIER_HEIGHT);
    // A magnifier larger than the whole screen pins to its origin.
    let sliver = magnifier_rect((5, 5), bounds(-50, -40, 60, 70), size);
    assert_eq!((sliver.x, sliver.y), (-50, -40));
    // Whatever the cursor, the result is inside the screen when it can be.
    for cursor in [(-1920, -200), (0, 0), (1919, 1079), (-1920, 1079), (1919, -200)] {
        let placed = magnifier_rect(cursor, screen, size);
        assert!(placed.x >= screen.x && placed.x + placed.width <= screen.right());
        assert!(placed.y >= screen.y && placed.y + placed.height <= screen.bottom());
    }
}

#[test]
fn dpi_maps_to_a_bounded_scale() {
    assert_eq!(scale_for_dpi(96), 1.0);
    assert_eq!(scale_for_dpi(120), 1.25);
    assert_eq!(scale_for_dpi(144), 1.5);
    assert_eq!(scale_for_dpi(192), 2.0);
    assert_eq!(scale_for_dpi(0), 1.0);
    assert_eq!(scale_for_dpi(48), 1.0);
    assert_eq!(scale_for_dpi(10_000), MAX_SCALE);
}

#[test]
fn metrics_scale_every_size_but_keep_the_eleven_by_eleven_samples() {
    let base = MagnifierMetrics::for_scale(1.0);
    assert_eq!(
        (base.cell, base.offset, base.frame, base.text_height, base.font),
        (CELL, MAGNIFIER_OFFSET, MAGNIFIER_FRAME, MAGNIFIER_TEXT_HEIGHT, 16)
    );
    assert_eq!((base.width(), base.height()), (MAGNIFIER_WIDTH, MAGNIFIER_HEIGHT));
    let one_and_a_half = MagnifierMetrics::for_scale(1.5);
    assert_eq!(
        (
            one_and_a_half.cell,
            one_and_a_half.offset,
            one_and_a_half.frame,
            one_and_a_half.text_height,
            one_and_a_half.font
        ),
        (15, 36, 3, 36, 24)
    );
    assert_eq!(one_and_a_half.grid(), 165);
    assert_eq!(one_and_a_half.width(), 165 + 6);
    assert_eq!(one_and_a_half.height(), 165 + 6 + 36);
    let double = MagnifierMetrics::for_scale(2.0);
    assert_eq!((double.cell, double.offset, double.frame), (20, 48, 4));
    // Out-of-range or non-finite input falls back to the bounded range.
    assert_eq!(MagnifierMetrics::for_scale(f32::NAN), base);
    assert_eq!(MagnifierMetrics::for_scale(0.1), base);
    assert_eq!(MagnifierMetrics::for_scale(99.0), MagnifierMetrics::for_scale(MAX_SCALE));
    // The sample grid is independent of the scale.
    assert_eq!(sample_grid((50, 50), bounds(0, 0, 100, 100))[5][5], (50, 50));
}

#[test]
fn magnifier_rect_scales_the_offset_and_flips_with_the_larger_size() {
    let screen = bounds(0, 0, 3840, 2160);
    let metrics = MagnifierMetrics::for_scale(2.0);
    let placed = magnifier_rect((100, 200), screen, 2.0);
    assert_eq!(
        (placed.x, placed.y, placed.width, placed.height),
        (148, 248, metrics.width(), metrics.height())
    );
    // Right/bottom flip uses the scaled offset and size.
    let flipped = magnifier_rect((3830, 2150), screen, 2.0);
    assert_eq!(flipped.x, 3830 - 48 - metrics.width());
    assert_eq!(flipped.y, 2150 - 48 - metrics.height());
    // A negative-origin screen with a scaled magnifier stays inside.
    let left = bounds(-2560, -400, 5120, 1840);
    for cursor in [(-2560, -400), (2559, 1439), (-2560, 1439), (0, 0)] {
        for scale in [1.0_f32, 1.25, 1.5, 2.0, 3.0] {
            let placed = magnifier_rect(cursor, left, scale);
            assert!(placed.x >= left.x && placed.x + placed.width <= left.right());
            assert!(placed.y >= left.y && placed.y + placed.height <= left.bottom());
        }
    }
}

#[test]
fn sample_grid_is_eleven_by_eleven_around_the_cursor() {
    let screen = bounds(0, 0, 100, 100);
    let cells = sample_grid((50, 60), screen);
    assert_eq!(cells.len(), 11);
    assert!(cells.iter().all(|row| row.len() == 11));
    assert_eq!(cells[0][0], (45, 55));
    assert_eq!(cells[5][5], (50, 60));
    assert_eq!(cells[10][10], (55, 65));
    assert_eq!(cells[0][10], (55, 55));
    assert_eq!(cells[10][0], (45, 65));
}

#[test]
fn sample_grid_clamps_to_every_edge_including_negative_origins() {
    let screen = bounds(-100, -50, 200, 120); // x in -100..=99, y in -50..=69
    let corner = sample_grid((-100, -50), screen);
    assert_eq!(corner[0][0], (-100, -50));
    assert_eq!(corner[5][5], (-100, -50));
    assert_eq!(corner[10][10], (-95, -45));
    assert_eq!(corner[0][10], (-95, -50));
    let far = sample_grid((99, 69), screen);
    assert_eq!(far[0][0], (94, 64));
    assert_eq!(far[5][5], (99, 69));
    assert_eq!(far[10][10], (99, 69));
    assert_eq!(far[10][0], (94, 69));
    // A one-pixel screen repeats its only pixel.
    let one = sample_grid((7, 7), bounds(7, 7, 1, 1));
    assert!(one.iter().flatten().all(|&cell| cell == (7, 7)));
    // Every cell of every cursor position stays inside the screen.
    for x in (-100..100).step_by(7) {
        for y in (-50..70).step_by(7) {
            for cell in sample_grid((x, y), screen).iter().flatten() {
                assert!(cell.0 >= -100 && cell.0 <= 99 && cell.1 >= -50 && cell.1 <= 69);
            }
        }
    }
}

#[test]
fn magnifier_geometry_constants_match_the_spec() {
    // 11 x 11 cells at 10x, 2px frame, a text strip below.
    assert_eq!(GRID * CELL, 110);
    assert_eq!(MAGNIFIER_WIDTH, 114);
    assert_eq!(MAGNIFIER_HEIGHT, 114 + MAGNIFIER_TEXT_HEIGHT);
    assert_eq!(MAGNIFIER_OFFSET, 24);
    assert_eq!(MAGNIFIER_FONT, 16);
    assert_eq!(OVERLAY_TIMEOUT.as_secs(), 60);
}

#[test]
fn hex_text_and_contrast_color() {
    assert_eq!(hex_text((0, 0, 0)), "#000000");
    assert_eq!(hex_text((255, 128, 1)), "#FF8001");
    assert_eq!(contrast_color((0, 0, 0)), (255, 255, 255));
    assert_eq!(contrast_color((255, 255, 255)), (0, 0, 0));
    assert_eq!(contrast_color((0, 0x78, 0xd4)), (255, 255, 255));
    assert_eq!(contrast_color((255, 185, 0)), (0, 0, 0));
}

#[test]
fn capturable_bounds_reject_empty_negative_and_absurd_sizes() {
    assert!(bounds(-1920, 0, 3840, 1080).is_capturable());
    assert!(!bounds(0, 0, 0, 1080).is_capturable());
    assert!(!bounds(0, 0, 1920, -1).is_capturable());
    assert!(!bounds(0, 0, 65_537, 10).is_capturable());
    assert!(!bounds(0, 0, 65_536, 65_536).is_capturable());
}

#[test]
fn disabled_native_dialogs_never_create_an_overlay_and_never_poll_the_caller() {
    // Without CLIPHOST_TAURI_ALLOW_DIALOGS=1 (or when headless) there is no overlay at all.
    let polled = std::cell::Cell::new(false);
    let result = pick_screen_color(0, &|| {
        polled.set(true);
        true
    });
    assert_eq!(result, Ok(None));
    assert!(!polled.get());
}

#[test]
fn test_builds_stub_the_overlay_even_with_dialogs_enabled() {
    // The stub is what keeps tests from covering a desktop; it ignores the lease.
    assert_eq!(show_overlay(0, &|| true), Ok(None));
    assert_eq!(show_overlay(1234, &|| false), Ok(None));
}
