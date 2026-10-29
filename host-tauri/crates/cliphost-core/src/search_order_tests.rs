use super::*;

#[test]
fn search_order_keeps_relevance_before_pin_and_stable_time_ties() {
    let base = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
    let make = |id: &str, title: &str, body: &str, pinned, seconds, sub_index| {
        let mut item = ClipRecord::new_text(body, "search-order");
        item.id = id.into();
        item.title = Some(title.into());
        item.is_top = pinned;
        item.create_time = base + chrono::Duration::seconds(seconds);
        item.sub_index = sub_index;
        item
    };
    let mut older = make("older-title", "needle", "plain", false, 0, 0);
    older.last_access = Some(base + chrono::Duration::seconds(5));
    let original = vec![
        make("pinned-body", "plain", "needle", true, 10, 0),
        older,
        make("later-subindex", "needle", "plain", false, 2, 2),
        make("tie-first", "needle", "plain", false, 2, 0),
        make("tie-second", "needle", "plain", false, 2, 0),
        make("pinned-title", "needle", "plain", true, -20, 0),
    ];
    for (query, sort, expected) in [
        (
            "needle",
            0,
            [
                "pinned-title",
                "tie-first",
                "tie-second",
                "later-subindex",
                "older-title",
                "pinned-body",
            ],
        ),
        (
            "needle",
            1,
            [
                "pinned-title",
                "older-title",
                "tie-first",
                "tie-second",
                "later-subindex",
                "pinned-body",
            ],
        ),
        (
            "",
            0,
            [
                "pinned-body",
                "pinned-title",
                "tie-first",
                "tie-second",
                "later-subindex",
                "older-title",
            ],
        ),
        (
            "",
            1,
            [
                "pinned-body",
                "pinned-title",
                "older-title",
                "tie-first",
                "tie-second",
                "later-subindex",
            ],
        ),
    ] {
        let mut items = original.clone();
        sort_clips(&mut items, Some(sort), query);
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            expected,
            "query={query:?}, sort={sort}"
        );
    }
}
