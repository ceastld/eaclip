//! CF_HTML helpers aligned with C# `BetterClip.ClipboardFormats.ClipboardCfHtml`.

const SOURCE_URL_SCAN_MAX_UTF16: usize = 4096;

// C# caps header scans in UTF-16 code units. Keep complete Unicode characters
// when the limit lands inside a surrogate pair; UTF-8 byte offsets below remain
// relative to the original, unmodified CF_HTML payload.
fn header_scan_prefix(raw: &str, max_units: usize) -> &str {
    if raw.len() <= max_units {
        return raw;
    }
    let mut units = 0;
    for (offset, ch) in raw.char_indices() {
        units += ch.len_utf16();
        if units > max_units {
            return &raw[..offset];
        }
    }
    raw
}

/// C# `ClipboardCfHtml.LooksLikeCompleteFormat`: `TrimStart` then `Version:` ordinal-ignore-case.
pub fn looks_like_complete_format(raw: &str) -> bool {
    raw.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Version:"))
}

pub fn try_parse_source_url(raw: &str) -> Option<String> {
    let window = header_scan_prefix(raw, SOURCE_URL_SCAN_MAX_UTF16);
    let window = &window[..window.find('<').unwrap_or(window.len())];
    let idx = window.to_ascii_lowercase().find("sourceurl:")?;
    let value_start = idx + "SourceURL:".len();
    if value_start >= window.len() {
        return None;
    }
    let rest = &window[value_start..];
    let line_end = rest.find(['\r', '\n']).unwrap_or(rest.len());
    let value = rest[..line_end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn try_extract_fragment_and_source_url(raw: &str) -> Option<(String, Option<String>)> {
    if !looks_like_complete_format(raw) {
        return None;
    }
    let source_url = try_parse_source_url(raw);
    if let Some(fragment) = extract_fragment_by_markers(raw) { return Some((fragment, source_url)); }
    if let Some((start, end)) = parse_header_offsets(raw) {
        let bytes = raw.as_bytes();
        if start < end && end <= bytes.len() {
            if let Ok(fragment) = std::str::from_utf8(&bytes[start..end]) {
                return Some((strip_fragment_markers(fragment), source_url));
            }
        }
    }
    extract_fragment_by_markers(raw).map(|fragment| (fragment, source_url))
}

pub fn build_clipboard_html(html_fragment: &str, source_url: Option<&str>) -> String {
    let src_line = source_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("SourceURL:{s}\r\n"))
        .unwrap_or_default();
    let version = "Version:0.9\r\n";
    let placeholder = "StartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let header_template = format!("{version}{placeholder}{src_line}");
    let header_byte_length = header_template.len();
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    let complete = looks_like_complete_format(html_fragment);
    let mut doc = if complete { document_part(html_fragment) } else { html_fragment }.to_string();
    // Numeric-only historical selections refer to the complete payload. Keep
    // their document-relative byte range before a new header changes offsets.
    if complete && fragment_range(&doc).is_none() {
        if let Some(((start, end), document_start)) = fragment_range(html_fragment).zip(html_fragment.find('<')) {
            if let Some((start, end)) = start.checked_sub(document_start).zip(end.checked_sub(document_start)) {
                if doc.get(start..end).is_some() {
                    doc.insert_str(end, END);
                    doc.insert_str(start, START);
                }
            }
        }
    }
    if !complete && !looks_like_document(&doc) {
        doc = format!("<html><body>\r\n{START}{doc}{END}\r\n</body></html>");
    } else if fragment_range(&doc).is_none() {
        let lower = doc.to_ascii_lowercase();
        let body = lower.find("<body").and_then(|start| lower[start..].find('>').map(|offset| start + offset + 1));
        if let Some((start, end)) = body.zip(lower.rfind("</body")).filter(|(start, end)| start <= end) {
            doc.insert_str(end, END);
            doc.insert_str(start, START);
        } else { doc = format!("{START}{doc}{END}"); }
    }
    let (fragment_start, fragment_end) = fragment_range(&doc).unwrap();
    let start_html = header_byte_length;
    let end_html = start_html + doc.len();
    let start_fragment = start_html + fragment_start;
    let end_fragment = start_html + fragment_end;
    let header = format!(
        "{version}StartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n{src_line}"
    );
    format!("{header}{doc}")
}

/// Preserve full CF_HTML until a real edit. Fragment edits replace only their
/// original range, retaining WPS head/styles/namespaces and rebuilding offsets.
pub fn merge_edited_html(original: Option<&str>, incoming: &str, source_url: Option<&str>) -> String {
    if incoming.is_empty() { return String::new(); }
    let original = original.unwrap_or_default();
    if source_url.is_none() && incoming == original { return original.to_string(); }
    let original_source = try_parse_source_url(original);
    if looks_like_complete_format(incoming) {
        let source = try_parse_source_url(incoming);
        return build_clipboard_html(incoming, source_url.or(source.as_deref()));
    }
    if looks_like_complete_format(original) {
        if source_url.is_none() && try_extract_fragment_and_source_url(original).is_some_and(|(fragment, _)| fragment == incoming) {
            return original.to_string();
        }
        if !looks_like_document(incoming) {
            if let Some((start, end)) = fragment_range(original) {
                let doc_start = original.find('<').unwrap_or(0);
                if start >= doc_start {
                    let mut document = format!("{}{}{}", &original[doc_start..start], incoming, &original[end..]);
                    if !document.to_ascii_lowercase().contains("<!--startfragment-->") {
                        let at = start - doc_start;
                        document.insert_str(at + incoming.len(), "<!--EndFragment-->");
                        document.insert_str(at, "<!--StartFragment-->");
                    }
                    return build_clipboard_html(document.trim_end_matches('\0'), source_url.or(original_source.as_deref()));
                }
            }
        }
    }
    build_clipboard_html(incoming, source_url.or(original_source.as_deref()))
}

fn looks_like_document(raw: &str) -> bool {
    let start = raw.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace()).to_ascii_lowercase();
    start.starts_with("<html") || start.starts_with("<!doctype")
}

fn document_part(raw: &str) -> &str {
    raw.find('<').map_or(raw, |start| &raw[start..]).trim_end_matches('\0')
}

/// Validated UTF-8 byte bounds for a borrowed CF_HTML display projection.
pub fn fragment_range(raw: &str) -> Option<(usize, usize)> {
    let lower = raw.to_ascii_lowercase();
    if let Some(start) = lower.find("<!--startfragment-->").map(|start| start + "<!--startfragment-->".len()) {
        if let Some(end) = lower[start..].find("<!--endfragment-->") { return Some((start, start + end)); }
    }
    if !looks_like_complete_format(raw) { return None; }
    let (start, end) = parse_header_offsets(raw)?;
    (start <= end && raw.get(start..end).is_some()).then_some((start, end))
}

fn parse_header_offsets(raw: &str) -> Option<(usize, usize)> {
    let sample = header_scan_prefix(raw, 16384);
    let start = parse_int_field(sample, "StartFragment:")?;
    let end = parse_int_field(sample, "EndFragment:")?;
    Some((start, end))
}

fn parse_int_field(raw: &str, key: &str) -> Option<usize> {
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(key) {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn extract_fragment_by_markers(raw: &str) -> Option<String> {
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    let i = raw.to_ascii_lowercase().find(&START.to_ascii_lowercase())?;
    let j = raw[i + START.len()..].to_ascii_lowercase().find(&END.to_ascii_lowercase())? + i + START.len();
    if j <= i {
        return None;
    }
    Some(raw[i + START.len()..j].to_string())
}

fn strip_fragment_markers(inner: &str) -> String {
    let mut s = inner.to_string();
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    if s.to_ascii_lowercase().starts_with(&START.to_ascii_lowercase()) {
        s = s[START.len()..].to_string();
    }
    if s.to_ascii_lowercase().ends_with(&END.to_ascii_lowercase()) {
        s.truncate(s.len() - END.len());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fragment_and_source_url() {
        let raw = build_clipboard_html("<b>hello</b>", Some("https://example.com/x"));
        assert!(looks_like_complete_format(&raw));
        let (fragment, src) = try_extract_fragment_and_source_url(&raw).expect("parse");
        assert_eq!(fragment, "<b>hello</b>");
        assert_eq!(src.as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn looks_like_complete_format_ignores_version_case() {
        assert!(looks_like_complete_format("version:0.9\r\nStartHTML:0000000000"));
        assert!(looks_like_complete_format("  VERSION:0.9\r\n"));
        assert!(!looks_like_complete_format("<html><body>hi</body></html>"));
    }

    #[test]
    fn roundtrip_non_ascii_fragment_uses_utf8_byte_offsets() {
        let fragment = "<p>中文片段</p>";
        let raw = build_clipboard_html(fragment, Some("https://example.com/中文"));
        assert!(raw.contains("Version:0.9"));
        let (parsed, src) = try_extract_fragment_and_source_url(&raw).expect("parse");
        assert_eq!(parsed, fragment);
        assert_eq!(src.as_deref(), Some("https://example.com/中文"));

        let lowered = raw.replacen("Version:", "VERSION:", 1);
        let (parsed_ci, _) = try_extract_fragment_and_source_url(&lowered).expect("parse lowercase Version");
        assert_eq!(parsed_ci, fragment);
    }

    fn wps_payload() -> String {
        build_clipboard_html("<html xmlns:wps='urn:wps'><head><style>.xl1{mso-number-format:'0.00'}</style></head><body>before<!--StartFragment--> <table class='xl1'><tr><td>中文😀</td></tr></table> <!--EndFragment-->after</body></html>", Some("https://example.test/来源"))
    }

    #[test]
    fn html_preservation_unchanged_raw_and_fragment_keep_exact_context() {
        let raw = format!("\u{feff}{}\0\0", wps_payload());
        assert_eq!(merge_edited_html(Some(&raw), &raw, None), raw);
        let fragment = try_extract_fragment_and_source_url(&raw).unwrap().0;
        assert!(fragment.starts_with(" <table"));
        assert_eq!(merge_edited_html(Some(&raw), &fragment, None), raw);
    }

    #[test]
    fn html_preservation_real_fragment_edit_keeps_styles_namespaces_and_byte_offsets() {
        let original = wps_payload();
        let edited = "<table class='xl1'><tr><td>新内容🦀😀</td></tr></table>";
        let raw = merge_edited_html(Some(&original), edited, None);
        assert!(raw.contains("<html xmlns:wps='urn:wps'><head><style>.xl1{mso-number-format:'0.00'}</style></head><body>before"));
        assert!(raw.contains("after</body></html>"));
        let (start, end) = parse_header_offsets(&raw).unwrap();
        assert_eq!(&raw[start..end], edited);
        assert_eq!(try_parse_source_url(&raw).as_deref(), Some("https://example.test/来源"));
    }

    #[test]
    fn html_preservation_full_edit_repairs_stale_offsets_and_markerless_context() {
        let original = wps_payload();
        let incoming = original.replace("中文😀", "更长的表格🦀😀");
        let edited = merge_edited_html(Some(&original), &incoming, None);
        let (start, end) = parse_header_offsets(&edited).unwrap();
        assert_eq!(&edited[start..end], " <table class='xl1'><tr><td>更长的表格🦀😀</td></tr></table> ");
        let markerless = original.replace("<!--StartFragment-->", &" ".repeat("<!--StartFragment-->".len()))
            .replace("<!--EndFragment-->", &" ".repeat("<!--EndFragment-->".len()));
        let edited = merge_edited_html(Some(&markerless), "<b>新😀</b>", None);
        assert!(edited.contains("<head><style>"));
        let (start, end) = parse_header_offsets(&edited).unwrap();
        assert_eq!(&edited[start..end], "<b>新😀</b>");
    }

    #[test]
    fn html_preservation_explicit_clear_removes_rich_payload() {
        assert_eq!(merge_edited_html(Some(&wps_payload()), "", None), "");
        assert_eq!(merge_edited_html(Some(""), "", Some("https://changed.test/")), "");
    }

    fn numeric_payload() -> String {
        build_clipboard_html("<html xmlns:wps='urn:wps'><head><style>b{color:red}</style></head><body>前文😀before<!--StartFragment--><b>selected中文😀</b><!--EndFragment-->after</body></html>", Some("https://old-source.test/来源😀"))
            .replace("<!--StartFragment-->", &" ".repeat("<!--StartFragment-->".len()))
            .replace("<!--EndFragment-->", &" ".repeat("<!--EndFragment-->".len()))
    }

    #[test]
    fn html_preservation_numeric_only_source_url_edit_keeps_selected_bounds() {
        let original = numeric_payload();
        let (start, end) = parse_header_offsets(&original).unwrap();
        assert_eq!(&original[start..end], "<b>selected中文😀</b>");
        assert_eq!(merge_edited_html(Some(&original), &original, None), original);
        let rebuilt = merge_edited_html(Some(&original), &original, Some("https://new-source.test/较长来源😀"));
        let (start, end) = parse_header_offsets(&rebuilt).unwrap();
        assert_eq!(&rebuilt[start..end], "<b>selected中文😀</b>");
        assert!(rebuilt.contains("前文😀before"));
        assert!(rebuilt.contains("after</body></html>"));
        assert!(rebuilt.contains("<head><style>"));
        assert_eq!(try_parse_source_url(&rebuilt).as_deref(), Some("https://new-source.test/较长来源😀"));
    }

    #[test]
    fn html_preservation_numeric_only_complete_source_edit_with_valid_offsets_keeps_selected_bounds() {
        let original = numeric_payload();
        // Equal UTF-8 byte lengths keep incoming offsets valid; stale offsets
        // without markers cannot be inferred reliably from the document alone.
        let incoming = original.replace("selected中文😀", "modified文本😀");
        let rebuilt = merge_edited_html(Some(&original), &incoming, None);
        let (start, end) = parse_header_offsets(&rebuilt).unwrap();
        assert_eq!(&rebuilt[start..end], "<b>modified文本😀</b>");
        assert!(rebuilt.contains("前文😀before"));
        assert!(rebuilt.contains("after</body></html>"));
        assert!(rebuilt.contains("<head><style>"));
    }
}
