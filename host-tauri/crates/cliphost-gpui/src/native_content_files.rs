//! Read-only file chips and first-line local-path candidates for history rows.
//! These are display projections; existence and file metadata belong to the host.

use cliphost_core::dto::{ClipboardItemDto, HighlightHitRangeDto};

use super::model::{ContentMode, TextProjection};

pub const FILE_CHIP_PREVIEW_LIMIT: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntryProjection {
    /// Index in the original DTO, before whitespace-only paths are filtered.
    pub original_index: usize,
    pub path: String,
    pub name: String,
    pub is_image: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListProjection {
    pub entries: Vec<FileEntryProjection>,
    /// Nonempty trimmed DTO paths, including those beyond the preview limit.
    pub total_entries: usize,
    pub fallback: TextProjection,
    pub mode: ContentMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalPathProjection {
    pub path: String,
    pub is_image: bool,
    pub text: TextProjection,
}

pub fn project_history_files(
    item: &ClipboardItemDto,
    mode: ContentMode,
    text: &str,
    hits: &[HighlightHitRangeDto],
) -> Option<FileListProjection> {
    if item.kind != "FileClipItem" {
        return None;
    }
    Some(project_file_paths(&item.file_paths, mode, text, hits))
}

/// Shared display-only file projection. Preserve the caller's original array
/// indices, including holes, for host actions and image-hover bindings.
pub fn project_file_paths(
    paths: &[String],
    mode: ContentMode,
    text: &str,
    hits: &[HighlightHitRangeDto],
) -> FileListProjection {
    let mut entries = Vec::with_capacity(paths.len().min(FILE_CHIP_PREVIEW_LIMIT));
    let mut total_entries = 0;
    for (original_index, raw) in paths.iter().enumerate() {
        let path = js_trim(raw);
        if path.is_empty() {
            continue;
        }
        total_entries += 1;
        if entries.len() < FILE_CHIP_PREVIEW_LIMIT {
            entries.push(FileEntryProjection {
                original_index,
                path: path.to_owned(),
                name: file_name(path).to_owned(),
                is_image: is_local_image_file_path(path),
            });
        }
    }
    FileListProjection {
        entries,
        total_entries,
        fallback: TextProjection::new(text, hits),
        mode,
    }
}

/// Mirror localPathProbe's raw primary choice, before any display normalization
/// or highlighted snippet. A candidate may have more body text after its first LF.
pub fn project_history_local_path(
    item: &ClipboardItemDto,
    text: TextProjection,
) -> Option<LocalPathProjection> {
    if matches!(item.kind.as_str(), "FileClipItem" | "ImageClipItem") {
        return None;
    }
    let primary = [
        item.display_text.as_str(),
        item.title.as_str(),
        item.summary.as_str(),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or_default();
    let path = js_trim(js_trim(primary).split('\n').next().unwrap_or_default());
    if !looks_like_single_line_local_path(path) {
        return None;
    }
    Some(LocalPathProjection {
        path: path.to_owned(),
        is_image: is_local_image_file_path(path),
        text,
    })
}

// ECMAScript \s/trim includes FEFF and excludes Unicode White_Space's U+0085.
fn js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(js_whitespace)
}

fn file_name(path: &str) -> &str {
    // getFileName uses only the first CRLF/LF line and keeps a trailing-slash
    // path as its own name instead of displaying an empty final component.
    let first_line = js_trim(path.split('\n').next().unwrap_or_default());
    first_line
        .rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(first_line)
}

fn looks_like_single_line_local_path(path: &str) -> bool {
    let length = path.encode_utf16().count();
    if !(3..=520).contains(&length) || path.contains(['\r', '\n', '\u{2028}', '\u{2029}']) {
        return false;
    }
    let bytes = path.as_bytes();
    if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/') {
        return true;
    }
    // /^[\\][\\][^\s\\]+[\\/]/: the server class allows '/', and
    // excludes JS whitespace rather than Rust's Unicode whitespace set.
    let Some(rest) = path.strip_prefix("\\\\") else {
        return false;
    };
    let mut chars = rest.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '\\' || js_whitespace(first) {
        return false;
    }
    for ch in chars {
        if matches!(ch, '\\' | '/') {
            return true;
        }
        if js_whitespace(ch) {
            return false;
        }
    }
    false
}

fn has_uri_scheme(path: &str) -> bool {
    if path
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
        || path
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("blob:"))
    {
        return true;
    }
    let Some((scheme, _)) = path.split_once("://") else {
        return false;
    };
    let mut chars = scheme.bytes();
    chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'+' | b'.' | b'-'))
}

fn is_local_image_file_path(path: &str) -> bool {
    let path = js_trim(path);
    if path.is_empty() || has_uri_scheme(path) {
        return false;
    }
    let base = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let Some((_, extension)) = base.rsplit_once('.') else {
        return false;
    };
    ["png", "jpg", "jpeg", "gif", "bmp", "webp"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

pub fn format_file_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return String::new();
    };
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = size as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    let rounded = if unit == 0 {
        size.round()
    } else {
        (size * 100.0).round() / 100.0
    };
    format!("{rounded}{}", units[unit])
}

pub fn format_image_meta(
    width: Option<u32>,
    height: Option<u32>,
    size: Option<u64>,
) -> Option<String> {
    let dimensions = width
        .zip(height)
        .map(|(width, height)| format!("{width}×{height}"));
    match (dimensions, size) {
        (Some(dimensions), Some(size)) => {
            Some(format!("{dimensions} · {}", format_file_size(Some(size))))
        }
        (Some(dimensions), None) => Some(dimensions),
        (None, Some(size)) => Some(format_file_size(Some(size))),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ClipboardItemDto {
        cliphost_core::dto::clip_to_dto(
            &cliphost_core::model::ClipRecord::new_text("body", "fixture.exe"),
            None,
        )
    }

    #[test]
    fn shared_file_paths_keep_holes_limits_and_match_history_without_mutating_inputs() {
        let mut paths = vec![
            String::new(),
            "\u{feff} C:\\photo.PNG \u{00a0}".to_owned(),
            " \n\u{feff}".to_owned(),
            "C:\\document.txt".to_owned(),
        ];
        paths.extend((0..8).map(|index| format!("C:\\extra{index}.txt")));
        let before_paths = paths.clone();
        let mut item = fixture();
        item.kind = "FileClipItem".into();
        item.file_paths = paths.clone();
        let before_item = serde_json::to_value(&item).unwrap();
        let hits = [HighlightHitRangeDto { start: 0, end: 7 }];
        for mode in [ContentMode::Standard, ContentMode::Minimal] {
            let projection = project_file_paths(&paths, mode, "visible title", &hits);
            assert_eq!(projection.entries.len(), FILE_CHIP_PREVIEW_LIMIT);
            assert_eq!(projection.total_entries, 10);
            assert_eq!(projection.entries[0].original_index, 1);
            assert_eq!(projection.entries[0].path, "C:\\photo.PNG");
            assert_eq!(projection.entries[0].name, "photo.PNG");
            assert!(projection.entries[0].is_image);
            assert_eq!(projection.entries[1].original_index, 3);
            assert!(!projection.entries[1].is_image);
            assert_eq!(projection.mode, mode);
            assert_eq!(
                projection.fallback,
                TextProjection::new("visible title", &hits)
            );
            assert_eq!(
                project_history_files(&item, mode, "visible title", &hits),
                Some(projection)
            );
        }
        assert_eq!(paths, before_paths);
        assert_eq!(serde_json::to_value(&item).unwrap(), before_item);
    }

    #[test]
    fn file_projection_keeps_original_indices_limits_and_fallback_without_dto_changes() {
        let mut item = fixture();
        item.kind = "FileClipItem".into();
        item.file_count = 999;
        item.file_paths = vec![
            " \u{feff}\u{00a0}".into(),
            " \u{feff}C:\\folder\\photo.PNG\u{00a0}".into(),
            "\u{0085}".into(),
            "\\\\server\\share\\".into(),
            "C:\\folder\\name.txt\r\nother".into(),
        ];
        item.file_paths
            .extend((5..=10).map(|index| format!("C:\\f{index}.txt")));
        let before = serde_json::to_value(&item).unwrap();
        let hits = [HighlightHitRangeDto { start: 0, end: 8 }];
        for mode in [ContentMode::Standard, ContentMode::Minimal] {
            let projection = project_history_files(&item, mode, "selected body", &hits).unwrap();
            assert_eq!(projection.mode, mode);
            assert_eq!(projection.total_entries, 10);
            assert_eq!(projection.entries.len(), FILE_CHIP_PREVIEW_LIMIT);
            assert_eq!(
                projection
                    .entries
                    .iter()
                    .map(|entry| entry.original_index)
                    .collect::<Vec<_>>(),
                (1..=8).collect::<Vec<_>>()
            );
            assert_eq!(projection.entries[0].path, "C:\\folder\\photo.PNG");
            assert_eq!(projection.entries[0].name, "photo.PNG");
            assert!(projection.entries[0].is_image);
            assert_eq!(projection.entries[1].name, "\u{0085}");
            assert_eq!(projection.entries[2].name, "\\\\server\\share\\");
            assert_eq!(projection.entries[3].name, "name.txt");
            assert_eq!(
                projection.fallback,
                TextProjection::new("selected body", &hits)
            );
        }
        assert_eq!(serde_json::to_value(&item).unwrap(), before);
        item.file_paths = vec!["\u{feff} \n".into()];
        let empty = project_history_files(&item, ContentMode::Standard, "fallback", &[]).unwrap();
        assert!(empty.entries.is_empty());
        assert_eq!(empty.total_entries, 0);
        assert_eq!(empty.fallback.text, "fallback");
        item.kind = "TextClipItem".into();
        assert!(project_history_files(&item, ContentMode::Standard, "fallback", &[]).is_none());
    }

    #[test]
    fn local_path_projection_matches_web_first_line_utf16_and_image_rules() {
        for (raw, expected) in [
            (
                " \u{feff}C:\\photo.PNG\u{00a0}\r\nlater body ",
                Some(("C:\\photo.PNG", true)),
            ),
            ("\nC:/photo.jpeg\nbody", Some(("C:/photo.jpeg", true))),
            (
                "\\\\server\\share\\doc.pdf",
                Some(("\\\\server\\share\\doc.pdf", false)),
            ),
            (
                "\\\\ser\u{0085}ver\\image.webp",
                Some(("\\\\ser\u{0085}ver\\image.webp", true)),
            ),
            ("\\\\ser\u{feff}ver\\image.webp", None),
            ("C://image.png", Some(("C://image.png", false))),
            ("C:\\image.bmp", Some(("C:\\image.bmp", true))),
            ("C:\\image.gif", Some(("C:\\image.gif", true))),
            ("C:\\image.jpg", Some(("C:\\image.jpg", true))),
            ("C:\\image.svg", Some(("C:\\image.svg", false))),
            ("C:\\image.png?x", Some(("C:\\image.png?x", false))),
            ("C:relative.png", None),
            ("file:///C:/image.png", None),
            ("https://example.test/image.png", None),
            ("C:\\a\rb.png", None),
            ("C:\\a\u{2028}b.png", None),
            ("C:\\a\u{2029}b.png", None),
            ("\\\\server", None),
            (" ", None),
        ] {
            let mut item = fixture();
            item.display_text = raw.into();
            item.title = "C:\\ignored.png".into();
            item.highlighted_text = "C:\\wrong.png".into();
            let before = serde_json::to_value(&item).unwrap();
            let text = TextProjection::new(
                "visible snippet",
                &[HighlightHitRangeDto { start: 0, end: 7 }],
            );
            let projected = project_history_local_path(&item, text.clone());
            assert_eq!(
                projected
                    .as_ref()
                    .map(|path| (path.path.as_str(), path.is_image)),
                expected,
                "{raw:?}"
            );
            if let Some(projected) = projected {
                assert_eq!(projected.text, text);
            }
            assert_eq!(serde_json::to_value(&item).unwrap(), before);
        }
        let mut item = fixture();
        item.display_text = format!("C:\\{}a", "😀".repeat(258)); // 520 UTF-16 units.
        assert!(project_history_local_path(&item, TextProjection::new("snippet", &[])).is_some());
        item.display_text.push('a');
        assert!(project_history_local_path(&item, TextProjection::new("snippet", &[])).is_none());
        item.display_text.clear();
        item.title = "C:\\from-title.png".into();
        assert_eq!(
            project_history_local_path(&item, TextProjection::new("snippet", &[]))
                .unwrap()
                .path,
            item.title
        );
        for kind in ["FileClipItem", "ImageClipItem"] {
            item.kind = kind.into();
            assert!(
                project_history_local_path(&item, TextProjection::new("snippet", &[])).is_none()
            );
        }
    }

    #[test]
    fn metadata_format_matches_web_units_and_optional_dimensions() {
        for (size, expected) in [
            (None, ""),
            (Some(0), "0B"),
            (Some(1023), "1023B"),
            (Some(1024), "1KB"),
            (Some(1536), "1.5KB"),
            (Some(1_048_576), "1MB"),
        ] {
            assert_eq!(format_file_size(size), expected);
        }
        assert_eq!(
            format_image_meta(Some(640), Some(480), Some(1536)),
            Some("640×480 · 1.5KB".into())
        );
        assert_eq!(
            format_image_meta(Some(640), Some(480), None),
            Some("640×480".into())
        );
        assert_eq!(format_image_meta(None, None, Some(0)), Some("0B".into()));
        assert_eq!(format_image_meta(None, None, None), None);
    }
}
