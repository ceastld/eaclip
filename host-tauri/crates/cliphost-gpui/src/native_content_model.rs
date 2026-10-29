//! Read-only adapters for the existing list DTO. No payload parsing or I/O.

use std::ops::Range;

use cliphost_core::{
    dto::{ClipboardItemDto, HighlightHitRangeDto},
    im_rich::{limit_for_list_preview, ImRichSegment, ImRichSegmentKind},
};
use serde_json::Value;

use super::files::{FileListProjection, LocalPathProjection};
use super::links::{collect_inline_links, utf16_hit_ranges, InlineLink};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentMode {
    Standard,
    /// Matches the current Web minimal row: QQ remains text, steps are structured.
    Minimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextProjection {
    /// Display only. Never pass this string to edit/copy/paste/export actions.
    pub text: String,
    pub links: Vec<InlineLink>,
    pub hit_ranges: Vec<Range<usize>>,
}

impl TextProjection {
    /// `hits` address this exact string in UTF-16. Call after any display-text
    /// preparation, with its remapped ranges; do not normalize indexed text here.
    pub fn new(text: &str, hits: &[HighlightHitRangeDto]) -> Self {
        Self {
            text: text.to_owned(),
            links: collect_inline_links(text),
            hit_ranges: utf16_hit_ranges(text, hits),
        }
    }

    /// Sorted, disjoint byte spans, as required by GPUI StyledText. Combining
    /// overlapping link and search styles as separate GPUI runs loses content.
    pub fn styled_spans(&self) -> Vec<TextSpan> {
        let mut boundaries = vec![0, self.text.len()];
        for range in self
            .links
            .iter()
            .map(|link| &link.range)
            .chain(&self.hit_ranges)
        {
            boundaries.extend([range.start, range.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        boundaries
            .windows(2)
            .filter_map(|pair| {
                let range = pair[0]..pair[1];
                if range.is_empty() {
                    return None;
                }
                let link = self
                    .links
                    .iter()
                    .any(|link| link.range.contains(&range.start));
                let hit = self.hit_ranges.iter().any(|hit| hit.contains(&range.start));
                (link || hit).then_some(TextSpan { range, link, hit })
            })
            .collect()
    }
}

/// History's ordinary standard content branch may show a distinct summary below
/// the primary text. Mirror ClipboardPage's *local* normalize function, which
/// keeps LF; Favorites' shared whitespace-flattening projection is separate.
/// Compare against the raw DTO primary choice, never a highlighted snippet.
pub fn history_secondary_projection(
    item: &ClipboardItemDto,
    mode: ContentMode,
) -> Option<TextProjection> {
    if mode == ContentMode::Minimal
        || matches!(item.kind.as_str(), "FileClipItem" | "ImageClipItem")
        || (item.kind == "StepClipItem" && item.step_presentation.is_some())
    {
        return None;
    }
    let summary = normalize_history_list_text(&item.summary);
    if summary.is_empty() {
        return None;
    }
    // JavaScript's || tests the original string's emptiness, before trim.
    let primary = [
        item.display_text.as_str(),
        item.title.as_str(),
        item.summary.as_str(),
    ]
    .into_iter()
    .find(|text| !text.is_empty())
    .unwrap_or_default();
    (summary != normalize_history_list_text(primary)).then(|| TextProjection::new(&summary, &[]))
}

fn js_list_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

// ClipboardPage: CRLF/CR -> LF; horizontal JS whitespace runs -> one space;
// spaces adjoining LF -> none; final JS trim. U+0085 is not JS whitespace.
fn normalize_history_list_text(raw: &str) -> String {
    let mut output = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(mut ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            ch = '\n';
        }
        if ch == '\n' {
            if output.ends_with(' ') {
                output.pop();
            }
            output.push('\n');
        } else if js_list_whitespace(ch) {
            if !output.ends_with(' ') && !output.ends_with('\n') {
                output.push(' ');
            }
        } else {
            output.push(ch);
        }
    }
    output.trim_matches(js_list_whitespace).to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSpan {
    pub range: Range<usize>,
    pub link: bool,
    pub hit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MixedPart {
    Text(String),
    Image { path: Option<String> },
    File { name: String, path: String },
    Placeholder(String),
}

pub const COMPACT_IMAGE_MAX_HEIGHT: f32 = 56.0;

/// CSS white-space:normal collapses these ASCII separators, but not NBSP.
/// Used only to prepare mixed preview text; the DTO text remains untouched.
pub fn collapse_inline_whitespace(text: &str) -> String {
    let mut output = String::new();
    let mut in_space = false;
    for ch in text.chars() {
        let space = matches!(ch, ' ' | '\t' | '\r' | '\n' | '\u{000c}');
        if space {
            if !in_space {
                output.push(' ');
            }
        } else {
            output.push(ch);
        }
        in_space = space;
    }
    output
}

/// Display dimensions follow the original natural aspect ratio, independently
/// of the worker's decoded thumbnail dimensions. Small images are not enlarged.
pub fn compact_image_size(width: f32, height: f32, available_width: f32) -> (f32, f32) {
    let scale = (available_width.max(1.0) / width.max(1.0))
        .min(COMPACT_IMAGE_MAX_HEIGHT / height.max(1.0))
        .min(1.0);
    (width.max(1.0) * scale, height.max(1.0) * scale)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepLine {
    pub title: String,
    pub suffix: String,
    pub display_text: String,
    pub icon: String,
    pub disabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepProjection {
    pub lines: Vec<StepLine>,
    pub preview_text: String,
    pub stats: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentProjection {
    Text(TextProjection),
    Mixed(Vec<MixedPart>),
    Steps(StepProjection),
    Files(FileListProjection),
    LocalPath(LocalPathProjection),
}

fn string(value: &Value, name: &str) -> String {
    value[name].as_str().unwrap_or_default().to_owned()
}

/// Decode the shared core segment DTO into its existing typed model, then use
/// the core preview limit. XML/envelopes and unknown nodes remain in core.
pub fn project_mixed(values: &[Value]) -> Vec<MixedPart> {
    project_mixed_indexed(values)
        .into_iter()
        .map(|(_, part)| part)
        .collect()
}

/// Parallel to `project_mixed`: original DTO indices, including repeated image
/// paths. Empty text filtering and the core preview limit never renumber them.
pub fn project_mixed_source_indices(values: &[Value]) -> Vec<usize> {
    project_mixed_indexed(values)
        .into_iter()
        .map(|(index, _)| index)
        .collect()
}

fn project_mixed_indexed(values: &[Value]) -> Vec<(usize, MixedPart)> {
    let segments: Vec<_> = values
        .iter()
        .take(cliphost_core::im_rich::LIST_PREVIEW_SEGMENT_LIMIT)
        .map(|value| ImRichSegment {
            kind: match value["kind"].as_str() {
                Some("image") => ImRichSegmentKind::Image,
                Some("file") => ImRichSegmentKind::File,
                Some("placeholder") => ImRichSegmentKind::Placeholder,
                _ => ImRichSegmentKind::Text,
            },
            text: if value["kind"] == "placeholder" && value["text"].is_null() {
                "[表情]".to_owned()
            } else {
                string(value, "text")
            },
            path: string(value, "path"),
        })
        .collect();
    limit_for_list_preview(&segments)
        .into_iter()
        .enumerate()
        .filter_map(|(source_index, segment)| {
            let part = match segment.kind {
                ImRichSegmentKind::Text => {
                    let text = segment.text.trim();
                    (!text.is_empty()).then(|| MixedPart::Text(text.to_owned()))
                }
                ImRichSegmentKind::Image => {
                    let path = segment.path.trim();
                    Some(MixedPart::Image {
                        path: is_local_image_path(path).then(|| path.to_owned()),
                    })
                }
                ImRichSegmentKind::File => {
                    let path = segment.path.trim().to_owned();
                    let name = path
                        .rsplit(['/', '\\'])
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or("[文件]")
                        .to_owned();
                    Some(MixedPart::File { name, path })
                }
                ImRichSegmentKind::Placeholder => Some(MixedPart::Placeholder(segment.text)),
            };
            part.map(|part| (source_index, part))
        })
        .collect()
}

/// Mirrors localImagePath's extension/URI test. Actual authorization and reads
/// are still performed by HostState::local_image_bytes on a background worker.
pub fn is_local_image_path(path: &str) -> bool {
    let path = path.trim();
    let lower = path.to_ascii_lowercase();
    if path.is_empty() || lower.starts_with("data:") || lower.starts_with("blob:") {
        return false;
    }
    if let Some((scheme, _)) = path.split_once("://") {
        if scheme
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
        {
            return false;
        }
    }
    let name = lower.rsplit(['/', '\\']).next().unwrap_or_default();
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(extension, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp")
    })
}

/// The core quicker_steps::presentation_dto already projects the bounded,
/// ordered top-level lines. Consume it without analyzing/flattening StepData.
pub fn project_steps(value: &Value) -> Option<StepProjection> {
    let primary_icon = string(value, "primaryIcon");
    let lines = value["previewLines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|line| {
            let original_title = string(line, "title");
            let display_text = string(line, "displayText");
            let title = [original_title.trim(), display_text.trim(), "—"]
                .into_iter()
                .find(|title| !title.is_empty())
                .unwrap()
                .to_owned();
            let suffix = string(line, "summarySuffix");
            let summary = string(line, "summary");
            let suffix = if !suffix.trim().is_empty() {
                suffix.trim().to_owned()
            } else if !summary.trim().is_empty() && summary.trim() != original_title.trim() {
                format!(": {}", summary.trim())
            } else {
                String::new()
            };
            let icon = string(line, "icon");
            // Web uses JS || here, rather than trim; keep its fallback selection.
            let icon = if !icon.is_empty() {
                icon
            } else if !primary_icon.is_empty() {
                primary_icon.clone()
            } else {
                "fa:Light_ListOl".to_owned()
            };
            StepLine {
                title,
                suffix,
                display_text,
                icon,
                disabled: line["disabled"].as_bool().unwrap_or(false),
            }
        })
        .collect::<Vec<_>>();
    let preview_text = string(value, "previewText");
    let stats = string(value, "statsLabel").trim().to_owned();
    (!lines.is_empty() || !preview_text.trim().is_empty() || !stats.is_empty()).then_some(
        StepProjection {
            lines,
            preview_text,
            stats,
        },
    )
}

/// File/image rows retain their existing specialized renderer. This projection
/// never owns the original DTO or the edit/output payload.
pub fn project_item(item: &ClipboardItemDto, mode: ContentMode) -> Option<ContentProjection> {
    let text = if !item.highlighted_text.is_empty() {
        &item.highlighted_text
    } else if !item.display_text.is_empty() {
        &item.display_text
    } else {
        &item.summary
    };
    project_item_with_text(item, mode, text, &item.highlight_hit_ranges)
}

/// Optional prepared display text lets the parent reuse its existing whitespace,
/// search and clamp preparation. Its supplied hits must address `text` exactly.
pub fn project_item_with_text(
    item: &ClipboardItemDto,
    mode: ContentMode,
    text: &str,
    hits: &[HighlightHitRangeDto],
) -> Option<ContentProjection> {
    if matches!(item.kind.as_str(), "ImageClipItem" | "FileClipItem") {
        return None;
    }
    if item.kind == "StepClipItem" {
        if let Some(steps) = item.step_presentation.as_ref().and_then(project_steps) {
            return Some(ContentProjection::Steps(steps));
        }
    }
    if mode == ContentMode::Standard
        && item.kind == cliphost_core::model::ClipKind::QQRichTextItem.as_str()
        && !item.im_rich_segments.is_empty()
    {
        return Some(ContentProjection::Mixed(project_mixed(
            &item.im_rich_segments,
        )));
    }
    Some(ContentProjection::Text(TextProjection::new(text, hits)))
}

/// Opt-in history adapter. The old compact entry points keep their established
/// file/image behavior. Candidates always come from the original immutable DTO,
/// while the supplied display text and UTF-16 hits remain indexed together.
pub fn project_history_item_with_text(
    item: &ClipboardItemDto,
    mode: ContentMode,
    text: &str,
    hits: &[HighlightHitRangeDto],
) -> Option<ContentProjection> {
    if let Some(files) = super::files::project_history_files(item, mode, text, hits) {
        return Some(ContentProjection::Files(files));
    }
    let projection = project_item_with_text(item, mode, text, hits)?;
    if let ContentProjection::Text(text) = projection {
        return Some(
            match super::files::project_history_local_path(item, text.clone()) {
                Some(path) => ContentProjection::LocalPath(path),
                None => ContentProjection::Text(text),
            },
        );
    }
    Some(projection)
}

impl ContentProjection {
    /// Includes metadata-only path probes, whose resources may contain no image
    /// or icon. Callers must use this when scheduling visible history rows.
    pub fn needs_resources(&self) -> bool {
        match self {
            Self::Files(files) => !files.entries.is_empty(),
            Self::LocalPath(_) => true,
            _ => !self.image_paths().is_empty() || !self.icon_specs().is_empty(),
        }
    }
    pub fn image_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if let Self::Mixed(parts) = self {
            for part in parts {
                if let MixedPart::Image { path: Some(path) } = part {
                    if !paths.contains(path) {
                        paths.push(path.clone());
                    }
                }
            }
        }
        paths
    }

    pub fn icon_specs(&self) -> Vec<String> {
        let mut specs = Vec::new();
        if let Self::Steps(steps) = self {
            for line in &steps.lines {
                if !specs.contains(&line.icon) {
                    specs.push(line.icon.clone());
                }
            }
        }
        specs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn history_adapter_adds_file_and_path_resources_without_replacing_rich_branches() {
        let mut item = cliphost_core::dto::clip_to_dto(
            &cliphost_core::model::ClipRecord::new_text("body", "fixture.exe"),
            None,
        );
        item.display_text = r"C:\original.png".into();
        item.highlighted_text = "visible snippet".into();
        let before = serde_json::to_value(&item).unwrap();
        let projected =
            project_history_item_with_text(&item, ContentMode::Standard, "visible snippet", &[])
                .unwrap();
        assert!(projected.needs_resources());
        let ContentProjection::LocalPath(path) = projected else {
            panic!("path preview");
        };
        assert_eq!(path.path, r"C:\original.png");
        assert_eq!(path.text.text, "visible snippet");
        assert_eq!(serde_json::to_value(&item).unwrap(), before);

        item.kind = "FileClipItem".into();
        item.file_paths = vec![String::new(), r"C:\file.txt".into()];
        assert!(project_item_with_text(&item, ContentMode::Standard, "body", &[]).is_none());
        let projection =
            project_history_item_with_text(&item, ContentMode::Minimal, "body", &[]).unwrap();
        assert!(projection.needs_resources());
        let ContentProjection::Files(files) = projection else {
            panic!("file chips");
        };
        assert_eq!(files.entries[0].original_index, 1);

        item.kind = "QQRichTextItem".into();
        item.im_rich_segments = vec![json!({"kind":"text","text":"rich body"})];
        assert!(matches!(
            project_history_item_with_text(&item, ContentMode::Standard, "body", &[]),
            Some(ContentProjection::Mixed(_))
        ));
        item.kind = "StepClipItem".into();
        item.step_presentation = Some(json!({"previewLines":[{"title":"step"}]}));
        assert!(matches!(
            project_history_item_with_text(&item, ContentMode::Minimal, "body", &[]),
            Some(ContentProjection::Steps(_))
        ));
        item.kind = "ImageClipItem".into();
        assert!(
            project_history_item_with_text(&item, ContentMode::Standard, "body", &[]).is_none()
        );
    }

    #[test]
    fn history_secondary_matches_local_web_normalization_gates_and_preserves_dto() {
        let fixture = cliphost_core::dto::clip_to_dto(
            &cliphost_core::model::ClipRecord::new_text("body", "fixture.exe"),
            None,
        );
        for (display, title, summary, snippet, expected) in [
            (
                "",
                "title",
                "body https://example.test/path",
                "body https://example.test/path",
                Some("body https://example.test/path"),
            ),
            (
                "same",
                "title",
                "same",
                "a different highlighted snippet",
                None,
            ),
            ("", "", "body", "a different snippet", None),
            ("title", "", "\u{feff}\u{00a0}\t", "hit", None),
            ("A B", "", "\u{00a0}A\u{feff}B\u{feff}", "hit", None),
            ("A\nB", "", " A \r\n \t B ", "hit", None),
            ("A B", "", " A \r\n B ", "hit", Some("A\nB")),
            (
                "title",
                "",
                "\u{feff} A\u{00a0}\r B\u{feff} ",
                "hit",
                Some("A\nB"),
            ),
            (
                "title",
                "",
                "\u{0085} A \u{0085}",
                "hit",
                Some("\u{0085} A \u{0085}"),
            ),
            (" ", "title", "body", "hit", Some("body")),
        ] {
            let mut item = fixture.clone();
            item.display_text = display.to_owned();
            item.title = title.to_owned();
            item.summary = summary.to_owned();
            item.highlighted_text = snippet.to_owned();
            item.highlight_hit_ranges = vec![HighlightHitRangeDto { start: 0, end: 2 }];
            let before = serde_json::to_value(&item).unwrap();
            let secondary = history_secondary_projection(&item, ContentMode::Standard);
            assert_eq!(secondary.as_ref().map(|text| text.text.as_str()), expected);
            if let Some(secondary) = secondary {
                assert!(secondary.hit_ranges.is_empty());
                if secondary.text.contains("https://") {
                    assert_eq!(secondary.links.len(), 1);
                    assert_eq!(
                        &secondary.text[secondary.links[0].range.clone()],
                        "https://example.test/path"
                    );
                }
            }
            assert_eq!(serde_json::to_value(&item).unwrap(), before);
        }
        for (kind, mode, steps, visible) in [
            ("TextClipItem", ContentMode::Minimal, None, false),
            ("FileClipItem", ContentMode::Standard, None, false),
            ("ImageClipItem", ContentMode::Standard, None, false),
            (
                "StepClipItem",
                ContentMode::Standard,
                Some(json!({})),
                false,
            ),
            ("StepClipItem", ContentMode::Standard, None, true),
            ("QQRichTextItem", ContentMode::Standard, None, true),
            ("HtmlClipItem", ContentMode::Standard, None, true),
            ("TextClipItem", ContentMode::Standard, None, true),
            ("Other", ContentMode::Standard, None, true),
        ] {
            let mut item = fixture.clone();
            item.kind = kind.to_owned();
            item.title = "title".to_owned();
            item.display_text = "title".to_owned();
            item.summary = "body".to_owned();
            item.step_presentation = steps;
            let before = serde_json::to_value(&item).unwrap();
            assert_eq!(
                history_secondary_projection(&item, mode).is_some(),
                visible,
                "{kind:?} {mode:?}"
            );
            assert_eq!(serde_json::to_value(&item).unwrap(), before);
        }
    }

    #[test]
    fn mixed_hover_indices_keep_empty_text_gaps_and_repeated_paths() {
        let mut values = vec![
            json!({"kind":"text","text":"  "}),
            json!({"kind":"image","path":"C:\\repeat.png"}),
            json!({"kind":"text","text":""}),
            json!({"kind":"file","path":"C:\\file.txt"}),
            json!({"kind":"image","path":"C:\\repeat.png"}),
            json!({"kind":"placeholder","text":""}),
        ];
        values.extend((6..13).map(|index| json!({"kind":"text","text":format!("t{index}")})));
        let before = values.clone();
        let indices = project_mixed_source_indices(&values);
        assert_eq!(indices, [1, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(indices.len(), project_mixed(&values).len());
        assert_eq!(values, before);
    }

    #[test]
    fn real_core_qq_dto_selects_standard_mixed_and_minimal_text_without_mutation() {
        use cliphost_core::model::{ClipKind, ClipRecord};
        let mut clip = ClipRecord::new_text("plain summary", "qq.exe");
        clip.kind = ClipKind::QQRichTextItem;
        clip.qq_rich_text = Some(r#"<QQRichEditFormat><EditElement type="0"><![CDATA[before]]></EditElement><EditElement type="3" filepath="C:\a.png"></EditElement><EditElement type="0"><![CDATA[after]]></EditElement></QQRichEditFormat>"#.to_owned());
        let item = cliphost_core::dto::clip_to_dto(&clip, None);
        let before = serde_json::to_value(&item).unwrap();
        assert_eq!(item.kind, "QQRichTextItem");
        let ContentProjection::Mixed(parts) = project_item(&item, ContentMode::Standard).unwrap()
        else {
            panic!("actual QQ list DTO must select mixed content")
        };
        assert_eq!(
            parts,
            vec![
                MixedPart::Text("before".into()),
                MixedPart::Image {
                    path: Some(r"C:\a.png".into())
                },
                MixedPart::Text("after".into())
            ]
        );
        assert!(matches!(
            project_item(&item, ContentMode::Minimal),
            Some(ContentProjection::Text(_))
        ));
        assert_eq!(serde_json::to_value(&item).unwrap(), before);
        assert!(clip.qq_rich_text.unwrap().contains("<![CDATA[before]]>"));
    }

    #[test]
    fn real_core_step_dto_is_structured_in_both_modes_and_files_keep_their_renderer() {
        use cliphost_core::model::{ClipKind, ClipRecord};
        let mut clip = ClipRecord::new_text(
            r#"{"Steps":[{"StepRunnerKey":"sys:notify"}]}"#,
            "quicker.exe",
        );
        clip.kind = ClipKind::StepClipItem;
        let item = cliphost_core::dto::clip_to_dto(&clip, None);
        for mode in [ContentMode::Standard, ContentMode::Minimal] {
            assert!(matches!(
                project_item(&item, mode),
                Some(ContentProjection::Steps(_))
            ));
        }
        clip.kind = ClipKind::FileClipItem;
        assert!(project_item(
            &cliphost_core::dto::clip_to_dto(&clip, None),
            ContentMode::Standard
        )
        .is_none());
    }

    #[test]
    fn mixed_dto_preserves_order_and_null_vs_empty() {
        let values = vec![
            json!({"kind":"text","text":"  first  "}),
            json!({"kind":"image","path":" C:\\a.PNG "}),
            json!({"kind":"file","path":"C:\\folder\\note.txt"}),
            json!({"kind":"placeholder","text":""}),
            json!({"kind":"placeholder"}),
            json!({"kind":"text","text":" last "}),
        ];
        assert_eq!(
            project_mixed(&values),
            vec![
                MixedPart::Text("first".into()),
                MixedPart::Image {
                    path: Some("C:\\a.PNG".into())
                },
                MixedPart::File {
                    name: "note.txt".into(),
                    path: "C:\\folder\\note.txt".into()
                },
                MixedPart::Placeholder("".into()),
                MixedPart::Placeholder("[表情]".into()),
                MixedPart::Text("last".into())
            ]
        );
    }

    #[test]
    fn mixed_uses_core_preview_limit_not_file_paths_or_raw_payload() {
        let mut segments = (0..13)
            .map(|i| ImRichSegment {
                kind: ImRichSegmentKind::Text,
                text: format!("t{i}"),
                path: String::new(),
            })
            .collect::<Vec<_>>();
        segments.extend((0..6).map(|i| ImRichSegment {
            kind: ImRichSegmentKind::Image,
            text: String::new(),
            path: format!("C:\\{i}.png"),
        }));
        let values = segments
            .iter()
            .map(cliphost_core::im_rich::segment_to_dto)
            .collect::<Vec<_>>();
        let projection = ContentProjection::Mixed(project_mixed(&values));
        assert!(projection.image_paths().is_empty());
        let ContentProjection::Mixed(parts) = projection else {
            unreachable!()
        };
        assert_eq!(parts.len(), 12);
        assert_eq!(parts[11], MixedPart::Text("t11".into()));
    }

    #[test]
    fn local_image_extensions_match_web_and_do_not_read_paths() {
        for path in [
            "C:\\x.png",
            "\\\\server\\share\\a.JPEG",
            "relative.webp",
            "a.bmp",
        ] {
            assert!(is_local_image_path(path));
        }
        for path in [
            "",
            "https://example/a.png",
            "file:///C:/a.png",
            "data:image/png,a",
            "blob:a.png",
            "C:\\x.svg",
            "a.png?q=1",
        ] {
            assert!(!is_local_image_path(path), "{path}");
        }
    }

    #[test]
    fn mixed_ascii_whitespace_collapses_but_nonbreaking_space_remains() {
        assert_eq!(
            collapse_inline_whitespace("A\n\t B  C\r\nD\u{000c}E\u{00a0}\u{00a0}F"),
            "A B C D E\u{00a0}\u{00a0}F"
        );
    }

    #[test]
    fn image_geometry_uses_natural_aspect_for_wide_short_and_small_images() {
        assert_eq!(compact_image_size(600.0, 300.0, 400.0), (112.0, 56.0));
        assert_eq!(compact_image_size(1000.0, 10.0, 400.0), (400.0, 4.0));
        assert_eq!(compact_image_size(40.0, 40.0, 400.0), (40.0, 40.0));
    }

    #[test]
    fn step_titles_suffix_fallback_disabled_and_stats_match_web() {
        let value = json!({"primaryIcon":"primary", "statsLabel":" 2 步 · 1 变量 ", "previewText":"original", "previewLines":[
            {"title":" x ","summarySuffix":" = 1 ","disabled":true},
            {"title":"","displayText":" display ","summary":" detail ","icon":"custom"},
            {"title":"same","summary":"same"}, {}]});
        let steps = project_steps(&value).unwrap();
        assert_eq!(
            steps
                .lines
                .iter()
                .map(|line| line.title.as_str())
                .collect::<Vec<_>>(),
            ["x", "display", "same", "—"]
        );
        assert_eq!(steps.lines[0].suffix, "= 1");
        assert_eq!(steps.lines[1].suffix, ": detail");
        assert_eq!(steps.lines[2].suffix, "");
        assert!(steps.lines[0].disabled);
        assert_eq!(steps.lines[0].icon, "primary");
        assert_eq!(steps.lines[1].icon, "custom");
        assert_eq!(steps.stats, "2 步 · 1 变量");
        assert!(project_steps(&json!({})).is_none());
        assert!(project_steps(&json!({"statsLabel":"0 步 · 0 变量"})).is_some());
    }

    #[test]
    fn core_step_projection_retains_disabled_line_and_recursive_count() {
        let dto = cliphost_core::quicker_steps::presentation_dto(
            r#"{"Steps":[{"StepRunnerKey":"sys:assign","InputParams":{"VarName":"x","Value":"1"}},{"StepRunnerKey":"sys:notify","Disabled":true}],"Variables":[{"Key":"x"}]}"#,
        );
        let steps = project_steps(&dto).unwrap();
        assert_eq!(steps.lines.len(), 2);
        assert!(steps.lines[1].disabled);
        assert_eq!(steps.stats, "1 步 · 1 变量");
    }

    #[test]
    fn overlapping_search_and_link_styles_form_disjoint_exact_spans() {
        let text = "😀 https://example.test/中文).";
        let projection = TextProjection::new(text, &[HighlightHitRangeDto { start: 3, end: 12 }]);
        assert_eq!(projection.text, text);
        let spans = projection.styled_spans();
        assert!(spans
            .iter()
            .all(|span| text.is_char_boundary(span.range.start)
                && text.is_char_boundary(span.range.end)));
        assert!(spans
            .windows(2)
            .all(|pair| pair[0].range.end <= pair[1].range.start));
        assert!(spans.iter().any(|span| span.link && span.hit));
        assert_eq!(
            &text[projection.links[0].range.clone()],
            "https://example.test/中文"
        );
    }
}
