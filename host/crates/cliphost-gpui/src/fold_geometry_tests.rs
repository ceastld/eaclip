//! Pure Fold geometry only. These tests inject current HWND measurements;
//! they do not prove native measurement or message-hook integration.
use super::*;

#[test]
fn folded_36_dip_scales_with_current_dpi_and_actual_frame_inset() {
    for (dpi, physical_client) in [(96, 36), (120, 45), (144, 54), (192, 72)] {
        for inset in [0, 16, 23] {
            assert_eq!(
                minimum_outer_height(36.0, dpi, 600 + inset, 600),
                Some(physical_client + inset as i32),
                "DPI {dpi}, measured inset {inset}"
            );
        }
    }
}

#[test]
fn expanded_and_already_folded_measurements_do_not_accumulate_frame_inset() {
    for (dpi, physical_client) in [(96, 36), (120, 45), (144, 54), (192, 72)] {
        for inset in [0, 16, 29] {
            let expected = physical_client + inset;
            let mut outer = minimum_outer_height(36.0, dpi, 900 + inset, 900).unwrap();
            assert_eq!(i64::from(outer), expected);
            // A later minimum-size message measures the already folded frame.
            // Reusing that outer height must retain one inset, not add another.
            for _ in 0..3 {
                outer = minimum_outer_height(36.0, dpi, i64::from(outer), physical_client).unwrap();
                assert_eq!(i64::from(outer), expected);
            }
        }
    }
}

#[test]
fn changed_live_dpi_and_nonstandard_inset_replace_previous_geometry() {
    assert_eq!(minimum_outer_height(36.0, 96, 416, 400), Some(52));
    // There is no GPUI DPI/frame-cache parameter: only this current measured
    // inset of 28 pixels and current 192-DPI scale determine the next result.
    assert_eq!(minimum_outer_height(36.0, 192, 428, 400), Some(100));
    assert_eq!(minimum_outer_height(36.0, 144, 431, 400), Some(85));
}

#[test]
fn invalid_target_dpi_and_inverted_or_negative_measurements_are_rejected() {
    for target in [0.0, -36.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(minimum_outer_height(target, 96, 416, 400), None);
    }
    assert_eq!(minimum_outer_height(36.0, 0, 416, 400), None);
    for (outer, client) in [(-1, 0), (10, -1), (-1, -1), (399, 400), (i64::MIN, 0)] {
        assert_eq!(minimum_outer_height(36.0, 96, outer, client), None);
    }
}

#[test]
fn physical_height_and_inset_sum_must_fit_i32_without_overflow() {
    let maximum = f64::from(i32::MAX);
    assert_eq!(minimum_outer_height(maximum, 96, 400, 400), Some(i32::MAX));
    assert_eq!(minimum_outer_height(maximum + 1.0, 96, 400, 400), None);
    assert_eq!(minimum_outer_height(f64::MAX, 192, 400, 400), None);
    assert_eq!(minimum_outer_height(maximum, 96, 416, 400), None);
    assert_eq!(minimum_outer_height(36.0, 96, i64::from(i32::MAX), 0), None);
    assert_eq!(minimum_outer_height(1.0, 96, i64::MAX, 0), None);
}
