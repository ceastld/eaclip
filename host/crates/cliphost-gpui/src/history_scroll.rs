//! Keep lazy-list navigation and refresh independent of measured row heights.

use gpui::{px, ListOffset, ListState};

/// The GPUI reveal calculation uses measured heights. An unmeasured target
/// needs an index anchor first so it is actually laid out and painted.
pub fn reveal(list: &ListState, index: usize) {
    if index >= list.item_count() {
        return;
    }
    if list.bounds_for_item(index).is_some() {
        list.scroll_to_reveal_item(index);
    } else {
        list.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: px(0.0),
        });
    }
}

/// Invalidating measurements must not discard the reader's current position.
pub fn remeasure(list: &ListState) {
    let anchor = list.logical_scroll_top();
    list.reset(list.item_count());
    list.scroll_to(anchor);
}

/// For the same query, retain the visible row by ID even when new rows arrive
/// above it. Readers already at the top continue seeing the newest top row.
pub fn refreshed_anchor(old: &[&str], new: &[&str], anchor: ListOffset) -> ListOffset {
    let top = ListOffset {
        item_ix: 0,
        offset_in_item: px(0.0),
    };
    if new.is_empty() || (anchor.item_ix == 0 && anchor.offset_in_item <= px(0.0)) {
        return top;
    }
    let start = anchor.item_ix.min(old.len());
    if let Some(index) = old
        .get(start)
        .and_then(|id| new.iter().position(|current| current == id))
    {
        return ListOffset {
            item_ix: index,
            offset_in_item: anchor.offset_in_item,
        };
    }
    // If the anchor was deleted, prefer the next surviving row, then the
    // preceding one, rather than jumping to the first page.
    let survivor = old[start..]
        .iter()
        .chain(old[..start].iter().rev())
        .find_map(|id| new.iter().position(|current| current == id));
    ListOffset {
        item_ix: survivor.unwrap_or_else(|| anchor.item_ix.min(new.len() - 1)),
        offset_in_item: px(0.0),
    }
}

/// When the active row disappears, continue at its next surviving neighbour,
/// falling back to the preceding row at the end of the list.
pub(crate) fn refreshed_selection(old: &[&str], new: &[&str], selected: &str) -> Option<usize> {
    if new.is_empty() { return None; }
    if let Some(index) = new.iter().position(|id| *id == selected) { return Some(index); }
    let start = old.iter().position(|id| *id == selected).unwrap_or(0);
    old[start..].iter().chain(old[..start].iter().rev())
        .find_map(|id| new.iter().position(|current| current == id))
        .or(Some(start.min(new.len() - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ListAlignment;

    #[test]
    fn deleting_active_rows_keeps_selection_near_the_old_position() {
        let old = ["a", "b", "c", "d", "e"];
        assert_eq!(refreshed_selection(&old, &["a", "d", "e"], "b"), Some(1));
        assert_eq!(refreshed_selection(&old, &["a", "b", "c"], "e"), Some(2));
        assert_eq!(refreshed_selection(&old, &["a", "b", "c"], "b"), Some(1));
        assert_eq!(refreshed_selection(&old, &[], "b"), None);
    }

    #[test]
    fn lazy_end_navigation_targets_an_unmeasured_row() {
        let list = ListState::new(100, ListAlignment::Top, px(150.0));
        list.scroll_to_reveal_item(80);
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        reveal(&list, 80);
        assert_eq!(list.logical_scroll_top().item_ix, 80);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(0.0));
        reveal(&list, 100);
        assert_eq!(list.logical_scroll_top().item_ix, 80);
    }

    #[test]
    fn appearance_measurement_reset_retains_a_real_gpui_offset() {
        let list = ListState::new(100, ListAlignment::Top, px(150.0));
        let anchor = ListOffset {
            item_ix: 70,
            offset_in_item: px(13.0),
        };
        list.scroll_to(anchor.clone());
        remeasure(&list);
        assert_eq!(list.logical_scroll_top().item_ix, anchor.item_ix);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            anchor.offset_in_item
        );
    }

    #[test]
    fn inserted_rows_keep_the_reading_row_but_top_stays_top() {
        let old = ["a", "b", "c"];
        let new = ["new", "a", "b", "c"];
        let moved = refreshed_anchor(
            &old,
            &new,
            ListOffset {
                item_ix: 1,
                offset_in_item: px(7.0),
            },
        );
        assert_eq!(moved.item_ix, 2);
        assert_eq!(moved.offset_in_item, px(7.0));
        let top = refreshed_anchor(
            &old,
            &new,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            },
        );
        assert_eq!(top.item_ix, 0);
    }

    #[test]
    fn deleted_anchor_uses_a_surviving_neighbor_and_empty_is_safe() {
        let old = ["a", "b", "c"];
        assert_eq!(
            refreshed_anchor(
                &old,
                &["a", "c"],
                ListOffset {
                    item_ix: 1,
                    offset_in_item: px(9.0)
                }
            )
            .item_ix,
            1
        );
        assert_eq!(
            refreshed_anchor(
                &old,
                &["a"],
                ListOffset {
                    item_ix: 2,
                    offset_in_item: px(9.0)
                }
            )
            .item_ix,
            0
        );
        assert_eq!(
            refreshed_anchor(
                &old,
                &[],
                ListOffset {
                    item_ix: 2,
                    offset_in_item: px(9.0)
                }
            )
            .item_ix,
            0
        );
    }
}
