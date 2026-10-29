//! Keep measured rows across updates to the same list presentation.
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    ops::Range,
};

use gpui::ListState;

/// A reload or malformed event cannot prove that a same-path resource stayed
/// unchanged. Callers aggregate these IDs and invalidate only confirmed deltas.
pub(crate) fn invalidation_ids(data: &serde_json::Value, upserts: &str) -> Option<HashSet<String>> {
    if data["op"].as_str()? != "delta" {
        return None;
    }
    let mut ids = HashSet::new();
    for value in data
        .get(upserts)?
        .as_array()?
        .iter()
        .chain(data.get("ids")?.as_array()?)
    {
        let id = value.as_str().or_else(|| value.get("id")?.as_str())?;
        ids.insert(id.to_owned());
    }
    Some(ids)
}

/// Keys include stable occurrence identity and everything affecting row layout.
/// Splices run backwards so each range still addresses the previous snapshot.
pub(crate) fn reconcile<K: Eq + Hash>(list: &ListState, old: &[K], new: &[K]) -> bool {
    if list.item_count() != old.len() {
        list.reset(new.len());
        return true;
    }
    let edits = edits(old, new);
    for (range, count) in edits.iter().rev() {
        list.splice(range.clone(), *count);
    }
    !edits.is_empty()
}

fn edits<K: Eq + Hash>(old: &[K], new: &[K]) -> Vec<(Range<usize>, usize)> {
    if old == new {
        return Vec::new();
    }
    let positions: HashMap<_, _> = old.iter().enumerate().map(|(i, key)| (key, i)).collect();
    let unique: HashSet<_> = new.iter().collect();
    if positions.len() != old.len() || unique.len() != new.len() {
        // Ambiguous keys must never attach a cached measurement to another row.
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        return vec![(prefix..old.len() - suffix, new.len() - prefix - suffix)];
    }
    // The longest increasing sequence of old positions is the largest set of
    // unchanged rows whose measurements can stay in their existing SumTree.
    let common: Vec<_> = new
        .iter()
        .enumerate()
        .filter_map(|(new_index, key)| positions.get(key).map(|old_index| (*old_index, new_index)))
        .collect();
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![None; common.len()];
    for (index, &(old_index, _)) in common.iter().enumerate() {
        let slot = tails.partition_point(|&at| common[at].0 < old_index);
        if slot > 0 {
            previous[index] = Some(tails[slot - 1]);
        }
        if slot == tails.len() {
            tails.push(index);
        } else {
            tails[slot] = index;
        }
    }
    let mut anchors = Vec::new();
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        anchors.push(common[index]);
        cursor = previous[index];
    }
    anchors.reverse();
    anchors.push((old.len(), new.len()));
    let mut edits = Vec::new();
    let (mut old_start, mut new_start) = (0, 0);
    for (old_end, new_end) in anchors {
        if old_start != old_end || new_start != new_end {
            edits.push((old_start..old_end, new_end - new_start));
        }
        old_start = old_end + 1;
        new_start = new_end + 1;
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, ListAlignment, ListOffset};

    #[test]
    fn resource_changes_distinguish_exact_deletions_edits_and_unknown_reload() {
        let ids = invalidation_ids(
            &serde_json::json!({"op":"delta","items":[{"id":"edited"}],"ids":["deleted"]}),
            "items",
        )
        .unwrap();
        assert_eq!(ids, HashSet::from(["edited".into(), "deleted".into()]));
        assert!(invalidation_ids(&serde_json::json!({"op":"reload"}), "items").is_none());
        assert!(invalidation_ids(
            &serde_json::json!({"op":"delta","items":null,"ids":[]}),
            "items"
        )
        .is_none());
        assert_eq!(
            invalidation_ids(
                &serde_json::json!({"op":"delta","upsertIds":[],"ids":[]}),
                "upsertIds"
            ),
            Some(HashSet::new())
        );
    }

    #[test]
    fn separated_deletions_keep_intervening_rows_and_the_reading_offset() {
        let old = ["a", "b", "c", "d", "e"];
        let new = ["b", "d", "e"];
        assert_eq!(edits(&old, &new), vec![(0..1, 0), (2..3, 0)]);
        let list = ListState::new(old.len(), ListAlignment::Top, px(100.));
        list.scroll_to(ListOffset {
            item_ix: 3,
            offset_in_item: px(17.),
        });
        assert!(reconcile(&list, &old, &new));
        assert_eq!(list.item_count(), 3);
        assert_eq!(list.logical_scroll_top().item_ix, 1);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(17.));
        assert!(!reconcile(&list, &new, &new));
    }

    #[test]
    fn inserted_and_changed_rows_do_not_invalidate_unrelated_rows() {
        assert_eq!(
            edits(&["a:v1", "b", "c"], &["x", "a:v2", "b", "c", "z"]),
            vec![(0..1, 2), (3..3, 1)]
        );
    }

    #[test]
    fn reorders_and_empty_pages_produce_the_exact_new_sequence() {
        for (old, new) in [
            (vec!["a", "b", "c"], vec!["c", "a", "b"]),
            (vec!["a", "a", "b"], vec!["a", "b", "b"]),
            (vec![], vec!["a"]),
            (vec!["a"], vec![]),
        ] {
            let mut actual = old.clone();
            // Insert placeholders carrying the expected keys at the new gap.
            let changes = edits(&old, &new);
            for (range, count) in changes.iter().rev() {
                actual.splice(range.clone(), std::iter::repeat("?").take(*count));
            }
            assert_eq!(actual.len(), new.len());
            for (actual, expected) in actual.iter().zip(&new) {
                assert!(*actual == "?" || actual == expected);
            }
        }
    }
}
