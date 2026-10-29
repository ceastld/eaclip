//! Apply confirmed deletions without waiting for a complete history reload.

use std::collections::{HashSet, VecDeque};

use crate::preview::PreviewPage;

const REMEMBERED_REMOVALS: usize = 512;

/// A local success and its later Core delta describe the same removal. Keep a
/// bounded set of exact IDs so both can share one immediate page update.
#[derive(Default)]
pub(crate) struct RemovalTracker {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl RemovalTracker {
    pub(crate) fn confirm(
        &mut self,
        ids: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> HashSet<String> {
        let mut first = HashSet::new();
        for id in ids {
            let id = id.as_ref();
            if !self.seen.insert(id.to_owned()) {
                continue;
            }
            first.insert(id.to_owned());
            self.order.push_back(id.to_owned());
            if self.order.len() > REMEMBERED_REMOVALS {
                if let Some(oldest) = self.order.pop_front() {
                    self.seen.remove(&oldest);
                }
            }
        }
        first
    }

    /// An upsert starts a new lifetime for this exact ID. Remove its queue
    /// entry too, so evicting the old lifetime cannot forget a newer removal.
    pub(crate) fn forget(&mut self, ids: impl IntoIterator<Item = impl AsRef<str>>) {
        let forgotten: HashSet<String> = ids.into_iter().map(|id| id.as_ref().to_owned()).collect();
        self.seen.retain(|id| !forgotten.contains(id));
        self.order.retain(|id| !forgotten.contains(id));
    }

    pub(crate) fn clear(&mut self) {
        self.order.clear();
        self.seen.clear();
    }
}

/// Only a nonempty, deletion-only delta proves which rows can disappear now.
/// Mixed upserts, reloads and malformed payloads require authoritative refresh.
pub(crate) fn pure_delete_ids(data: &serde_json::Value) -> Option<HashSet<String>> {
    if data.get("op")?.as_str()? != "delta" || !data.get("items")?.as_array()?.is_empty() {
        return None;
    }
    let values = data.get("ids")?.as_array()?;
    if values.is_empty() {
        return None;
    }
    values
        .iter()
        .map(|value| {
            let id = value.as_str()?;
            (!id.is_empty()).then(|| id.to_owned())
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemovalPlan {
    Refill { target: usize },
    Reload,
}

/// Plan against the page before removing rows. The caller must prove that its
/// loaded prefix is current and no range read can have raced the accepted write.
/// A prior refill target survives cancellation by another deletion in the batch.
pub(crate) fn plan_removal(
    page: &PreviewPage,
    ids: &HashSet<String>,
    range_is_current: bool,
    previous_target: Option<usize>,
) -> RemovalPlan {
    if !range_is_current || page.skip != 0 {
        return RemovalPlan::Reload;
    }
    let present: HashSet<&str> = page.rows.iter().map(|row| row.item.id.as_str()).collect();
    if ids.iter().all(|id| present.contains(id.as_str())) {
        RemovalPlan::Refill {
            target: previous_target.unwrap_or(page.rows.len()),
        }
    } else {
        RemovalPlan::Reload
    }
}

/// Unknown IDs do not change loaded-range counts. The background query still
/// owns filling the gap and reconciling totals outside this loaded range.
pub(crate) fn remove_from_page(page: &mut PreviewPage, ids: &HashSet<String>) -> usize {
    let before = page.rows.len();
    page.rows.retain(|row| !ids.contains(&row.item.id));
    let removed = before - page.rows.len();
    page.total = page.total.saturating_sub(removed);
    page.next_skip = page.next_skip.saturating_sub(removed);
    page.has_more = page.next_skip < page.total;
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::PreviewRow;
    use cliphost_core::{dto::clip_to_dto, model::ClipRecord};
    use serde_json::json;

    fn ids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn page(values: &[&str], skip: usize, total: usize) -> PreviewPage {
        PreviewPage {
            rows: values
                .iter()
                .map(|id| {
                    let mut record = ClipRecord::new_text((*id).to_owned(), "fixture");
                    record.id = (*id).to_owned();
                    PreviewRow {
                        item: clip_to_dto(&record, None),
                        preview_text: String::new(),
                        thumbnail: None,
                    }
                })
                .collect(),
            total,
            skip,
            take: 100,
            next_skip: skip + values.len(),
            has_more: skip + values.len() < total,
        }
    }

    fn dataset(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("row-{index}")).collect()
    }

    fn initial_page(live: &[String], take: usize) -> PreviewPage {
        page(
            &live
                .iter()
                .take(take)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            0,
            live.len(),
        )
    }

    /// Consume the production plan using an independent ordered data source.
    /// The recorded requests also ensure a local deletion does not reload the prefix.
    fn fetch_refill(
        page_to_fill: &mut PreviewPage,
        live: &[String],
        plan: RemovalPlan,
    ) -> Vec<(usize, usize)> {
        let RemovalPlan::Refill { target } = plan else {
            panic!("expected a local refill plan");
        };
        let take = target
            .min(page_to_fill.total)
            .saturating_sub(page_to_fill.rows.len());
        if take == 0 {
            return Vec::new();
        }
        let skip = page_to_fill.next_skip;
        let values: Vec<&str> = live
            .iter()
            .skip(skip)
            .take(take)
            .map(String::as_str)
            .collect();
        let response = page(&values, skip, live.len());
        page_to_fill.rows.extend(response.rows);
        page_to_fill.total = response.total;
        page_to_fill.next_skip = response.next_skip;
        page_to_fill.has_more = response.has_more;
        vec![(skip, take)]
    }

    fn assert_prefix(page: &PreviewPage, live: &[String], take: usize) {
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.item.id.as_str())
                .collect::<Vec<_>>(),
            live.iter()
                .take(take)
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        assert_eq!(page.total, live.len());
        assert_eq!(page.next_skip, page.rows.len());
        assert_eq!(page.has_more, page.rows.len() < live.len());
    }

    #[test]
    fn local_middle_deletion_fetches_only_one_missing_row() {
        let mut live = dataset(1_000);
        let mut loaded = initial_page(&live, 100);
        let removed = ids(&["row-10"]);
        let plan = plan_removal(&loaded, &removed, true, None);
        live.retain(|id| !removed.contains(id));
        remove_from_page(&mut loaded, &removed);
        assert_eq!(fetch_refill(&mut loaded, &live, plan), [(99, 1)]);
        assert_prefix(&loaded, &live, 100);
    }

    #[test]
    fn consecutive_deletions_keep_the_cancelled_refill_target() {
        let mut live = dataset(1_000);
        let mut loaded = initial_page(&live, 100);
        let first = ids(&["row-10"]);
        let RemovalPlan::Refill { target } = plan_removal(&loaded, &first, true, None) else {
            panic!("current prefix should support local removal");
        };
        live.retain(|id| !first.contains(id));
        remove_from_page(&mut loaded, &first);
        // The first refill has not returned when the next accepted delete retires it.
        let second = ids(&["row-30"]);
        let plan = plan_removal(&loaded, &second, true, Some(target));
        live.retain(|id| !second.contains(id));
        remove_from_page(&mut loaded, &second);
        assert_eq!(fetch_refill(&mut loaded, &live, plan), [(98, 2)]);
        assert_prefix(&loaded, &live, 100);
    }

    #[test]
    fn deleting_from_the_complete_last_page_needs_no_fetch() {
        let mut live = dataset(10);
        let mut loaded = initial_page(&live, 100);
        let removed = ids(&["row-3", "row-8"]);
        let plan = plan_removal(&loaded, &removed, true, None);
        live.retain(|id| !removed.contains(id));
        remove_from_page(&mut loaded, &removed);
        assert!(fetch_refill(&mut loaded, &live, plan).is_empty());
        assert_prefix(&loaded, &live, 100);
    }

    #[test]
    fn unproven_ranges_and_unknown_ids_require_a_reload() {
        let live = dataset(1_000);
        let mut loaded = initial_page(&live, 100);
        let known = ids(&["row-10"]);
        assert_eq!(
            plan_removal(&loaded, &known, false, Some(100)),
            RemovalPlan::Reload
        );
        assert_eq!(
            plan_removal(&loaded, &ids(&["row-10", "row-200"]), true, None),
            RemovalPlan::Reload
        );
        loaded.skip = 10;
        assert_eq!(
            plan_removal(&loaded, &known, true, None),
            RemovalPlan::Reload
        );
    }

    #[test]
    fn exact_noncontiguous_removals_count_loaded_rows_once() {
        let mut page = page(&["a", "b", "c", "b", "d"], 5, 12);
        assert_eq!(
            remove_from_page(&mut page, &ids(&["b", "d", "b", "missing"])),
            3
        );
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.item.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(
            (
                page.total,
                page.next_skip,
                page.skip,
                page.take,
                page.has_more
            ),
            (9, 7, 5, 100, true)
        );
        assert_eq!(remove_from_page(&mut page, &ids(&["b", "missing", "A"])), 0);
        assert_eq!((page.total, page.next_skip), (9, 7));
    }

    #[test]
    fn last_page_empty_page_and_stale_counts_are_safe() {
        let mut last = page(&["a", "b"], 5, 7);
        assert_eq!(remove_from_page(&mut last, &ids(&["a", "b"])), 2);
        assert!(last.rows.is_empty());
        assert_eq!((last.total, last.next_skip, last.has_more), (5, 5, false));
        assert_eq!(remove_from_page(&mut last, &ids(&["missing"])), 0);
        let mut stale = page(&["a", "b"], 0, 1);
        stale.next_skip = 1;
        remove_from_page(&mut stale, &ids(&["a", "b"]));
        assert_eq!(
            (stale.total, stale.next_skip, stale.has_more),
            (0, 0, false)
        );
    }

    #[test]
    fn tracker_merges_replies_but_allows_recreated_ids_and_reload() {
        let mut tracker = RemovalTracker::default();
        assert_eq!(tracker.confirm(["a", "b", "a"]), ids(&["a", "b"]));
        assert!(tracker.confirm(["a", "b"]).is_empty());
        tracker.forget(["a"]);
        assert_eq!(tracker.confirm(["a", "b"]), ids(&["a"]));
        assert_eq!(tracker.order.len(), 2);
        tracker.clear();
        assert!(tracker.order.is_empty() && tracker.seen.is_empty());
        assert_eq!(tracker.confirm(["a", "b"]), ids(&["a", "b"]));
    }

    #[test]
    fn tracker_evicts_oldest_ids_at_its_bound() {
        let mut tracker = RemovalTracker::default();
        tracker.confirm((0..REMEMBERED_REMOVALS).map(|index| index.to_string()));
        tracker.forget(["0"]);
        tracker.confirm(["0", "new"]);
        assert_eq!(tracker.order.len(), REMEMBERED_REMOVALS);
        assert_eq!(tracker.seen.len(), REMEMBERED_REMOVALS);
        assert!(!tracker.seen.contains("1"));
        assert!(tracker.confirm(["0"]).is_empty());
        assert_eq!(tracker.confirm(["1"]), ids(&["1"]));
    }

    #[test]
    fn parser_accepts_only_well_formed_pure_deletions() {
        assert_eq!(
            pure_delete_ids(&json!({"op":"delta","items":[],"ids":["a","b","a"]})),
            Some(ids(&["a", "b"]))
        );
        for invalid in [
            json!({"op":"reload","items":[],"ids":["a"]}),
            json!({"op":"delta","items":[{"id":"b"}],"ids":["a"]}),
            json!({"op":"delta","items":[],"ids":[]}),
            json!({"op":"delta","items":[],"ids":[""]}),
            json!({"op":"delta","items":[],"ids":["a",1]}),
            json!({"op":"delta","items":[],"ids":"a"}),
            json!({"op":"delta","items":null,"ids":["a"]}),
            json!({"op":"delta","ids":["a"]}),
            json!(null),
        ] {
            assert_eq!(pure_delete_ids(&invalid), None, "{invalid}");
        }
    }
}
