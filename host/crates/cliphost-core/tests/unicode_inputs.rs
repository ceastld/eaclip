use cliphost_core::{cf_html, clipboard, fa_vector, file_icon, ClipKind};

fn html_fixture_dir() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new().prefix("unicode-html-").tempdir_in(root).unwrap()
}

#[test]
fn unicode_file_icon_paths_preserve_the_complete_path() {
    for path in [r"C:\中文\记录.txt", r"D:\🦀\图片.png", "中文.ico", ".文本", "é图标.ico"] {
        assert_eq!(file_icon::try_get_shell_path(path).as_deref(), Some(path));
        assert_eq!(file_icon::try_get_shell_path(&format!(" IcOn: {path} ")).as_deref(), Some(path));
    }
    assert_eq!(file_icon::try_get_shell_path("text:🦀"), None);
}

#[test]
fn unicode_unknown_vector_icons_return_none() {
    for spec in ["🦀", "a中文", "é图标", "FA:🦀"] {
        assert!(fa_vector::try_get_vector(spec).is_none(), "{spec}");
    }
    assert!(fa_vector::try_get_vector("FA:Light_Text").is_some());
}

#[test]
fn large_unicode_html_snapshot_preserves_payload_fragment_and_source() {
    let temp = html_fixture_dir();
    let source = "https://example.test/中文/🦀";
    let mut split_source_scan = false;
    let mut split_offset_scan = false;
    for padding in 0..4 {
        let fragment = format!("<p>{}{}</p>", "x".repeat(padding), "汉字🦀é".repeat(3000));
        let raw = cf_html::build_clipboard_html(&fragment, Some(source));
        split_source_scan |= !raw.is_char_boundary(4096);
        split_offset_scan |= !raw.is_char_boundary(16384);
        let item = clipboard::item_from_snapshot(
            clipboard::ClipboardSnapshot {
                html: Some(raw.clone()),
                text: Some("网页复制内容".into()),
                ..Default::default()
            },
            temp.path(),
            "chrome.exe",
        ).expect("HTML capture");
        assert_eq!(item.kind, ClipKind::HtmlClipItem);
        let stored = item.html_data.as_deref().expect("complete CF_HTML payload");
        assert!(stored.as_bytes() == raw.as_bytes(), "complete Unicode CF_HTML must remain byte-identical");
        let (projected, projected_source) = cf_html::try_extract_fragment_and_source_url(stored)
            .expect("Unicode fragment projection");
        assert!(projected.as_bytes() == fragment.as_bytes(), "Unicode fragment projection must preserve exact bytes");
        assert_eq!(projected_source.as_deref(), Some(source));
        assert_eq!(item.html_source_url.as_deref(), Some(source));
    }
    assert!(split_source_scan && split_offset_scan, "exercise both scan boundaries");
}

#[test]
fn source_url_scan_uses_csharp_utf16_limit() {
    let source = "https://example.test/中文";
    // Beyond 4096 UTF-8 bytes, but still inside C#'s 4096 UTF-16 code units.
    let raw = format!("Comment:{}\r\nSourceURL:{source}\r\n{}", "中🦀".repeat(700), "界".repeat(5000));
    assert_eq!(cf_html::try_parse_source_url(&raw).as_deref(), Some(source));
    // Supplementary characters consume two of the C# scan units.
    let outside = format!("{}SourceURL:{source}\r\n", "🦀".repeat(2048));
    assert_eq!(cf_html::try_parse_source_url(&outside), None);
}

#[test]
fn fragment_offsets_after_unicode_metadata_match_csharp() {
    let prefix = format!("Version:0.9\r\nComment:{}\r\n", "中🦀".repeat(3000));
    let fragment = "<p>只按字节偏移提取 🦀</p>";
    let fields = "StartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let start = prefix.len() + fields.len();
    let end = start + fragment.len();
    let raw = format!("{prefix}StartFragment:{start:010}\r\nEndFragment:{end:010}\r\n{fragment}");
    // No marker fallback: fields beyond 16KB must still be found within 16K UTF-16 units.
    let (actual, source) = cf_html::try_extract_fragment_and_source_url(&raw).expect("header offsets");
    assert_eq!(actual, fragment);
    assert_eq!(source, None);
}

#[test]
fn text_capture_uses_csharp_utf16_size_limit() {
    let temp = tempfile::tempdir().unwrap();
    for text in ["中".repeat(100 * 1024), "🦀".repeat(50 * 1024)] {
        let capture = |text: String| clipboard::item_from_snapshot(
            clipboard::ClipboardSnapshot { text: Some(text), ..Default::default() },
            temp.path(), "chrome.exe",
        );
        let item = capture(text.clone()).expect("text at C# character limit");
        assert_eq!(item.kind, ClipKind::TextClipItem);
        assert_eq!(item.body_text.as_deref(), Some(text.as_str()));
        assert!(capture(format!("{text}x")).is_none(), "one UTF-16 unit over the limit");
    }
}

#[test]
fn html_capture_preserves_unicode_payload_without_legacy_html_limit() {
    let temp = html_fixture_dir();
    let capture = |html: String, text: String| clipboard::item_from_snapshot(
        clipboard::ClipboardSnapshot { html: Some(html), text: Some(text), ..Default::default() },
        temp.path(), "chrome.exe",
    ).expect("HTML item with valid plain text preserves its complete HTML");
    let text = "汉字🦀é".repeat(10000);
    let fragment = format!("<p>{text}</p>");
    let raw = cf_html::build_clipboard_html(&fragment, Some("https://example.test/中文"));
    assert!(raw.len() > 100 * 1024 && raw.encode_utf16().count() < 100 * 1024);
    let item = capture(raw.clone(), text.clone());
    assert_eq!(item.kind, ClipKind::HtmlClipItem);
    assert!(item.body_text.as_deref() == Some(text.as_str()), "associated Unicode plain text must remain exact");
    let stored = item.html_data.as_deref().expect("complete Unicode CF_HTML payload");
    assert!(stored.as_bytes() == raw.as_bytes(), "HTML storage must preserve the original CF_HTML bytes");
    let (projected, projected_source) = cf_html::try_extract_fragment_and_source_url(stored)
        .expect("Unicode HTML fragment projection");
    assert!(projected.as_bytes() == fragment.as_bytes(), "HTML fragment projection must preserve the Unicode selection");
    assert_eq!(projected_source.as_deref(), Some("https://example.test/中文"));
    assert_eq!(item.html_source_url.as_deref(), Some("https://example.test/中文"));

    let below = "中".repeat(100 * 1024 - 1);
    let above = format!("{below}中中");
    assert!(above.encode_utf16().count() > 100 * 1024);
    // The historical HTML cutoff must not discard original payloads. The
    // separate plain-text UTF-16 collection limit remains covered above.
    for html in [below, above] {
        let item = capture(html.clone(), "网页".into());
        assert!(item.html_data.as_deref().is_some_and(|stored| stored.as_bytes() == html.as_bytes()),
            "HTML on either side of the legacy cutoff must remain byte-identical");
    }
}
