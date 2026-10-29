//! Pure list/detail hover projections; no storage, file probes or image decode.
//! A row's text tooltip and each rendered thumbnail are separate targets.
use super::{Content, ImageSource};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowHoverSource {
    History,
    Favorite,
}

/// Fields already available in ClipboardItemDto/FavorItemDto. For favorites,
/// `display_text` may be empty; title/summary remain their existing projection.
pub struct RowHoverInput<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub summary: &'a str,
    pub display_text: &'a str,
    pub file_paths: &'a [String],
    pub im_rich_segments: &'a [Value],
}

/// Original DTO indices are stable keys; identical paths at distinct positions
/// still produce distinct image targets. Only visible widgets bind their bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoverThumbnailTarget {
    ItemImage,
    FileImage(usize),
    FileMedia(usize),
    ImImage(usize),
    TextImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverThumbnail {
    pub target: HoverThumbnailTarget,
    pub source: ImageSource,
    /// Caller may supply the already-loaded dimensions/size caption. Sources
    /// and whole-file paths stay in the row tooltip, not an unbreakable caption.
    pub meta_line: String,
}
impl HoverThumbnail {
    pub fn content(&self) -> Content {
        if let (HoverThumbnailTarget::FileMedia(_), ImageSource::Local(path)) =
            (self.target, &self.source)
        {
            return Content::Video { path: path.clone() };
        }
        Content::Image {
            source: self.source.clone(),
            meta_line: self.meta_line.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RichPart {
    Text(String),
    /// Local raster path; remote or unsupported references stay text parts.
    Image(String),
}

/// One card for mixed text/images (2026-10-24): the whole row previews it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichCard {
    pub parts: Vec<RichPart>,
    pub images: usize,
}

/// `paths` keeps the original DTO list (including empty entries) so `focus`
/// stays the original index used by the row's file chips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCard {
    pub paths: Vec<String>,
    pub focus: Option<usize>,
}
impl FileCard {
    /// Non-empty entries with their original indices.
    pub fn entries(&self) -> impl Iterator<Item = (usize, &str)> {
        self.paths
            .iter()
            .enumerate()
            .map(|(index, path)| (index, path.trim()))
            .filter(|(_, path)| !path.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowHoverContent {
    /// Always the row text intent; never substitute the first QQ/file image.
    pub row: Content,
    /// Whole-row preview: an image item, a single media/text file, several
    /// files or mixed QQ text/images. The row controller restores `row` when
    /// media previews are disabled.
    pub row_media: Option<Content>,
    pub thumbnails: Vec<HoverThumbnail>,
    /// The complete supplied projection, in its original order. File,
    /// placeholder and unknown nodes/metadata are not flattened or discarded.
    pub im_rich_segments: Vec<Value>,
}
impl RowHoverContent {
    pub fn thumbnail(&self, target: HoverThumbnailTarget) -> Option<&HoverThumbnail> {
        self.thumbnails
            .iter()
            .find(|thumbnail| thumbnail.target == target)
    }
}

pub fn row_hover_content(source: RowHoverSource, input: RowHoverInput<'_>) -> RowHoverContent {
    let row = if input.id.is_empty() {
        Content::Text(first_nonempty([input.summary, input.title, input.display_text]).to_owned())
    } else {
        match source {
            RowHoverSource::History => Content::HistoryText(input.id.to_owned()),
            RowHoverSource::Favorite => Content::FavoriteText(input.id.to_owned()),
        }
    };
    let mut thumbnails = Vec::new();
    let mut add = |target, source| {
        thumbnails.push(HoverThumbnail {
            target,
            source,
            meta_line: String::new(),
        })
    };
    let kind = input.kind.to_ascii_lowercase();
    if is_image_kind(&kind) && !input.id.is_empty() {
        add(
            HoverThumbnailTarget::ItemImage,
            match source {
                RowHoverSource::History => ImageSource::History(input.id.to_owned()),
                RowHoverSource::Favorite => ImageSource::Favorite(input.id.to_owned()),
            },
        );
    }
    if is_file_kind(&kind) {
        for (index, path) in input.file_paths.iter().enumerate() {
            if local_raster_path(path) {
                add(
                    HoverThumbnailTarget::FileImage(index),
                    ImageSource::Local(path.clone()),
                );
            } else if local_video_path(path) {
                add(
                    HoverThumbnailTarget::FileMedia(index),
                    ImageSource::Local(path.clone()),
                );
            }
        }
    }
    for (index, segment) in input.im_rich_segments.iter().enumerate() {
        if segment["kind"].as_str() == Some("image") {
            if let Some(path) = segment["path"]
                .as_str()
                .filter(|path| local_raster_path(path))
            {
                add(
                    HoverThumbnailTarget::ImImage(index),
                    ImageSource::Local(path.to_owned()),
                );
            }
        }
    }
    // Mirrors the existing text-line local image candidate. This is a plan,
    // not an existence assertion; only bind it if that image is actually shown.
    if !is_image_kind(&kind) && !is_file_kind(&kind) {
        let text = first_nonempty([input.display_text, input.title, input.summary]).trim();
        let line = text.lines().next().unwrap_or("").trim();
        if windows_path_line(line) && local_raster_path(line) {
            add(
                HoverThumbnailTarget::TextImage,
                ImageSource::Local(line.to_owned()),
            );
        }
    }
    let files = input
        .file_paths
        .iter()
        .enumerate()
        .filter(|(_, path)| !path.trim().is_empty())
        .collect::<Vec<_>>();
    let row_media = if is_image_kind(&kind) && !input.id.is_empty() {
        thumbnails
            .iter()
            .find(|entry| entry.target == HoverThumbnailTarget::ItemImage)
            .map(HoverThumbnail::content)
    } else if is_file_kind(&kind) && files.len() == 1 {
        let (index, path) = files[0];
        thumbnails
            .iter()
            .find(|entry| {
                matches!(
                    entry.target,
                    HoverThumbnailTarget::FileImage(_) | HoverThumbnailTarget::FileMedia(_)
                )
            })
            .map(HoverThumbnail::content)
            // Any other single file uses the file card too (2026-10-24):
            // text files open on their content, others as a clickable row.
            .or_else(|| {
                Some(Content::Files(FileCard {
                    paths: input.file_paths.to_vec(),
                    focus: local_text_path(path).then_some(index),
                }))
            })
    } else if is_file_kind(&kind) && files.len() > 1 {
        Some(Content::Files(FileCard {
            paths: input.file_paths.to_vec(),
            focus: None,
        }))
    } else {
        rich_card(input.im_rich_segments).map(Content::Rich)
    };
    RowHoverContent {
        row,
        row_media,
        thumbnails,
        im_rich_segments: input.im_rich_segments.to_vec(),
    }
}

/// QQ segments become one card only when at least one local image exists;
/// text-only messages keep the plain tooltip.
fn rich_card(segments: &[Value]) -> Option<RichCard> {
    let mut parts: Vec<RichPart> = Vec::new();
    let mut images = 0;
    let push_text = |parts: &mut Vec<RichPart>, value: &str| {
        if value.is_empty() {
            return;
        }
        if let Some(RichPart::Text(last)) = parts.last_mut() {
            last.push_str(value);
        } else {
            parts.push(RichPart::Text(value.to_owned()));
        }
    };
    for segment in segments {
        let kind = segment["kind"].as_str().unwrap_or_default();
        let path = segment["path"].as_str().unwrap_or_default();
        match kind {
            "image" if local_raster_path(path) => {
                images += 1;
                parts.push(RichPart::Image(path.trim().to_owned()));
            }
            "image" => push_text(&mut parts, "[图片]"),
            "file" => {
                let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
                push_text(&mut parts, &format!("[文件] {name}"));
            }
            _ => push_text(&mut parts, segment["text"].as_str().unwrap_or_default()),
        }
    }
    (images > 0).then_some(RichCard { parts, images })
}

/// Plain text and source files the hover card can preview from a bounded
/// prefix. Filename only; the reader still rejects binary content.
pub fn local_text_path(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() || path.contains("://") {
        return false;
    }
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path).to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "makefile" | "dockerfile" | "license" | "readme" | ".gitignore" | ".gitattributes"
            | ".editorconfig" | ".env"
    ) {
        return true;
    }
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        crate::text_preview::language_for_extension(extension).is_some()
    })
}

/// Same local raster extensions accepted by frontend localImagePath.ts.
/// URIs never become local files merely because their suffix resembles an image.
pub fn local_raster_path(path: &str) -> bool {
    let path = path.trim();
    let lower = path.to_ascii_lowercase();
    if path.is_empty()
        || path.contains("://")
        || lower.starts_with("data:")
        || lower.starts_with("blob:")
    {
        return false;
    }
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp"
        )
    })
}

/// Filename classification only: codec support and file availability are
/// resolved after the hover delay by the native media presenter.
pub fn local_video_path(path: &str) -> bool {
    let path = path.trim();
    let lower = path.to_ascii_lowercase();
    if path.is_empty()
        || path.contains("://")
        || lower.starts_with("data:")
        || lower.starts_with("blob:")
    {
        return false;
    }
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .rsplit_once('.')
        .is_some_and(|(_, extension)| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp4"
                    | "m4v"
                    | "mov"
                    | "mkv"
                    | "webm"
                    | "avi"
                    | "wmv"
                    | "mpeg"
                    | "mpg"
                    | "m2ts"
                    | "ts"
            )
        })
}

fn windows_path_line(line: &str) -> bool {
    if line.encode_utf16().count() > 520 || line.contains(['\r', '\n', '\u{2028}', '\u{2029}']) {
        return false;
    }
    let bytes = line.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        return true;
    }
    let Some(host) = line.strip_prefix("\\\\") else {
        return false;
    };
    let Some(separator) = host.find(['\\', '/']) else {
        return false;
    };
    separator > 0 && !host[..separator].chars().any(char::is_whitespace)
}

fn is_file_kind(kind: &str) -> bool {
    matches!(kind, "fileclipitem" | "file" | "directory")
}
fn is_image_kind(kind: &str) -> bool {
    matches!(kind, "imageclipitem" | "image")
}
fn first_nonempty<'a>(values: impl IntoIterator<Item = &'a str>) -> &'a str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or("")
}
fn text<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}
fn metadata_text(detail: &Value) -> String {
    first_nonempty(
        [
            text(detail, "summary"),
            text(detail, "title"),
            text(detail, "displayText"),
        ]
        .into_iter()
        .flatten(),
    )
    .to_owned()
}

/// This is the existing tooltip presentation, never a paste/editor serialization.
/// Text/HTML/QQ keep explicitly empty original bodies and their whitespace.
pub fn detail_hover_text(detail: &Value) -> String {
    let kind = text(detail, "kind").unwrap_or("").to_ascii_lowercase();
    if is_file_kind(&kind) {
        if let Some(paths) = detail["filePaths"]
            .as_array()
            .filter(|paths| !paths.is_empty())
        {
            return paths
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n");
        }
        return metadata_text(detail);
    }
    if matches!(
        kind.as_str(),
        "stepclipitem" | "quickeractionsteps" | "step"
    ) {
        if let Some(preview) = step_tooltip(&detail["stepPresentation"]) {
            return preview;
        }
        // History detail's summary is already generated by core's existing
        // Quicker step projection; bodyText/rawJson are never rendered as labels.
        return metadata_text(detail);
    }
    if matches!(kind.as_str(), "actionclipitem" | "quickeraction" | "action") {
        return metadata_text(detail);
    }
    if matches!(
        kind.as_str(),
        "" | "textclipitem" | "text" | "htmlclipitem" | "html" | "qqrichtextitem" | "qqrichtext"
    ) {
        return ["bodyText", "displayText", "summary", "title"]
            .iter()
            .find_map(|field| text(detail, field))
            .unwrap_or("")
            .to_owned();
    }
    metadata_text(detail)
}

fn step_tooltip(presentation: &Value) -> Option<String> {
    if let Some(preview) = text(presentation, "previewText").filter(|text| !text.trim().is_empty())
    {
        return Some(preview.to_owned());
    }
    let mut lines = Vec::new();
    for line in presentation["previewLines"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(display) = text(line, "displayText").filter(|text| !text.trim().is_empty()) {
            lines.push(display.to_owned());
            continue;
        }
        let title = first_nonempty(
            [text(line, "title"), text(line, "summary")]
                .into_iter()
                .flatten(),
        );
        if title.is_empty() {
            continue;
        }
        let suffix = text(line, "summarySuffix")
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                text(line, "summary")
                    .filter(|summary| !summary.is_empty() && *summary != title)
                    .map(|summary| format!(": {summary}"))
            })
            .unwrap_or_default();
        lines.push(format!("{title}{suffix}"));
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn input<'a>(kind: &'a str, paths: &'a [String], segments: &'a [Value]) -> RowHoverInput<'a> {
        RowHoverInput {
            id: "original-id",
            kind,
            title: "title",
            summary: "summary",
            display_text: "display",
            file_paths: paths,
            im_rich_segments: segments,
        }
    }
    #[test]
    fn row_and_each_qq_thumbnail_keep_distinct_targets_and_original_segments() {
        let segments = vec![
            json!({"kind":"text","text":"before","unknown":{"token":1}}),
            json!({"kind":"image","path":"C:\\first.png"}),
            json!({"kind":"file","path":"C:\\attached.bin"}),
            json!({"kind":"placeholder","text":"[表情]"}),
            json!({"kind":"image","path":"C:\\second.gif"}),
            json!({"kind":"image","path":"C:\\first.png"}),
        ];
        let plan = row_hover_content(
            RowHoverSource::History,
            input("QQRichTextItem", &[], &segments),
        );
        assert_eq!(plan.row, Content::HistoryText("original-id".into()));
        assert_eq!(plan.im_rich_segments, segments);
        assert_eq!(
            plan.row_media,
            Some(Content::Rich(RichCard {
                parts: vec![
                    RichPart::Text("before".into()),
                    RichPart::Image("C:\\first.png".into()),
                    RichPart::Text("[文件] attached.bin[表情]".into()),
                    RichPart::Image("C:\\second.gif".into()),
                    RichPart::Image("C:\\first.png".into()),
                ],
                images: 3,
            }))
        );
        assert_eq!(
            plan.thumbnails
                .iter()
                .map(|entry| entry.target)
                .collect::<Vec<_>>(),
            vec![
                HoverThumbnailTarget::ImImage(1),
                HoverThumbnailTarget::ImImage(4),
                HoverThumbnailTarget::ImImage(5)
            ]
        );
        assert_eq!(
            plan.thumbnail(HoverThumbnailTarget::ImImage(4))
                .unwrap()
                .source,
            ImageSource::Local("C:\\second.gif".into())
        );
    }
    #[test]
    fn file_images_preserve_original_indices_paths_and_duplicates() {
        let paths = vec![
            "C:\\text.txt".into(),
            "C:\\a.PNG".into(),
            "C:\\b.gif".into(),
            "C:\\a.PNG".into(),
        ];
        let plan = row_hover_content(RowHoverSource::Favorite, input("File", &paths, &[]));
        assert_eq!(plan.row, Content::FavoriteText("original-id".into()));
        assert_eq!(
            plan.row_media,
            Some(Content::Files(FileCard {
                paths: paths.clone(),
                focus: None
            }))
        );
        assert_eq!(
            plan.thumbnails
                .iter()
                .map(|entry| entry.target)
                .collect::<Vec<_>>(),
            vec![
                HoverThumbnailTarget::FileImage(1),
                HoverThumbnailTarget::FileImage(2),
                HoverThumbnailTarget::FileImage(3)
            ]
        );
        assert_eq!(
            plan.thumbnails[2].content(),
            Content::Image {
                source: ImageSource::Local(paths[3].clone()),
                meta_line: String::new()
            }
        );
        assert_eq!(
            detail_hover_text(&json!({"kind":"FileClipItem","filePaths":paths})),
            "C:\\text.txt\nC:\\a.PNG\nC:\\b.gif\nC:\\a.PNG"
        );
    }
    #[test]
    fn single_media_file_previews_the_whole_row_and_keeps_its_text_fallback() {
        for source in [RowHoverSource::History, RowHoverSource::Favorite] {
            let paths = vec![String::new(), " C:\\media clip.MP4 ".to_owned()];
            let plan = row_hover_content(source, input("FileClipItem", &paths, &[]));
            assert_eq!(
                plan.row_media,
                Some(Content::Video {
                    path: paths[1].clone()
                })
            );
            assert_eq!(
                plan.thumbnails[0].target,
                HoverThumbnailTarget::FileMedia(1)
            );
            assert!(matches!(
                plan.row,
                Content::HistoryText(_) | Content::FavoriteText(_)
            ));

            let paths = vec!["C:\\animated.gif".to_owned()];
            let plan = row_hover_content(source, input("FileClipItem", &paths, &[]));
            assert_eq!(
                plan.row_media,
                Some(Content::Image {
                    source: ImageSource::Local(paths[0].clone()),
                    meta_line: String::new(),
                })
            );
        }
    }
    #[test]
    fn mixed_media_chips_keep_original_indices_without_substituting_the_row() {
        let paths = vec![
            String::new(),
            "C:\\notes.txt".into(),
            "C:\\a.GIF".into(),
            "C:\\clip.webm".into(),
            "C:\\clip.webm".into(),
            "https://example.test/movie.mp4".into(),
        ];
        let plan = row_hover_content(RowHoverSource::History, input("FileClipItem", &paths, &[]));
        let Some(Content::Files(card)) = &plan.row_media else {
            panic!("several files use the file card");
        };
        assert_eq!(card.focus, None);
        assert_eq!(card.entries().map(|(index, _)| index).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            plan.thumbnails
                .iter()
                .map(|entry| entry.target)
                .collect::<Vec<_>>(),
            vec![
                HoverThumbnailTarget::FileImage(2),
                HoverThumbnailTarget::FileMedia(3),
                HoverThumbnailTarget::FileMedia(4)
            ]
        );
        assert_eq!(
            plan.thumbnails[2].content(),
            Content::Video {
                path: paths[4].clone()
            }
        );
        assert_eq!(paths[0], "");
    }
    #[test]
    fn video_filename_detection_is_local_and_does_not_probe_files() {
        for extension in [
            "mp4", "M4V", "mov", "mkv", "webm", "avi", "wmv", "mpeg", "mpg", "m2ts", "ts",
        ] {
            assert!(local_video_path(&format!("C:\\not-probed.{extension}")));
        }
        for path in [
            "https://example.test/movie.mp4",
            "file:///C:/movie.mp4",
            "data:video/movie.mp4",
            "blob:movie.mp4",
            "C:\\movie.mp4.txt",
            "",
        ] {
            assert!(!local_video_path(path), "accepted {path}");
        }
    }
    #[test]
    fn explicit_text_html_and_qq_bodies_keep_empty_and_whitespace() {
        for kind in [
            "TextClipItem",
            "HtmlClipItem",
            "QQRichTextItem",
            "Text",
            "Html",
            "QQRichText",
        ] {
            assert_eq!(
                detail_hover_text(&json!({"kind":kind,"bodyText":"","summary":"fallback"})),
                ""
            );
            assert_eq!(
                detail_hover_text(
                    &json!({"kind":kind,"bodyText":" \r\n\t原文 ","summary":"fallback"})
                ),
                " \r\n\t原文 "
            );
        }
    }
    #[test]
    fn step_uses_existing_presentation_and_never_serialized_body() {
        let body = "{\"Steps\":[{\"raw\":true}]}";
        assert_eq!(
            detail_hover_text(
                &json!({"kind":"StepClipItem","bodyText":body,"stepPresentation":{
            "previewText":"Run script\n… 共 8 步","previewLines":[{"displayText":"unused"}]}})
            ),
            "Run script\n… 共 8 步"
        );
        assert_eq!(
            detail_hover_text(
                &json!({"kind":"QuickerActionSteps","bodyText":body,"stepPresentation":{
            "previewLines":[{"displayText":"First (disabled)"},{"title":"Second","summarySuffix":": value"}]}})
            ),
            "First (disabled)\nSecond: value"
        );
        assert_eq!(
            detail_hover_text(
                &json!({"kind":"StepClipItem","bodyText":body,"rawJson":body,"summary":"Existing offline step projection"})
            ),
            "Existing offline step projection"
        );
    }
    #[test]
    fn action_uses_dto_metadata_not_raw_json_or_body() {
        for kind in ["ActionClipItem", "QuickerAction"] {
            assert_eq!(
                detail_hover_text(&json!({"kind":kind,"bodyText":"{raw}","rawJson":"{raw}",
                "summary":"动作：Example\r\n参数：one\r\n","title":"title"})),
                "动作：Example\r\n参数：one\r\n"
            );
            assert_eq!(
                detail_hover_text(
                    &json!({"kind":kind,"bodyText":"{raw}","summary":"","title":"Visible action"})
                ),
                "Visible action"
            );
        }
    }
    #[test]
    fn unsupported_image_references_remain_in_segments_without_local_thumbnail() {
        let segments = vec![
            json!({"kind":"image","path":"https://example.test/remote.png"}),
            json!({"kind":"image","path":"file:///C:/example.png"}),
            json!({"kind":"image","path":"C:\\supported.webp"}),
        ];
        let plan = row_hover_content(
            RowHoverSource::Favorite,
            input("QQRichText", &[], &segments),
        );
        assert_eq!(plan.im_rich_segments, segments);
        assert_eq!(plan.thumbnails.len(), 1);
        assert_eq!(plan.thumbnails[0].target, HoverThumbnailTarget::ImImage(2));
    }
    #[test]
    fn item_image_has_explicit_target_and_text_path_is_only_a_plan() {
        let plan = row_hover_content(RowHoverSource::History, input("ImageClipItem", &[], &[]));
        assert_eq!(plan.thumbnails[0].target, HoverThumbnailTarget::ItemImage);
        assert_eq!(plan.row_media, Some(plan.thumbnails[0].content()));
        assert_eq!(
            plan.thumbnails[0].source,
            ImageSource::History("original-id".into())
        );
        let mut request = input("TextClipItem", &[], &[]);
        request.display_text = " C:\\not-probed.gif\r\ncaption";
        let plan = row_hover_content(RowHoverSource::History, request);
        assert_eq!(plan.thumbnails[0].target, HoverThumbnailTarget::TextImage);
        assert_eq!(
            plan.thumbnails[0].source,
            ImageSource::Local("C:\\not-probed.gif".into())
        );
    }
    #[test]
    fn single_text_file_previews_its_content_and_other_files_use_a_file_row() {
        let paths = vec!["D:\\src\\main.rs".to_owned()];
        let plan = row_hover_content(RowHoverSource::History, input("FileClipItem", &paths, &[]));
        assert_eq!(
            plan.row_media,
            Some(Content::Files(FileCard { paths: paths.clone(), focus: Some(0) }))
        );
        for name in ["D:\\setup.exe", "D:\\archive.zip"] {
            let paths = vec![name.to_owned()];
            let plan = row_hover_content(RowHoverSource::History, input("FileClipItem", &paths, &[]));
            assert_eq!(plan.row_media, Some(Content::Files(FileCard { paths, focus: None })));
        }
        assert!(local_text_path("C:\\repo\\Dockerfile"));
        assert!(local_text_path("notes.TXT"));
        assert!(!local_text_path("https://example.test/a.txt"));
    }
    #[test]
    fn text_only_qq_messages_keep_the_plain_tooltip() {
        let segments = vec![
            json!({"kind":"text","text":"hello"}),
            json!({"kind":"image","path":"https://example.test/remote.png"}),
        ];
        let plan = row_hover_content(RowHoverSource::History, input("QQRichTextItem", &[], &segments));
        assert_eq!(plan.row_media, None);
    }
}
