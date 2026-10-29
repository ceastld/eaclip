//! Product keyboard selection rules mirrored from ClipboardPage's range,
//! sequential Space selection and base-filter category navigation.
use super::*;

fn state(active: Option<usize>, ids: &[&str], anchor: Option<usize>) -> SelectionState {
    SelectionState {
        active,
        selected_ids: ids.iter().map(|id| (*id).to_string()).collect(),
        anchor,
    }
}

fn expect_selection(
    rows: &[&str],
    previous: &SelectionState,
    key: Key,
    shift: bool,
    expected: SelectionState,
    expected_reveal: Option<usize>,
) -> SelectionState {
    match transition(rows, previous, key, shift) {
        Transition::Selection { state, reveal } => {
            assert_eq!(state, expected);
            assert_eq!(reveal, expected_reveal);
            state
        }
        other => panic!("expected selection transition, got {other:?}"),
    }
}

#[test]
fn key_aliases_and_primary_tab_toggle_match_web_and_gpui_names() {
    for name in ["ArrowLeft", "left"] {
        assert!(matches!(Key::from_name(name), Some(Key::Left)));
    }
    for name in ["ArrowRight", "right"] {
        assert!(matches!(Key::from_name(name), Some(Key::Right)));
    }
    for name in ["ArrowUp", "up"] {
        assert!(matches!(Key::from_name(name), Some(Key::Up)));
    }
    for name in ["ArrowDown", "down"] {
        assert!(matches!(Key::from_name(name), Some(Key::Down)));
    }
    for name in ["Home", "home"] {
        assert!(matches!(Key::from_name(name), Some(Key::Home)));
    }
    for name in ["End", "end"] {
        assert!(matches!(Key::from_name(name), Some(Key::End)));
    }
    for name in [" ", "Space", "Spacebar", "space"] {
        assert!(matches!(Key::from_name(name), Some(Key::Space)));
    }
    for name in ["Enter", "Tab", "Escape", "a"] {
        assert!(Key::from_name(name).is_none());
    }
    assert_eq!(primary_tab_route(true), "/clipboard");
    assert_eq!(primary_tab_route(false), "/favorites");
}

#[test]
fn horizontal_categories_cycle_from_known_or_unknown_selection_and_ignore_shift() {
    assert_eq!(category_index(4, Some(0), Direction::Previous), Some(3));
    assert_eq!(category_index(4, Some(3), Direction::Next), Some(0));
    assert_eq!(category_index(4, None, Direction::Previous), Some(3));
    assert_eq!(category_index(4, None, Direction::Next), Some(1));
    assert_eq!(category_index(1, None, Direction::Next), Some(0));
    assert_eq!(category_index(0, None, Direction::Previous), None);
    assert_eq!(category_index(0, Some(0), Direction::Next), None);
    let selected = state(Some(0), &["a"], Some(0));
    for shift in [false, true] {
        assert_eq!(
            transition(&["a"], &selected, Key::Left, shift),
            Transition::Category(Direction::Previous)
        );
        assert_eq!(
            transition(&["a"], &selected, Key::Right, shift),
            Transition::Category(Direction::Next)
        );
        // Categories are independent of the current row list; an empty
        // category still permits switching to another base filter.
        assert_eq!(
            transition(&[], &selected, Key::Left, shift),
            Transition::Category(Direction::Previous)
        );
        assert_eq!(
            transition(&[], &selected, Key::Right, shift),
            Transition::Category(Direction::Next)
        );
    }
}

#[test]
fn shift_ranges_keep_original_anchor_and_shrink_when_direction_reverses() {
    let rows = ["a", "b", "c", "d", "e"];
    let mut selected = state(Some(2), &["c"], Some(2));
    for (name, active, ids) in [
        ("up", 1, vec!["b", "c"]),
        ("up", 0, vec!["a", "b", "c"]),
        ("down", 1, vec!["b", "c"]),
        ("down", 2, vec!["c"]),
        ("down", 3, vec!["c", "d"]),
        ("home", 0, vec!["a", "b", "c"]),
        ("end", 4, vec!["c", "d", "e"]),
        ("up", 3, vec!["c", "d"]),
    ] {
        selected = expect_selection(
            &rows,
            &selected,
            Key::from_name(name).unwrap(),
            true,
            state(Some(active), &ids, Some(2)),
            Some(active),
        );
    }
}

#[test]
fn missing_active_uses_web_zero_base_and_shift_keeps_absent_anchor() {
    let rows = ["a", "b", "c"];
    let empty = SelectionState::default();
    expect_selection(
        &rows,
        &empty,
        Key::Down,
        false,
        state(Some(1), &["b"], Some(1)),
        Some(1),
    );
    expect_selection(
        &rows,
        &empty,
        Key::Up,
        false,
        state(Some(0), &["a"], Some(0)),
        Some(0),
    );
    expect_selection(
        &rows,
        &empty,
        Key::Down,
        true,
        state(Some(1), &["a", "b"], None),
        Some(1),
    );
    expect_selection(
        &rows,
        &empty,
        Key::Up,
        true,
        state(Some(0), &["a"], None),
        Some(0),
    );
    // Home/End pass the absent active (-1 in WebView) into selectRange;
    // clamping affects the range, while selectRange never creates an anchor.
    expect_selection(
        &rows,
        &empty,
        Key::Home,
        true,
        state(Some(0), &["a"], None),
        Some(0),
    );
    expect_selection(
        &rows,
        &empty,
        Key::End,
        true,
        state(Some(2), &["a", "b", "c"], None),
        Some(2),
    );
}

#[test]
fn space_without_live_selection_selects_first_row_even_with_stale_ids() {
    let rows = ["a", "b", "c"];
    expect_selection(
        &rows,
        &SelectionState::default(),
        Key::Space,
        false,
        state(Some(0), &["a"], Some(0)),
        Some(0),
    );
    expect_selection(
        &rows,
        &state(Some(2), &["gone"], Some(2)),
        Key::Space,
        false,
        state(Some(0), &["a"], Some(0)),
        Some(0),
    );
}

#[test]
fn space_fills_holes_from_raw_selected_min_and_repeats_down_to_last_row() {
    let rows = ["a", "b", "c", "d", "e"];
    let scattered = state(Some(0), &["d", "gone", "b"], Some(4));
    expect_selection(
        &rows,
        &scattered,
        Key::Space,
        false,
        state(Some(4), &["b", "c", "d", "e"], Some(1)),
        Some(4),
    );
    let mut selected = state(Some(1), &["b"], Some(1));
    for active in 2..=4 {
        selected = expect_selection(
            &rows,
            &selected,
            Key::Space,
            false,
            state(Some(active), &rows[1..=active], Some(1)),
            Some(active),
        );
    }
    assert_eq!(
        transition(&rows, &selected, Key::Space, false),
        Transition::Unchanged
    );
}

#[test]
fn space_at_selected_tail_is_total_noop_preserving_raw_order_stale_ids_and_anchor() {
    let rows = ["a", "b", "c", "d", "e"];
    let selected = state(Some(0), &["e", "c", "gone", "b"], Some(4));
    for shift in [false, true] {
        assert_eq!(
            transition(&rows, &selected, Key::Space, shift),
            Transition::Unchanged
        );
    }
    assert_eq!(selected.selected_ids, ["e", "c", "gone", "b"]);
    assert_eq!(selected.active, Some(0));
    assert_eq!(selected.anchor, Some(4));
}

#[test]
fn empty_rows_leave_selection_untouched_and_reveal_requires_active_change() {
    let selected = state(Some(1), &["b", "gone"], Some(2));
    for name in ["up", "down", "home", "end", "space"] {
        for shift in [false, true] {
            assert_eq!(
                transition(&[], &selected, Key::from_name(name).unwrap(), shift),
                Transition::Unchanged
            );
        }
    }
    let rows = ["a", "b", "c"];
    // Selection and anchor can change at the same active row without scrolling.
    expect_selection(
        &rows,
        &state(Some(0), &["b", "gone"], Some(2)),
        Key::Home,
        false,
        state(Some(0), &["a"], Some(0)),
        None,
    );
    expect_selection(
        &rows,
        &state(Some(0), &[], None),
        Key::Space,
        false,
        state(Some(0), &["a"], Some(0)),
        None,
    );
    expect_selection(
        &rows,
        &state(Some(1), &["b"], Some(1)),
        Key::Down,
        false,
        state(Some(2), &["c"], Some(2)),
        Some(2),
    );
}

#[test]
fn select_all_uses_loaded_order_preserves_cursor_and_anchor_and_replaces_stale_ids() {
    let rows = ["filtered-a", "filtered-b", "filtered-c"];
    let selected = expect_selection(
        &rows,
        &state(Some(1), &["filtered-c", "old-query", "filtered-a"], Some(2)),
        Key::SelectAll,
        false,
        state(Some(1), &rows, Some(2)),
        None,
    );
    // Subsequent Shift navigation uses the existing range anchor.
    expect_selection(&rows, &selected, Key::Home, true,
        state(Some(0), &rows, Some(2)), Some(0));
    let appended = ["filtered-a", "filtered-b", "filtered-c", "next-page"];
    assert_eq!(selected.selected_ids, rows);
    expect_selection(&appended, &selected, Key::SelectAll, false,
        state(Some(1), &appended, Some(2)), None);
}

#[test]
fn select_all_handles_empty_single_and_unset_cursor_without_loading_or_revealing_rows() {
    assert_eq!(transition(&[], &SelectionState::default(), Key::SelectAll, false), Transition::Unchanged);
    expect_selection(&["one"], &SelectionState::default(), Key::SelectAll, false,
        state(Some(0), &["one"], Some(0)), Some(0));
    expect_selection(&["one", "two"], &state(Some(90), &["gone"], Some(90)),
        Key::SelectAll, false, state(Some(0), &["one", "two"], Some(0)), Some(0));
    assert_eq!(Key::from_name("SelectAll"), Some(Key::SelectAll));
    assert_eq!(Key::from_name("a"), None, "plain letters must still search");
}
