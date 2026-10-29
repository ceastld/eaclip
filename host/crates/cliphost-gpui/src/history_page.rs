//! Refresh the already loaded range while respecting Core's 500-row pages.

use crate::preview::PreviewPage;
use std::collections::HashSet;

/// Include the complete presentation, so edits to a surviving ID are measured
/// again while deleting its neighbours can retain the existing measurement.
pub(crate) fn row_key(row: &crate::preview::PreviewRow) -> (String, [u8; 32]) {
    (row.item.id.clone(), *blake3::hash(&serde_json::to_vec(row).unwrap_or_default()).as_bytes())
}

pub fn query_loaded_range(
    skip: usize,
    take: usize,
    mut fetch: impl FnMut(usize, usize) -> Result<PreviewPage, String>,
) -> Result<PreviewPage, String> {
    if take <= 500 {
        return fetch(skip, take);
    }
    let mut page = fetch(skip, 500)?;
    let mut seen: HashSet<String> = page.rows.iter().map(|row| row.item.id.clone()).collect();
    let mut last_skip = skip;
    let mut budget = take.div_ceil(500);
    while page.rows.len() < take && page.has_more {
        if page.next_skip <= last_skip {
            return Err("Clipboard list paging did not advance.".into());
        }
        if budget == 0 {
            return Err("Clipboard list changed repeatedly during refresh.".into());
        }
        budget -= 1;
        last_skip = page.next_skip;
        let next = fetch(last_skip, (take - page.rows.len()).min(500))?;
        page.total = next.total;
        page.has_more = next.has_more;
        page.next_skip = next.next_skip;
        page.rows.extend(
            next.rows
                .into_iter()
                .filter(|row| seen.insert(row.item.id.clone())),
        );
    }
    page.skip = skip;
    page.take = take;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::PreviewRow;
    use cliphost_core::{dto::clip_to_dto, model::ClipRecord};

    fn page(skip: usize, take: usize, total: usize) -> PreviewPage {
        let end = skip.saturating_add(take).min(total);
        PreviewPage {
            rows: (skip..end)
                .map(|index| {
                    let mut record = ClipRecord::new_text(index.to_string(), "fixture");
                    record.id = index.to_string();
                    PreviewRow {
                        item: clip_to_dto(&record, None),
                        preview_text: record.body_text.unwrap_or_default(),
                        thumbnail: None,
                    }
                })
                .collect(),
            skip,
            take,
            total,
            next_skip: end,
            has_more: end < total,
        }
    }

    #[test]
    fn refresh_keeps_rows_after_core_page_limit() {
        let mut calls = Vec::new();
        let result = query_loaded_range(0, 1_000, |skip, take| {
            calls.push((skip, take));
            Ok(page(skip, take, 1_006))
        })
        .unwrap();
        assert_eq!(calls, [(0, 500), (500, 500)]);
        assert_eq!(result.rows.len(), 1_000);
        assert_eq!(result.rows[800].item.id, "800");
        assert_eq!(result.next_skip, 1_000);
        assert!(result.has_more);
    }

    #[test]
    fn shorter_history_and_small_pages_keep_core_semantics() {
        let result = query_loaded_range(0, 1_000, |skip, take| Ok(page(skip, take, 730))).unwrap();
        assert_eq!(result.rows.len(), 730);
        assert!(!result.has_more);
        let mut calls = 0;
        query_loaded_range(100, 100, |skip, take| {
            calls += 1;
            assert_eq!((skip, take), (100, 100));
            Ok(page(skip, take, 1_000))
        })
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn surviving_row_keys_stay_stable_but_content_and_pin_edits_change_them() {
        let original = page(0, 3, 3);
        let surviving = original.rows[1].clone();
        assert_eq!(row_key(&original.rows[1]), row_key(&surviving));
        let mut edited = surviving.clone();
        edited.preview_text = "changed content".into();
        assert_ne!(row_key(&surviving), row_key(&edited));
        edited = surviving.clone();
        edited.item.is_top = true;
        assert_ne!(row_key(&surviving), row_key(&edited));
    }

    #[test]
    fn error_or_stalled_paging_does_not_return_partial_success() {
        assert!(query_loaded_range(0, 1_000, |skip, take| {
            if skip == 0 {
                Ok(page(skip, take, 1_000))
            } else {
                Err("read failed".into())
            }
        })
        .is_err());
        let result = query_loaded_range(0, 1_000, |skip, take| {
            let mut page = page(skip, take, 1_000);
            page.next_skip = skip;
            Ok(page)
        });
        assert!(result.unwrap_err().contains("did not advance"));
    }
}
