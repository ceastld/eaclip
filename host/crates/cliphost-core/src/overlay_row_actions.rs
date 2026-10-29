//! Presentation and shortcut availability shared by native overlay surfaces.
//! It does not load records, write the clipboard, or resolve an input target.

#[derive(Clone, Debug, Default)]
pub struct RowItem {
    pub kind: String,
    pub display_text: String,
    pub summary: String,
    pub title: String,
    pub file_paths: Vec<String>,
    pub file_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowActionKind {
    Pin {
        provider: String,
    },
    Simulate,
    Open {
        target_kind: &'static str,
        value: String,
    },
}

impl RowActionKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pin { .. } => "pin",
            Self::Simulate => "simulate",
            Self::Open { .. } => "open",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowAction {
    pub shortcut: usize,
    pub kind: RowActionKind,
}

pub fn row_text(item: &RowItem) -> String {
    [&item.display_text, &item.summary, &item.title]
        .into_iter()
        .find(|text| !text.is_empty())
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

fn path_line(item: &RowItem) -> String {
    [&item.display_text, &item.title, &item.summary]
        .into_iter()
        .find(|text| !text.is_empty())
        .and_then(|text| text.trim().lines().next())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn trim_punctuation(value: &str) -> &str {
    value
        .trim()
        .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '>'])
}

pub fn windows_path(value: &str) -> Option<String> {
    let value = trim_punctuation(value);
    if value.starts_with("~/") || value.starts_with("~\\") {
        return Some(value.to_string());
    }
    if value.len() < 3
        || value.chars().count() > 520
        || value.contains(['\r', '\n', '\u{2028}', '\u{2029}'])
    {
        return None;
    }
    let bytes = value.as_bytes();
    let drive =
        bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\');
    let unc = value.strip_prefix("\\\\").is_some_and(|rest| {
        rest.find(['/', '\\'])
            .is_some_and(|end| end > 0 && !rest[..end].contains(char::is_whitespace))
    });
    (drive || unc).then(|| value.replace('/', "\\"))
}

pub fn local_raster(path: &str) -> bool {
    let path = path.trim();
    let lower = path.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("data:") || lower.starts_with("blob:") {
        return false;
    }
    path.rsplit(['/', '\\'])
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(_, ext)| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp"
            )
        })
}

/// Conservatively detect the same HTTP(S)/www token the WebView offers. The
/// core shell handler remains responsible for URL parsing and scheme checks.
pub fn http_uri(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let start = ["http://", "https://", "www."]
        .iter()
        .filter_map(|prefix| lower.find(prefix))
        .min()?;
    let token = text
        .get(start..)?
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '<' | '>' | '\"' | '\''))
        .next()?;
    let token = trim_punctuation(token);
    let uri = if token.to_ascii_lowercase().starts_with("www.") {
        format!("https://{token}")
    } else {
        token.to_string()
    };
    let authority = uri.split_once("://")?.1.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('\\') {
        return None;
    }
    Some(uri)
}

pub fn open_target(item: &RowItem) -> Option<(&'static str, String)> {
    if matches!(item.kind.as_str(), "ImageClipItem" | "FileClipItem") {
        return item
            .file_paths
            .first()
            .map(|path| path.trim())
            .filter(|path| !path.is_empty())
            .map(|path| ("path", path.to_string()));
    }
    if item.kind == "LinkClipItem" {
        if let Some(uri) = http_uri(&item.summary)
            .or_else(|| http_uri(&item.display_text))
            .or_else(|| http_uri(&item.title))
        {
            return Some(("uri", uri));
        }
    }
    let line = path_line(item);
    windows_path(&line)
        .map(|path| ("path", path))
        .or_else(|| http_uri(&line).map(|uri| ("uri", uri)))
}

pub fn pinable(item: &RowItem) -> bool {
    if item.kind == "ImageClipItem" {
        return true;
    }
    if item.kind == "FileClipItem" {
        return item.file_count == 1
            && item
                .file_paths
                .first()
                .is_some_and(|path| local_raster(path));
    }
    let line = path_line(item);
    windows_path(&line).is_some() && !line.starts_with('~') && local_raster(&line)
}

pub fn auto_row_actions(item: &RowItem, provider: Option<&str>) -> Vec<RowAction> {
    let mut automatic = Vec::new();
    if pinable(item) {
        if let Some(provider) =
            provider.filter(|provider| matches!(*provider, "snipaste" | "pixpin"))
        {
            automatic.push(RowActionKind::Pin {
                provider: provider.to_string(),
            });
        }
    }
    let text = row_text(item);
    let simulate = !matches!(item.kind.as_str(), "FileClipItem" | "ImageClipItem")
        && !text.is_empty()
        && text != "…";
    if simulate {
        automatic.push(RowActionKind::Simulate);
    }
    let open =
        open_target(item).map(|(target_kind, value)| RowActionKind::Open { target_kind, value });
    if let Some(open) = &open {
        automatic.push(open.clone());
    }
    automatic
        .into_iter()
        .enumerate()
        .map(|(index, kind)| RowAction {
            shortcut: index + 1,
            kind,
        })
        .collect()
}

pub fn row_actions(item: &RowItem, slots: [&str; 2], provider: Option<&str>) -> Vec<RowAction> {
    let automatic = auto_row_actions(item, provider);
    let simulate = automatic
        .iter()
        .any(|action| matches!(action.kind, RowActionKind::Simulate));
    let open = automatic
        .iter()
        .find(|action| matches!(action.kind, RowActionKind::Open { .. }))
        .map(|action| action.kind.clone());
    slots
        .into_iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            let kind = match slot.trim().to_ascii_lowercase().as_str() {
                "none" => None,
                "simulate" => simulate.then_some(RowActionKind::Simulate),
                "open" => open.clone(),
                _ => automatic.get(index).map(|action| action.kind.clone()),
            }?;
            Some(RowAction {
                shortcut: index + 1,
                kind,
            })
        })
        .collect()
}

/// Pinned rows restart numbering in reverse order, matching the WebView.
pub fn display_index(pinned: &[bool], index: usize) -> (usize, bool) {
    match pinned.get(index) {
        Some(true) => (
            pinned[index..].iter().filter(|pinned| **pinned).count(),
            true,
        ),
        Some(false) => (
            pinned[..=index].iter().filter(|pinned| !**pinned).count(),
            false,
        ),
        None => (index + 1, false),
    }
}

/// Match QuickPasteOverlayPage.computeListPlacement's side decision in screen
/// pixels. Negative monitor origins and physical DPI-scaled inputs are valid.
pub fn preview_on_left(
    cursor_x: f64,
    work_left: f64,
    work_width: f64,
    panel_width: f64,
    preview_width: f64,
    gap: f64,
    edge: f64,
    cursor_gap: f64,
) -> bool {
    if preview_width <= 0.0 || work_width <= 0.0 {
        return false;
    }
    let cursor = cursor_x - work_left;
    let mut list_left = if cursor >= work_width / 2.0 {
        cursor - panel_width - cursor_gap
    } else {
        cursor + cursor_gap
    };
    if list_left + panel_width > work_width - edge {
        list_left = cursor - panel_width - cursor_gap;
    }
    if list_left < edge {
        list_left = cursor + cursor_gap;
    }
    list_left = list_left
        .max(edge)
        .min((work_width - panel_width - edge).max(edge));
    let right = work_width - edge - (list_left + panel_width + gap);
    let left = list_left - gap - edge;
    if list_left + panel_width / 2.0 > work_width / 2.0 {
        left >= preview_width || left >= right
    } else if right >= preview_width && left >= preview_width {
        left > right
    } else if right >= preview_width {
        false
    } else if left >= preview_width {
        true
    } else {
        left > right
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_slots_keep_numbers_and_never_offer_unavailable_operations() {
        let image = RowItem {
            kind: "ImageClipItem".into(),
            file_paths: vec!["C:\\a.png".into()],
            ..Default::default()
        };
        assert_eq!(
            row_actions(&image, ["auto", "auto"], None),
            vec![RowAction {
                shortcut: 1,
                kind: RowActionKind::Open {
                    target_kind: "path",
                    value: "C:\\a.png".into()
                }
            }]
        );
        assert_eq!(
            row_actions(&image, ["none", "open"], Some("pixpin"))[0].shortcut,
            2
        );
        assert!(row_actions(&image, ["simulate", "none"], Some("pixpin")).is_empty());
        let actions = row_actions(&image, ["auto", "auto"], Some("snipaste"));
        assert_eq!(actions[0].kind.name(), "pin");
        assert_eq!(actions[1].kind.name(), "open");
    }
    #[test]
    fn local_image_text_and_multifile_have_frontend_availability() {
        let image_text = RowItem {
            kind: "TextClipItem".into(),
            display_text: "C:\\images\\a.webp\ncaption".into(),
            ..Default::default()
        };
        assert_eq!(
            row_actions(&image_text, ["auto", "auto"], Some("pixpin"))
                .iter()
                .map(|a| a.kind.name())
                .collect::<Vec<_>>(),
            ["pin", "simulate"]
        );
        let files = RowItem {
            kind: "FileClipItem".into(),
            file_count: 2,
            file_paths: vec!["C:\\a.png".into(), "C:\\b.png".into()],
            ..Default::default()
        };
        assert!(!pinable(&files));
        assert_eq!(
            row_actions(&files, ["auto", "auto"], Some("pixpin"))[0]
                .kind
                .name(),
            "open"
        );
        let web = RowItem {
            kind: "TextClipItem".into(),
            display_text: "https://a.test/image.png".into(),
            ..Default::default()
        };
        assert!(!pinable(&web));
    }
    #[test]
    fn open_detection_preserves_paths_and_extracts_only_http_schemes() {
        assert_eq!(
            windows_path("C:/hello world/a.txt"),
            Some("C:\\hello world\\a.txt".into())
        );
        assert!(windows_path("regular sentence").is_none());
        assert_eq!(
            http_uri("go to www.example.test/a)."),
            Some("https://www.example.test/a".into())
        );
        assert_eq!(
            http_uri("HTTPS://example.test/x?y=1"),
            Some("HTTPS://example.test/x?y=1".into())
        );
        assert!(http_uri("file:///C:/a.txt").is_none());
        assert!(http_uri("https://").is_none());
    }
    #[test]
    fn pinned_numbering_restarts_from_list_tail() {
        let pinned = [false, false, true, true];
        assert_eq!(display_index(&pinned, 1), (2, false));
        assert_eq!(display_index(&pinned, 2), (2, true));
        assert_eq!(display_index(&pinned, 3), (1, true));
    }

    #[test]
    fn preview_side_uses_actual_anchor_and_negative_monitor_origins() {
        assert!(!preview_on_left(
            100.0, 0.0, 1920.0, 300.0, 400.0, 8.0, 14.0, 12.0
        ));
        assert!(preview_on_left(
            1800.0, 0.0, 1920.0, 300.0, 400.0, 8.0, 14.0, 12.0
        ));
        assert!(preview_on_left(
            -120.0, -1920.0, 1920.0, 300.0, 400.0, 8.0, 14.0, 12.0
        ));
        assert!(!preview_on_left(
            1800.0, 0.0, 1920.0, 300.0, 0.0, 8.0, 14.0, 12.0
        ));
    }
}
