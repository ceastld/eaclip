//! Pure history-list keyboard transitions matching ClipboardPage's host and
//! window navigation. The view retains focus, IME, popup and session guards.
//! Apply each returned state before processing the next queued key.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Space,
    SelectAll,
}

impl Key {
    /// Core uses DOM names; GPUI window events use lowercase names. Enter,
    /// Delete and Tab keep their existing output/primary-route handlers.
    pub fn from_name(value: &str) -> Option<Self> {
        Some(match value {
            "ArrowLeft" | "Left" | "left" => Self::Left,
            "ArrowRight" | "Right" | "right" => Self::Right,
            "ArrowUp" | "Up" | "up" => Self::Up,
            "ArrowDown" | "Down" | "down" => Self::Down,
            "Home" | "home" => Self::Home,
            "End" | "end" => Self::End,
            " " | "Space" | "Spacebar" | "space" => Self::Space,
            "SelectAll" => Self::SelectAll,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Previous,
    Next,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionState {
    /// None corresponds to Web activeIndex=-1. This is independent of whether
    /// selected_ids is empty; it must not be inferred from selection order.
    pub active: Option<usize>,
    /// Preserve raw order and off-page IDs until the actual key changes them.
    pub selected_ids: Vec<String>,
    pub anchor: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    /// Known key, with no selection/category update. The view can still mark
    /// the key handled and reset its existing idle/hover interaction state.
    Unchanged,
    Category(Direction),
    Selection {
        state: SelectionState,
        /// Web scrolls on activeIndex change, not merely a selection change.
        reveal: Option<usize>,
    },
}

/// Tab/Shift+Tab toggle the two primary titlebar pages, rather than categories.
/// Both directions have the same destination, as in clipHostPrimaryTabs.ts.
pub fn primary_tab_route(favorites_active: bool) -> &'static str {
    if favorites_active {
        "/clipboard"
    } else {
        "/favorites"
    }
}

/// Left/Right wrap the current category. An unknown category uses index zero
/// as its base, matching Web handleArrowNavigation.
pub fn category_index(count: usize, current: Option<usize>, direction: Direction) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let current = current.filter(|index| *index < count).unwrap_or(0);
    Some(match direction {
        Direction::Previous if current == 0 => count - 1,
        Direction::Previous => current - 1,
        Direction::Next if current == count - 1 => 0,
        Direction::Next => current + 1,
    })
}

/// Only modifiers already accepted by the host/view reach here. Shift extends
/// Up/Down/Home/End; Left/Right and plain Space ignore it, as the host Web API
/// does. Space remains a keydown action and repeats extend one row at a time.
/// The separate Web list-reconcile effect is deliberately outside this API.
pub fn transition(rows: &[&str], state: &SelectionState, key: Key, shift: bool) -> Transition {
    match key {
        Key::Left => return Transition::Category(Direction::Previous),
        Key::Right => return Transition::Category(Direction::Next),
        _ => {}
    }
    let Some(last) = rows.len().checked_sub(1) else {
        return Transition::Unchanged;
    };
    if key == Key::SelectAll {
        // ClipboardPage selects its loaded, filtered items, not just rendered
        // rows or every database match. Keep the cursor/anchor for Shift ranges.
        return selection_update(state, SelectionState {
            active: Some(state.active.filter(|index| *index <= last).unwrap_or(0)),
            selected_ids: rows.iter().map(|id| (*id).to_string()).collect(),
            anchor: Some(state.anchor.filter(|index| *index <= last).unwrap_or(0)),
        });
    }
    if key == Key::Space {
        let mut first_selected = None;
        let mut last_selected = None;
        for index in state
            .selected_ids
            .iter()
            .filter_map(|id| rows.iter().position(|row| *row == id.as_str()))
        {
            first_selected = Some(first_selected.map_or(index, |first: usize| first.min(index)));
            last_selected = Some(last_selected.map_or(index, |last: usize| last.max(index)));
        }
        return match (first_selected, last_selected) {
            (Some(_), Some(end)) if end == last => Transition::Unchanged,
            (Some(start), Some(end)) => {
                let mut next = range(rows, state, start, end + 1);
                next.anchor = Some(start);
                selection_update(state, next)
            }
            _ => selection_update(state, single(rows, 0)),
        };
    }
    let base = state.active.unwrap_or(0);
    let target = match key {
        Key::Up => base.saturating_sub(1).min(last),
        Key::Down => base.saturating_add(1).min(last),
        Key::Home => 0,
        Key::End => last,
        _ => unreachable!("category and Space handled above"),
    };
    let next = if shift {
        // For Home/End the Web fallback may be -1. Since target is a valid
        // nonnegative row, clipping that range has the same lower edge as 0.
        let start = state.anchor.unwrap_or(base);
        range(rows, state, start, target)
    } else {
        single(rows, target)
    };
    selection_update(state, next)
}

fn single(rows: &[&str], target: usize) -> SelectionState {
    SelectionState {
        active: Some(target),
        selected_ids: vec![rows[target].to_string()],
        anchor: Some(target),
    }
}

fn range(rows: &[&str], state: &SelectionState, from: usize, to: usize) -> SelectionState {
    let end = from.max(to).min(rows.len() - 1);
    let start = from.min(to);
    SelectionState {
        active: Some(to),
        selected_ids: rows[start..=end]
            .iter()
            .map(|id| (*id).to_string())
            .collect(),
        anchor: state.anchor,
    }
}

fn selection_update(previous: &SelectionState, state: SelectionState) -> Transition {
    let reveal = (previous.active != state.active)
        .then_some(state.active)
        .flatten();
    Transition::Selection { state, reveal }
}

#[cfg(test)]
#[path = "history_keyboard_tests.rs"]
mod tests;
