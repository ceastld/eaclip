//! Real store captures use the same policy as monitoring; no user data or host.
use crate::{model::{ClipKind, ClipRecord}, store::AppStore};

fn html(payload: &str, url: &str) -> ClipRecord {
    let mut item = ClipRecord::new_text("same table\r\n", "wps.exe");
    item.kind = ClipKind::HtmlClipItem;
    item.html_data = Some(payload.into());
    item.html_source_url = Some(url.into());
    item
}

fn full_html() -> String {
    crate::cf_html::build_clipboard_html("<html xmlns:wps='urn:wps'><head><style>td{color:red}</style></head><body><table><tr><td>same table</td></tr></table></body></html>", None)
}

#[test]
fn capture_dedupe_store_plain_fragment_full_style_and_url_all_keep_their_payloads() {
    let root = tempfile::tempdir().unwrap();
    let store = AppStore::open(&root.path().join("userdata.db")).unwrap();
    let plain = ClipRecord::new_text("same table", "owned-fixture.exe");
    let full = full_html();
    let blue = crate::cf_html::merge_edited_html(Some(&full), &full.replace("color:red", "color:blue"), None);
    let rows = [plain, html("<table><tr><td>same table</td></tr></table>", "https://source.test/"),
        html(&full, "https://source.test/"), html(&blue, "https://source.test/"),
        html(&full, "https://different-source.test/")];
    for row in &rows {
        let saved = store.persist_capture(row, false, |_| false).unwrap();
        assert_eq!(saved.id, row.id, "A different rich capture was merged into an older row");
        assert_eq!(store.get_clip(&row.id).unwrap().html_data, row.html_data);
    }
    assert_eq!(store.list_clips().unwrap().len(), rows.len());
    for (index, a) in rows.iter().enumerate() {
        for b in rows.iter().skip(index + 1) {
            assert_ne!(a.distinct_key(), b.distinct_key(), "Manual dedupe could discard different payloads");
        }
    }
}

#[test]
fn capture_dedupe_store_identical_complete_html_reuses_id_without_overwriting_user_metadata() {
    let root = tempfile::tempdir().unwrap();
    let store = AppStore::open(&root.path().join("userdata.db")).unwrap();
    let mut first = html(&full_html(), "https://source.test/");
    first.title = Some("user title".into());
    first.is_top = true;
    store.persist_capture(&first, false, |_| false).unwrap();
    let mut repeated = html(&full_html(), "https://source.test/");
    repeated.body_text = Some("same table".into());
    repeated.title = Some("capture title".into());
    let saved = store.persist_capture(&repeated, false, |_| false).unwrap();
    assert_eq!(saved.id, first.id);
    assert_eq!(saved.title.as_deref(), Some("user title"));
    assert!(saved.is_top);
    assert_eq!(saved.html_data, first.html_data);
    assert_eq!(store.list_clips().unwrap().len(), 1);
    assert_eq!(first.distinct_key(), repeated.distinct_key());
}

#[test]
fn capture_dedupe_store_new_copy_does_not_replace_user_edited_html() {
    let root = tempfile::tempdir().unwrap();
    let store = AppStore::open(&root.path().join("userdata.db")).unwrap();
    let original = full_html();
    let edited_html = crate::cf_html::merge_edited_html(Some(&original), &original.replace("color:red", "color:green"), None);
    let edited = html(&edited_html, "https://source.test/");
    store.persist_capture(&edited, false, |_| false).unwrap();
    let captured = html(&original, "https://source.test/");
    let saved = store.persist_capture(&captured, false, |_| false).unwrap();
    assert_eq!(saved.id, captured.id);
    assert_ne!(saved.id, edited.id);
    assert_eq!(store.get_clip(&edited.id).unwrap().html_data, edited.html_data);
    assert_eq!(store.list_clips().unwrap().len(), 2);
}

#[test]
fn capture_dedupe_store_qq_content_and_unknown_rich_payloads_are_not_assumed_equal() {
    let root = tempfile::tempdir().unwrap();
    let store = AppStore::open(&root.path().join("userdata.db")).unwrap();
    let full = full_html();
    let base = html(&full, "https://source.test/");
    let mut qq_a = html(&full, "https://source.test/");
    qq_a.kind = ClipKind::QQRichTextItem;
    qq_a.qq_rich_text = Some("<QQRichEditFormat>first</QQRichEditFormat>".into());
    let mut qq_b = html(&full, "https://source.test/");
    qq_b.kind = ClipKind::QQRichTextItem;
    qq_b.qq_rich_text = Some("<QQRichEditFormat>second</QQRichEditFormat>".into());
    let unknown_a = html("", "");
    let unknown_b = html("", "");
    for row in [&base, &qq_a, &qq_b, &unknown_a, &unknown_b] {
        assert_eq!(store.persist_capture(row, false, |_| false).unwrap().id, row.id);
    }
    let mut repeat = qq_a.clone();
    repeat.id = uuid::Uuid::new_v4().simple().to_string();
    assert_eq!(store.persist_capture(&repeat, false, |_| false).unwrap().id, qq_a.id);
    assert_ne!(unknown_a.distinct_key(), unknown_b.distinct_key());
    assert_eq!(store.list_clips().unwrap().len(), 5);
}
