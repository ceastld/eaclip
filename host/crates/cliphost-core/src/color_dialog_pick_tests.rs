use super::*;
use std::cell::Cell;

#[test]
fn parses_six_and_eight_digit_colors_without_losing_alpha() {
    for (initial, expected) in [
        ("#123456", (0xff, 0x12, 0x34, 0x56)),
        (" \t#AbCdEf\r\n", (0xff, 0xab, 0xcd, 0xef)),
        ("#00123456", (0x00, 0x12, 0x34, 0x56)),
        ("#ffABCDEF", (0xff, 0xab, 0xcd, 0xef)),
        (" #80a1B2c3 ", (0x80, 0xa1, 0xb2, 0xc3)),
    ] {
        let color = parse_color(initial).expect(initial);
        assert_eq!(
            (color.alpha, color.red, color.green, color.blue),
            expected,
            "{initial}"
        );
    }
}

#[test]
fn picker_receives_bgr_and_exact_owner_and_preserves_original_alpha() {
    let owner = -0x1234_isize;
    for (initial, expected) in [
        ("#00A1B2C3", "#00AABBCC"),
        ("#FFA1B2C3", "#FFAABBCC"),
        ("#80A1B2C3", "#80AABBCC"),
    ] {
        let color = parse_color(initial).unwrap();
        assert_eq!(color.color_ref(), 0x00c3b2a1);
        assert_eq!(color.with_color_ref(0x00ccbbaa), expected);
        let result = run_guarded_color_picker(initial, owner, &|| Ok(()), |rgb, hwnd| {
            assert_eq!(rgb, 0x00c3b2a1);
            assert_eq!(hwnd, owner);
            Ok(Some(0x00ccbbaa))
        });
        assert_eq!(result, Ok(Some(expected.to_string())));
    }
}

#[test]
fn invalid_initial_color_stays_unparsed_but_picker_uses_opaque_white() {
    for initial in [
        "",
        "123456",
        "#123",
        "#12345",
        "#1234567",
        "#123456789",
        "#GG1122",
        "#12 3456",
        "rgba(1,2,3,1)",
    ] {
        let shows = Cell::new(0);
        assert!(parse_color(initial).is_none(), "{initial}");
        let result = run_guarded_color_picker(initial, -7, &|| Ok(()), |rgb, owner| {
            shows.set(shows.get() + 1);
            assert_eq!(rgb, 0x00ffffff, "{initial}");
            assert_eq!(owner, -7, "{initial}");
            Ok(Some(0x00563412))
        });
        assert_eq!(result, Ok(Some("#FF123456".to_string())), "{initial}");
        assert_eq!(shows.get(), 1, "{initial}");
    }
}

#[test]
fn invalid_initial_cancel_returns_none_and_keeps_the_original_draft() {
    let initial = " unfinished-color ".to_string();
    let original = initial.clone();
    let result = run_guarded_color_picker(&initial, 7, &|| Ok(()), |rgb, owner| {
        assert_eq!(rgb, 0x00ffffff);
        assert_eq!(owner, 7);
        Ok(None)
    });
    assert_eq!(result, Ok(None));
    assert_eq!(initial, original);
}

#[test]
fn cancellation_and_native_failure_keep_their_results_after_return_guard() {
    for initial in ["#80123456", "invalid"] {
        for outcome in [Ok(None), Err("native failure".to_string())] {
            let checks = Cell::new(0);
            let verify = || {
                checks.set(checks.get() + 1);
                Ok(())
            };
            let expected = outcome.clone().map(|_| None::<String>);
            let result = run_guarded_color_picker(initial, 9, &verify, |_, _| {
                assert_eq!(checks.get(), 2);
                outcome
            });
            assert_eq!(result, expected, "{initial}");
            assert_eq!(checks.get(), 3, "{initial}");
        }
    }
}

#[test]
fn already_retired_owner_never_opens_picker() {
    for initial in ["#123456", "invalid"] {
        let checks = Cell::new(0);
        let shown = Cell::new(false);
        let verify = || {
            checks.set(checks.get() + 1);
            Err("retired before parsing".to_string())
        };
        let result = run_guarded_color_picker(initial, 11, &verify, |_, _| {
            shown.set(true);
            Ok(Some(0))
        });
        assert_eq!(
            result,
            Err("retired before parsing".to_string()),
            "{initial}"
        );
        assert_eq!(checks.get(), 1, "{initial}");
        assert!(!shown.get(), "{initial}");
    }
}

#[test]
fn retirement_at_the_post_parse_guard_never_opens_picker() {
    for initial in ["#123456", "invalid"] {
        let checks = Cell::new(0);
        let shown = Cell::new(false);
        let verify = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                Err("retired before show".to_string())
            } else {
                Ok(())
            }
        };
        let result = run_guarded_color_picker(initial, 13, &verify, |_, _| {
            shown.set(true);
            Ok(Some(0))
        });
        assert_eq!(result, Err("retired before show".to_string()), "{initial}");
        assert_eq!(checks.get(), 2, "{initial}");
        assert!(!shown.get(), "{initial}");
    }
}

#[test]
fn retirement_during_show_overrides_selection_cancellation_and_failure() {
    for initial in ["#80123456", "invalid"] {
        for outcome in [
            Ok(Some(0x00ccbbaa)),
            Ok(None),
            Err("native failure".to_string()),
        ] {
            let retired = Cell::new(false);
            let checks = Cell::new(0);
            let verify = || {
                checks.set(checks.get() + 1);
                if retired.get() {
                    Err("retired during show".to_string())
                } else {
                    Ok(())
                }
            };
            let result = run_guarded_color_picker(initial, 15, &verify, |_, _| {
                retired.set(true);
                outcome
            });
            assert_eq!(result, Err("retired during show".to_string()), "{initial}");
            assert_eq!(checks.get(), 3, "{initial}");
        }
    }
}

#[test]
fn public_guarded_entry_uses_test_cancel_stub_without_bypassing_guards() {
    let checks = Cell::new(0);
    let verify = || {
        checks.set(checks.get() + 1);
        Ok(())
    };
    assert_eq!(pick_color_guarded("#80123456", 17, &verify), Ok(None));
    assert_eq!(checks.get(), 3);
    assert_eq!(pick_color_guarded("invalid", 17, &verify), Ok(None));
    assert_eq!(checks.get(), 6);
    assert_eq!(
        pick_color_guarded("invalid", 17, &|| Err("retired first".to_string())),
        Err("retired first".to_string())
    );
}
