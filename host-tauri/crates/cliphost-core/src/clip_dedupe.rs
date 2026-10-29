//! C# `ClipCaptureDedupePolicy` + `DiscardDuplicateCaptureArtifact`.

use crate::model::{ClipKind, ClipRecord};

/// C# `ClipCaptureDedupePolicy.RecentDuplicateScanCount`.
pub const RECENT_DUPLICATE_SCAN_COUNT: usize = 20;

pub fn find_merge_target<'a>(
    recent_items_by_time: &'a [ClipRecord],
    incoming: &ClipRecord,
    allow_duplicate_copy: bool,
    is_deferred_clear_hidden: impl Fn(&str) -> bool,
) -> Option<&'a ClipRecord> {
    if allow_duplicate_copy {
        return None;
    }
    recent_items_by_time
        .iter()
        .take(RECENT_DUPLICATE_SCAN_COUNT)
        .filter(|existing| !is_deferred_clear_hidden(&existing.id))
        .find(|existing| existing.looks_equal(incoming))
}

/// C# `ClipDataService.DiscardDuplicateCaptureArtifact` (temp PNG from a merged capture).
pub fn discard_duplicate_capture_artifact(incoming: &ClipRecord, merged: &ClipRecord) {
    if incoming.kind != ClipKind::ImageClipItem {
        return;
    }
    let Some(path) = incoming.image_path.as_deref().map(str::trim).filter(|p| !p.is_empty()) else {
        return;
    };
    if merged
        .image_path
        .as_deref()
        .is_some_and(|kept| kept.eq_ignore_ascii_case(path))
    {
        return;
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClipRecord;

    fn text(id: &str, body: &str) -> ClipRecord {
        let mut rec = ClipRecord::new_text(body, "t");
        rec.id = id.into();
        rec
    }

    #[test]
    fn skips_deferred_clear_hidden_items() {
        let hidden = text("hidden", "same-text");
        let visible = text("visible", "other");
        let incoming = ClipRecord::new_text("same-text", "t");
        let recents = [hidden, visible];
        let merge = find_merge_target(&recents, &incoming, false, |id| id == "hidden")
            .map(|m| m.id.clone());
        assert!(merge.is_none());
    }

    #[test]
    fn merges_visible_duplicate() {
        let existing = text("row", "dup");
        let incoming = ClipRecord::new_text("dup", "t");
        let recents = [existing];
        let merge = find_merge_target(&recents, &incoming, false, |_| false).map(|m| m.id.clone());
        assert_eq!(merge.as_deref(), Some("row"));
    }

    #[test]
    fn allow_duplicate_copy_returns_none() {
        let existing = text("row", "dup");
        let incoming = ClipRecord::new_text("dup", "t");
        let recents = [existing];
        let merge = find_merge_target(&recents, &incoming, true, |_| false);
        assert!(merge.is_none());
    }

    #[test]
    fn prefers_first_visible_match_not_hidden_duplicate() {
        let hidden = text("hidden", "text");
        let visible = text("visible", "text");
        let incoming = ClipRecord::new_text("text", "t");
        let recents = [hidden, visible];
        let merge = find_merge_target(&recents, &incoming, false, |id| id == "hidden")
            .map(|m| m.id.clone());
        assert_eq!(merge.as_deref(), Some("visible"));
    }

    #[test]
    fn text_normalize_keeps_html_as_a_distinct_payload() {
        let existing = text("row", "Hello\r\n");
        let mut incoming = ClipRecord::new_text("Hello", "chrome");
        incoming.kind = ClipKind::HtmlClipItem;
        incoming.html_data = Some("<p>Hello</p>".into());
        let recents = [existing];
        let merge = find_merge_target(&recents, &incoming, false, |_| false).map(|m| m.id.clone());
        assert!(merge.is_none());
    }
}

#[cfg(test)]
#[path = "clip_dedupe_rich_tests.rs"]
mod rich_tests;
