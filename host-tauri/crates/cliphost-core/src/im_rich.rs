//! QQ / WeChat rich-edit envelopes aligned with C# `QQRichTextParser` + `ImRichSegmentMapper`.

use std::path::Path;

use serde::Serialize;

#[path = "im_rich_document.rs"]
pub mod document;

use document::{ImRichDocument, RichNodeKind, RichOrigin};

const QQ_OPEN: &str = "<QQRichEditFormat>";
const QQ_CLOSE: &str = "</QQRichEditFormat>";
const WECHAT_OPEN: &str = "<WeChatRichEditFormat>";
const WECHAT_CLOSE: &str = "</WeChatRichEditFormat>";

const QQ_TYPE_TEXT: i32 = 0;
const QQ_TYPE_IMAGE_EMOJI: i32 = 1;
const QQ_TYPE_IMAGE: i32 = 3;
const QQ_TYPE_FILE: i32 = 4;
const WECHAT_TYPE_TEXT: i32 = 0;
const WECHAT_TYPE_IMAGE: i32 = 1;
const WECHAT_TYPE_FILE: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImRichOrigin {
    Unknown,
    Qq,
    WeChat,
    Wework,
}

impl ImRichOrigin {
    pub fn as_api_token(self) -> &'static str {
        match self {
            Self::Qq => "qq",
            Self::WeChat => "wechat",
            Self::Wework => "wework",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ImRichSegmentKind {
    Text,
    Image,
    File,
    Placeholder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImRichSegment {
    pub kind: ImRichSegmentKind,
    pub text: String,
    pub path: String,
}

pub fn extract_envelope(raw: &str) -> Option<&str> {
    let document = ImRichDocument::parse(raw, RichOrigin::Unknown).ok()?;
    Some(&raw[document.envelope_range()])
}

pub fn contains_im_rich_envelope(raw: &str) -> bool {
    extract_envelope(raw).is_some()
}

pub fn uses_wechat_envelope(raw: &str) -> bool {
    extract_envelope(raw)
        .map(|e| e.starts_with("<WeChatRichEditFormat"))
        .unwrap_or(false)
}

pub fn origin_from_process(name: &str) -> ImRichOrigin {
    let stem = name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    match stem.to_ascii_lowercase().as_str() {
        "qq" | "tim" => ImRichOrigin::Qq,
        "wechat" | "weixin" => ImRichOrigin::WeChat,
        "wxwork" => ImRichOrigin::Wework,
        _ => ImRichOrigin::Unknown,
    }
}

pub fn guess_origin_from_xml(raw: &str) -> ImRichOrigin {
    let Ok(document) = ImRichDocument::parse(raw, RichOrigin::Unknown) else {
        return ImRichOrigin::Unknown;
    };
    let envelope = &raw[document.envelope_range()];
    // Only real Info metadata is a source hint; text/CDATA/comments containing
    // '<Info' must not make a WeChat payload use QQ's image/file type codes.
    if document
        .nodes()
        .iter()
        .any(|node| node.is_metadata() && node.raw_xml().starts_with("<Info"))
    {
        return ImRichOrigin::Qq;
    }
    if envelope.starts_with("<WeChatRichEditFormat") {
        return ImRichOrigin::WeChat;
    }
    if document
        .nodes()
        .iter()
        .any(|node| node.kind() == RichNodeKind::Image && !node.path().is_empty())
    {
        return match document.origin() {
            RichOrigin::Wechat | RichOrigin::Wework => ImRichOrigin::WeChat,
            _ => ImRichOrigin::Qq,
        };
    }
    ImRichOrigin::Unknown
}

pub fn resolve_origin(process_name: &str, raw: &str) -> ImRichOrigin {
    let from_process = origin_from_process(process_name);
    if from_process != ImRichOrigin::Unknown {
        return from_process;
    }
    let from_xml = guess_origin_from_xml(raw);
    if from_xml != ImRichOrigin::Unknown {
        return from_xml;
    }
    // C# `ImRichOriginDetector.Resolve`: no process/xml hint → WeChat envelope else QQ (never Unknown).
    if uses_wechat_envelope(raw) {
        ImRichOrigin::WeChat
    } else {
        ImRichOrigin::Qq
    }
}

pub const LIST_PREVIEW_SEGMENT_LIMIT: usize = 12;
pub const LIST_PREVIEW_IMAGE_PATH_LIMIT: usize = 4;

/// C# `ImRichSegmentMapper.LimitForListPreview`.
pub fn limit_for_list_preview(segments: &[ImRichSegment]) -> Vec<ImRichSegment> {
    segments
        .iter()
        .take(LIST_PREVIEW_SEGMENT_LIMIT)
        .cloned()
        .collect()
}

/// C# `ImRichSegmentMapper.CollectImagePaths` (first-occurrence OrdinalIgnoreCase).
pub fn collect_image_paths(segments: &[ImRichSegment]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for segment in segments {
        if segment.kind != ImRichSegmentKind::Image {
            continue;
        }
        if segment.path.trim().is_empty() {
            continue;
        }
        if seen.insert(segment.path.to_ascii_lowercase()) {
            out.push(segment.path.clone());
        }
    }
    out
}

/// C# `ImRichSegmentMapper.CollectImagePathsForList`.
pub fn collect_image_paths_for_list(segments: &[ImRichSegment]) -> Vec<String> {
    collect_image_paths(segments)
        .into_iter()
        .take(LIST_PREVIEW_IMAGE_PATH_LIMIT)
        .collect()
}

/// C# `ImRichSegmentDtoMapper.ToDto`.
pub fn segment_to_dto(segment: &ImRichSegment) -> serde_json::Value {
    let kind = match segment.kind {
        ImRichSegmentKind::Image => "image",
        ImRichSegmentKind::File => "file",
        ImRichSegmentKind::Placeholder => "placeholder",
        ImRichSegmentKind::Text => "text",
    };
    let text = matches!(
        segment.kind,
        ImRichSegmentKind::Text | ImRichSegmentKind::Placeholder
    )
    .then(|| segment.text.clone());
    let path = matches!(
        segment.kind,
        ImRichSegmentKind::Image | ImRichSegmentKind::File
    )
    .then(|| segment.path.clone());
    serde_json::json!({
        "kind": kind,
        "text": text,
        "path": path,
    })
}

pub fn parse_segments(raw: &str, origin: ImRichOrigin) -> Vec<ImRichSegment> {
    let Ok(document) = ImRichDocument::parse(raw, document_origin(origin)) else {
        return Vec::new();
    };
    document
        .nodes()
        .iter()
        .filter_map(|node| match node.kind() {
            RichNodeKind::Text => Some(text_seg(node.text())),
            RichNodeKind::Image => Some(path_seg(ImRichSegmentKind::Image, node.path())),
            RichNodeKind::File => Some(path_seg(ImRichSegmentKind::File, node.path())),
            RichNodeKind::Placeholder => Some(placeholder_seg(node.text())),
            RichNodeKind::Opaque => None,
        })
        .collect()
}

pub fn build_plain_summary(segments: &[ImRichSegment]) -> String {
    let mut out = String::new();
    for segment in segments {
        match segment.kind {
            ImRichSegmentKind::Text | ImRichSegmentKind::Placeholder => out.push_str(&segment.text),
            ImRichSegmentKind::Image => out.push_str("[图片]"),
            ImRichSegmentKind::File => out.push_str("[文件]"),
        }
    }
    out
}

fn text_seg(text: impl Into<String>) -> ImRichSegment {
    ImRichSegment {
        kind: ImRichSegmentKind::Text,
        text: text.into(),
        path: String::new(),
    }
}

fn placeholder_seg(text: impl Into<String>) -> ImRichSegment {
    ImRichSegment {
        kind: ImRichSegmentKind::Placeholder,
        text: text.into(),
        path: String::new(),
    }
}

fn path_seg(kind: ImRichSegmentKind, path: impl Into<String>) -> ImRichSegment {
    ImRichSegment {
        kind,
        text: String::new(),
        path: path.into(),
    }
}

fn html_unescape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find('&') {
        result.push_str(&remaining[..start]);
        remaining = &remaining[start..];
        let decoded = remaining
            .as_bytes()
            .iter()
            .take(32)
            .position(|byte| *byte == b';')
            .and_then(|end| {
                let entity = &remaining[1..end];
                let character = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|number| u32::from_str_radix(number, 16).ok())
                        .or_else(|| {
                            entity
                                .strip_prefix('#')
                                .and_then(|number| number.parse::<u32>().ok())
                        })
                        .and_then(char::from_u32),
                }?;
                Some((end + 1, character))
            });
        if let Some((length, character)) = decoded {
            result.push(character);
            remaining = &remaining[length..];
        } else {
            result.push('&');
            remaining = &remaining[1..];
        }
    }
    result.push_str(remaining);
    result
}

fn xml_attribute_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn uses_wechat_codes(origin: ImRichOrigin) -> bool {
    matches!(origin, ImRichOrigin::WeChat | ImRichOrigin::Wework)
}

/// C# `ClipboardDataObjectExtensions.SetQQImage`.
pub fn qq_image_envelope(path: &str) -> String {
    let path = xml_attribute_escape(path);
    format!(
        r#"<QQRichEditFormat><Info version="1001"></Info><EditElement type="{QQ_TYPE_IMAGE_EMOJI}" filepath="{path}"></EditElement></QQRichEditFormat>"#
    )
}

pub fn build_envelope(
    segments: &[ImRichSegment],
    origin: ImRichOrigin,
    newline_between: bool,
) -> String {
    let wechat = uses_wechat_codes(origin);
    let fragments: Vec<String> = segments
        .iter()
        .map(|segment| segment_to_edit_element(segment, wechat))
        .collect();
    let sep = if newline_between {
        edit_element_xml(QQ_TYPE_TEXT, Some("\r\n"), None)
    } else {
        String::new()
    };
    let (open, close) = if wechat {
        (WECHAT_OPEN, WECHAT_CLOSE)
    } else {
        (QQ_OPEN, QQ_CLOSE)
    };
    format!("{open}{}{close}", fragments.join(&sep))
}

pub fn rewrite_for_origin(raw: &str, origin: ImRichOrigin) -> String {
    rewrite_for_record_origin(raw, "", origin)
}

/// Preserve the recorded source-process fallback for legacy QQ wrappers that
/// contain WeChat type codes. The target still chooses the clipboard format.
pub fn rewrite_for_record_origin(raw: &str, source_process: &str, target: ImRichOrigin) -> String {
    if raw.trim().is_empty() {
        return raw.to_owned();
    }
    let source = resolve_origin(source_process, raw);
    let mut document = match ImRichDocument::parse(raw, document_origin(source)) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(%error, "IM rich output kept unparseable complete original payload");
            return raw.to_owned();
        }
    };
    let source_family = document.origin().as_token();
    match document.rewrite_origin(document_origin(target)) {
        Ok(report) => {
            if report.preserved_unmapped_nodes > 0 {
                tracing::warn!(
                    source = source_family,
                    target = target.as_api_token(),
                    preserved_objects = report.preserved_unmapped_nodes,
                    "IM rich cross-family output retained objects without a known type mapping"
                );
            }
            document.serialize()
        }
        Err(error) => {
            tracing::warn!(%error, "IM rich output conversion failed; complete original payload retained");
            raw.to_owned()
        }
    }
}

/// C# `ImRichClipboardPostProcessor.PersistQqRichTextImages`.
pub fn persist_images(raw: &str, origin: ImRichOrigin, blobs: &Path) -> String {
    persist_images_with(raw, origin, |src| copy_image_to_library(src, blobs))
}

/// C# favorite-path `PersistQqRichTextImages` via `CopyImageToLibraryFolder`.
pub fn persist_images_to_library_folder(
    raw: &str,
    origin: ImRichOrigin,
    library_root: &Path,
) -> String {
    persist_images_with(raw, origin, |src| {
        let copied = copy_image_to_library_folder(src, library_root);
        (!copied.is_empty()).then_some(copied)
    })
}

/// Resource preparation for a new reliable favorite/edit. Unlike the legacy
/// best-effort wrapper, any known local image failure rejects the operation.
pub fn persist_images_to_library_folder_checked(
    raw: &str, origin: ImRichOrigin, library_root: &Path,
) -> Result<String, String> {
    if raw.trim().is_empty() { return Ok(raw.to_string()); }
    let mut document = ImRichDocument::parse(raw, document_origin(origin))
        .map_err(|_| "invalid_im_resource_document".to_string())?;
    let images: Vec<_> = document.nodes().iter()
        .filter(|node| node.kind() == RichNodeKind::Image && !node.path().is_empty())
        .map(|node| (node.id(), node.path().to_string())).collect();
    for (id, source) in images {
        let copied = copy_image_to_library_folder_checked(&source, library_root)?;
        if !copied.eq_ignore_ascii_case(&source) {
            document.set_path(id, &copied).map_err(|_| "invalid_im_resource_destination".to_string())?;
        }
    }
    Ok(document.serialize())
}

fn persist_images_with(
    raw: &str,
    origin: ImRichOrigin,
    copy: impl Fn(&str) -> Option<String>,
) -> String {
    if raw.trim().is_empty() {
        return raw.to_string();
    }
    let mut document = match ImRichDocument::parse(raw, document_origin(origin)) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(%error, "IM rich image persistence kept unparseable original XML");
            return raw.to_string();
        }
    };
    // Stable node IDs let set_path patch just one attribute value, including
    // its original quote. Metadata, unknown XML, CDATA and prefix/suffix never
    // pass through the simplified preview DTO or a DOM serializer.
    let images: Vec<_> = document
        .nodes()
        .iter()
        .filter(|node| node.kind() == RichNodeKind::Image && !node.path().is_empty())
        .map(|node| (node.id(), node.path().to_string()))
        .collect();
    for (id, path) in images {
        let Some(copied) = copy(&path).filter(|path| !path.trim().is_empty()) else {
            tracing::warn!(source=%path, "IM rich image copy failed; original path and XML retained");
            continue;
        };
        if copied.eq_ignore_ascii_case(&path) {
            continue;
        }
        if let Err(error) = document.set_path(id, &copied) {
            tracing::warn!(source=%path, %error,
                "IM rich image destination could not be encoded; original image reference retained");
        }
    }
    // serialize is byte-identical for every untouched part; even no-image or
    // no-path-change documents return the complete raw payload, not an envelope.
    document.serialize()
}

fn document_origin(origin: ImRichOrigin) -> RichOrigin {
    match origin {
        ImRichOrigin::Qq => RichOrigin::Qq,
        ImRichOrigin::WeChat => RichOrigin::Wechat,
        ImRichOrigin::Wework => RichOrigin::Wework,
        ImRichOrigin::Unknown => RichOrigin::Unknown,
    }
}

/// Preserve an image under `data/Image`; a same-name file is never replaced or
/// assumed to contain the incoming image. The checked variant lets callers
/// reject a favorite whose required resource could not be persisted.
pub fn copy_image_to_library_folder(image_source: &str, library_root: &Path) -> String {
    let compatible = || -> Result<String, String> {
        let source = Path::new(image_source);
        if !source.is_file() { return Err("resource_not_regular_file".into()); }
        if source.parent().is_some_and(|parent| path_eq_ignore_case(parent, library_root)) {
            return Ok(image_source.to_owned());
        }
        let name = source.file_name().and_then(|name| name.to_str()).ok_or("invalid_image_filename")?;
        let existing = library_root.join(name);
        if compatible_image_bytes_match(source, &existing) {
            return Ok(existing.to_string_lossy().into_owned());
        }
        crate::storage_core::resources::publish_copy_compatible(source, library_root, name)
            .map(|path| path.to_string_lossy().into_owned())
    };
    match compatible() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "IM rich image persistence failed; original reference retained");
            String::new()
        }
    }
}

/// The legacy best-effort path remains idempotent when an earlier edit already
/// retained these exact bytes. Filename equality alone never authorizes reuse.
/// Read errors mean no reuse; publication still gets its own verified attempt.
fn compatible_image_bytes_match(source: &Path, existing: &Path) -> bool {
    use std::io::Read;
    let compare = || -> Result<bool, std::io::Error> {
        let open = |path: &Path| {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(1);
            }
            options.open(path)
        };
        let mut incoming = open(source)?;
        let mut retained = open(existing)?;
        let source_meta = incoming.metadata()?;
        let retained_meta = retained.metadata()?;
        if !source_meta.is_file() || !retained_meta.is_file() || source_meta.len() != retained_meta.len() {
            return Ok(false);
        }
        let mut remaining = source_meta.len();
        let mut left = [0u8; 65536];
        let mut right = [0u8; 65536];
        while remaining > 0 {
            let count = remaining.min(left.len() as u64) as usize;
            incoming.read_exact(&mut left[..count])?;
            retained.read_exact(&mut right[..count])?;
            if left[..count] != right[..count] { return Ok(false); }
            remaining -= count as u64;
        }
        let mut end = [0u8; 1];
        Ok(incoming.read(&mut end)? == 0 && retained.read(&mut end)? == 0)
    };
    compare().unwrap_or(false)
}

pub fn copy_image_to_library_folder_checked(image_source: &str, library_root: &Path) -> Result<String, String> {
    let source = Path::new(image_source);
    crate::storage_core::resources::validate_local_file(source)?;
    if source
        .parent()
        .is_some_and(|parent| path_eq_ignore_case(parent, library_root))
    {
        return Ok(image_source.to_string());
    }
    let name = source.file_name().and_then(|name| name.to_str()).ok_or("invalid_image_filename")?;
    crate::storage_core::resources::publish_copy(source, library_root, name)
        .map(|path| path.to_string_lossy().into_owned())
}

fn path_eq_ignore_case(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// C# `ImRichClipboardPostProcessor.TryUpgradeHtmlToQqRich`.
pub fn try_upgrade_html_to_qq(
    html: &str,
    text: &str,
    source_process: &str,
    library: &Path,
) -> Option<(String, String)> {
    let origin = origin_from_process(source_process);
    if origin == ImRichOrigin::Unknown {
        return None;
    }
    if html.to_ascii_lowercase().find("<img").is_none() {
        return None;
    }
    let segments = parse_html_to_segments(html);
    if !segments.iter().any(|s| s.kind == ImRichSegmentKind::Image) {
        return None;
    }
    let envelope = persist_images_to_library_folder(
        &build_envelope(&segments, origin, false),
        origin,
        library,
    );
    let summary = build_plain_summary(&parse_segments(&envelope, origin));
    let body = if summary.is_empty() {
        text.to_string()
    } else {
        summary
    };
    Some((envelope, body))
}

fn parse_html_to_segments(html: &str) -> Vec<ImRichSegment> {
    let re = regex::Regex::new(r#"(?is)<img\b[^>]*\bsrc\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*>"#)
        .expect("img regex");
    let mut segments = Vec::new();
    let mut last = 0usize;
    for cap in re.captures_iter(html) {
        let m = cap.get(0).expect("full match");
        let before = strip_html_tags(&html[last..m.start()]);
        if !before.trim().is_empty() {
            segments.push(text_seg(before));
        }
        if let Some(path) = resolve_local_image_path(
            cap.get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str())
                .unwrap_or(""),
        ) {
            segments.push(path_seg(ImRichSegmentKind::Image, path));
        }
        last = m.end();
    }
    let tail = strip_html_tags(&html[last..]);
    if !tail.trim().is_empty() {
        segments.push(text_seg(tail));
    }
    segments
}

fn strip_html_tags(fragment: &str) -> String {
    if fragment.is_empty() {
        return String::new();
    }
    let stripped = regex::Regex::new(r"<[^>]+>")
        .expect("tag regex")
        .replace_all(fragment, "");
    html_unescape(&stripped)
        .replace("\r\n", "\n")
        .replace('\n', " ")
        .trim()
        .to_string()
}

fn resolve_local_image_path(src: &str) -> Option<String> {
    let trimmed = html_unescape(src).trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("file:") {
        let path = url::Url::parse(&trimmed).ok()?.to_file_path().ok()?;
        return path.is_file().then(|| path.to_string_lossy().into_owned());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("data:") {
        return None;
    }
    let path = Path::new(&trimmed);
    path.is_file().then_some(trimmed)
}

fn copy_image_to_library(src: &str, blobs: &Path) -> Option<String> {
    let src_path = Path::new(src);
    if !src_path.is_file() { return None; }
    if already_in_blobs(src_path, blobs) {
        return Some(src.to_string());
    }
    let ext = src_path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("png");
    let name = format!("{}.{ext}", uuid::Uuid::new_v4().simple());
    let dest = crate::storage_core::resources::publish_copy_compatible(src_path, blobs, &name).ok()?;
    Some(dest.to_string_lossy().into_owned())
}

fn already_in_blobs(path: &Path, blobs: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(blobs) = blobs.canonicalize() else {
        return false;
    };
    path.starts_with(blobs)
}

fn segment_to_edit_element(segment: &ImRichSegment, wechat: bool) -> String {
    match segment.kind {
        ImRichSegmentKind::Text | ImRichSegmentKind::Placeholder => edit_element_xml(
            if wechat {
                WECHAT_TYPE_TEXT
            } else {
                QQ_TYPE_TEXT
            },
            Some(&segment.text),
            None,
        ),
        ImRichSegmentKind::Image => edit_element_xml(
            if wechat {
                WECHAT_TYPE_IMAGE
            } else {
                QQ_TYPE_IMAGE
            },
            None,
            Some(&segment.path),
        ),
        ImRichSegmentKind::File => edit_element_xml(
            if wechat {
                WECHAT_TYPE_FILE
            } else {
                QQ_TYPE_FILE
            },
            None,
            Some(&segment.path),
        ),
    }
}

fn edit_element_xml(type_code: i32, data: Option<&str>, filepath: Option<&str>) -> String {
    let inner = data
        .filter(|s| !s.is_empty())
        .map(|s| format!("<![CDATA[{s}]]>"))
        .unwrap_or_default();
    let path_attr = filepath
        .filter(|s| !s.is_empty())
        .map(|s| format!(" filepath=\"{}\"", xml_attribute_escape(s)))
        .unwrap_or_default();
    format!("<EditElement type=\"{type_code}\"{path_attr}>{inner}</EditElement>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_rich_root() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join(".tmp")
            .join("im-rich-lossless");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(root)
            .unwrap()
    }

    fn escaped_attribute(value: &str, quote: char) -> String {
        let escaped = value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\r', "&#13;")
            .replace('\n', "&#10;")
            .replace('\t', "&#9;");
        if quote == '\'' {
            escaped.replace('\'', "&apos;")
        } else {
            escaped.replace('"', "&quot;")
        }
    }

    fn lossless_payload(origin: ImRichOrigin, first: &str, second: &str) -> String {
        let (root, image_type, file_type) = if uses_wechat_codes(origin) {
            ("WeChatRichEditFormat", 1, 3)
        } else {
            ("QQRichEditFormat", 3, 4)
        };
        let first = escaped_attribute(first, '\'');
        let second = escaped_attribute(second, '"');
        format!(
            r#"prefix-中文<?fixture preserve='yes'?>
<{root} vendor='unchanged &amp; value'><Info version='1001' vendor="opaque"/>
<!-- <EditElement type="3" filepath="comment-fake.png"/> -->
<EditElement type = '0' style="vendor"><![CDATA[text <Info/> </{root}> <EditElement type="3" filepath="cdata-fake.png"/>]]></EditElement>
<Vendor flag='do-not-flatten'><Unknown p="&amp;"><![CDATA[opaque]]></Unknown><EditElement type="3" filepath="vendor-fake.png"/></Vendor>
<EditElement type = '&#{numeric};' vendor='alpha' filepath = '{first}' other="keep &amp; this"><![CDATA[image vendor body]]></EditElement>
<EditElement filepath="{second}" data-vendor='&quot;' type="{image_type}" />
<EditElement type='{file_type}' filepath='file-reference.txt' vendor="file is not an image"/>
</{root}>suffix-原样"#,
            numeric = image_type + 48
        )
    }

    #[test]
    fn native_output_same_family_keeps_complete_raw_instead_of_preview_segments() {
        for (origin, target) in [
            (ImRichOrigin::Qq, ImRichOrigin::Qq),
            (ImRichOrigin::WeChat, ImRichOrigin::WeChat),
            (ImRichOrigin::Wework, ImRichOrigin::WeChat),
            (ImRichOrigin::WeChat, ImRichOrigin::Wework),
        ] {
            let raw = lossless_payload(origin, "first & 图.png", "second.png");
            assert_eq!(rewrite_for_origin(&raw, target), raw);
        }
        let legacy = "prefix<QQRichEditFormat vendor='keep'><Info version='1001'/><EditElement type='1' filepath='image.png'/><EditElement type='3' filepath='file.txt'/><Vendor><Blob/></Vendor></QQRichEditFormat>suffix";
        assert_eq!(
            rewrite_for_record_origin(legacy, "wxwork.exe", ImRichOrigin::WeChat),
            legacy
        );
    }

    #[test]
    fn native_output_cross_family_preserves_metadata_unknown_nodes_and_cdata() {
        for (
            source,
            target,
            source_root,
            target_root,
            source_image,
            target_image,
            source_file,
            target_file,
        ) in [
            (
                ImRichOrigin::Qq,
                ImRichOrigin::WeChat,
                "QQRichEditFormat",
                "WeChatRichEditFormat",
                3,
                1,
                4,
                3,
            ),
            (
                ImRichOrigin::WeChat,
                ImRichOrigin::Qq,
                "WeChatRichEditFormat",
                "QQRichEditFormat",
                1,
                3,
                3,
                4,
            ),
        ] {
            let raw = lossless_payload(source, "first & 图.png", "second.png");
            let expected = raw
                .replacen(
                    &format!("<{source_root} vendor="),
                    &format!("<{target_root} vendor="),
                    1,
                )
                .replacen(
                    &format!("</{source_root}>suffix-原样"),
                    &format!("</{target_root}>suffix-原样"),
                    1,
                )
                .replacen(
                    &format!("type = '&#{};'", source_image + 48),
                    &format!("type = '{target_image}'"),
                    1,
                )
                .replacen(
                    &format!("data-vendor='&quot;' type=\"{source_image}\""),
                    &format!("data-vendor='&quot;' type=\"{target_image}\""),
                    1,
                )
                .replacen(
                    &format!("type='{source_file}' filepath='file-reference.txt'"),
                    &format!("type='{target_file}' filepath='file-reference.txt'"),
                    1,
                );
            assert_eq!(rewrite_for_origin(&raw, target), expected);
        }
    }

    #[test]
    fn native_output_retains_malformed_xml_and_unmapped_placeholders() {
        for raw in [
            "prefix<QQRichEditFormat><EditElement type='0'><![CDATA[broken</QQRichEditFormat>suffix",
            "prefix<QQRichEditFormat><EditElement type='3' filepath='image.png'></QQRichEditFormat>suffix",
            "prefix-without-an-envelope <Vendor/> suffix",
        ] {
            for target in [ImRichOrigin::Qq, ImRichOrigin::WeChat, ImRichOrigin::Unknown] {
                assert_eq!(rewrite_for_origin(raw, target), raw);
            }
        }
        let raw = "prefix<QQRichEditFormat><Info version='1001'/><EditElement type='2' face='qq-face'/><EditElement type='6' fileid='face-9'><Vendor><![CDATA[bytes]]></Vendor></EditElement><EditElement type='99'><Binary keep='raw'/></EditElement></QQRichEditFormat>suffix";
        assert_eq!(
            rewrite_for_origin(raw, ImRichOrigin::WeChat),
            raw.replacen("<QQRichEditFormat>", "<WeChatRichEditFormat>", 1)
                .replacen(
                    "</QQRichEditFormat>suffix",
                    "</WeChatRichEditFormat>suffix",
                    1
                )
        );
        let no_info = "prefix<QQRichEditFormat vendor='legacy'><EditElement type='1' filepath='image.png'/></QQRichEditFormat>suffix";
        assert_eq!(rewrite_for_origin(no_info, ImRichOrigin::WeChat), no_info);
        assert_eq!(
            rewrite_for_origin(no_info, ImRichOrigin::Qq),
            no_info.replacen("type='1'", "type='3'", 1)
        );
    }

    #[test]
    fn native_original_record_ole_payload_preserves_raw_and_cross_family_types() {
        let dir = isolated_rich_root();
        let image = dir.path().join("owned & 图片.png");
        std::fs::write(&image, b"original referenced bytes").unwrap();
        let raw = lossless_payload(ImRichOrigin::Qq, &image.to_string_lossy(), "second.png");
        let mut item = crate::ClipRecord::new_text("original Unicode 🦀", "qq.exe");
        item.kind = crate::ClipKind::QQRichTextItem;
        item.qq_rich_text = Some(raw.clone());
        for (target_process, origin) in [
            ("qq.exe", ImRichOrigin::Qq),
            ("wechat.exe", ImRichOrigin::WeChat),
            ("wxwork.exe", ImRichOrigin::Wework),
        ] {
            let payload = crate::ole_drag::payload_from_items(
                std::slice::from_ref(&item),
                false,
                false,
                false,
                target_process,
            ).unwrap();
            let expected = rewrite_for_record_origin(&raw, &item.source_process_name, origin);
            if uses_wechat_codes(origin) {
                assert_eq!(payload.wechat_rich.as_deref(), Some(expected.as_str()));
                assert!(payload.qq_rich.is_none());
            } else {
                assert_eq!(payload.qq_rich.as_deref(), Some(raw.as_str()));
                assert!(payload.wechat_rich.is_none());
            }
            assert_eq!(payload.text, "original Unicode 🦀");
            assert_eq!(item.qq_rich_text.as_deref(), Some(raw.as_str()));
        }
        assert_eq!(std::fs::read(&image).unwrap(), b"original referenced bytes");
    }

    #[test]
    fn image_persistence_preserves_complete_raw_without_images_or_path_changes() {
        let raw = r#"prefix<QQRichEditFormat vendor='1'><Info version='1001'/><!-- <EditElement type='3' filepath='fake.png'/> --><EditElement type='0'><![CDATA[<EditElement type='3' filepath='fake.png'/> </QQRichEditFormat>]]></EditElement><Vendor unknown='yes'/></QQRichEditFormat>suffix"#;
        assert_eq!(
            persist_images_with(raw, ImRichOrigin::Qq, |_| panic!(
                "fake images are not copied"
            )),
            raw
        );
        let envelope = extract_envelope(raw).unwrap();
        assert!(envelope.ends_with("<Vendor unknown='yes'/></QQRichEditFormat>"));
        assert!(contains_im_rich_envelope("prefix<QQRichEditFormat/>suffix"));
        for origin in [ImRichOrigin::Qq, ImRichOrigin::WeChat, ImRichOrigin::Wework] {
            let raw = lossless_payload(origin, "first.png", "second.png");
            assert_eq!(
                persist_images_with(&raw, origin, |path| Some(path.to_string())),
                raw
            );
            assert_eq!(persist_images_with(&raw, origin, |_| None), raw);
            assert_eq!(
                persist_images_with(&raw, origin, |_| Some(String::new())),
                raw
            );
        }
    }

    #[test]
    fn image_persistence_changes_only_identified_attribute_values_and_preserves_origin() {
        for (origin, process) in [
            (ImRichOrigin::Qq, "QQ.exe"),
            (ImRichOrigin::WeChat, "WeChat.exe"),
            (ImRichOrigin::Wework, "WXWork.exe"),
        ] {
            let first = "old-one & ' < 图片.png";
            let second = "old-two & \" > image.png";
            let destination = "new-path & ' \" < >\t图.png";
            let raw = lossless_payload(origin, first, second);
            let seen = std::cell::RefCell::new(Vec::new());
            let persisted = persist_images_with(&raw, origin, |source| {
                seen.borrow_mut().push(source.to_string());
                Some(destination.into())
            });
            assert_eq!(*seen.borrow(), vec![first, second]);
            let expected = raw
                .replacen(
                    &format!("filepath = '{}'", escaped_attribute(first, '\'')),
                    &format!("filepath = '{}'", escaped_attribute(destination, '\'')),
                    1,
                )
                .replacen(
                    &format!("filepath=\"{}\"", escaped_attribute(second, '"')),
                    &format!("filepath=\"{}\"", escaped_attribute(destination, '"')),
                    1,
                );
            assert_eq!(persisted, expected);
            assert_eq!(resolve_origin(process, &persisted), origin);
            let segments = parse_segments(&persisted, origin);
            assert_eq!(
                segments
                    .iter()
                    .filter(|segment| segment.kind == ImRichSegmentKind::Image)
                    .map(|segment| segment.path.as_str())
                    .collect::<Vec<_>>(),
                vec![destination, destination]
            );
            assert_eq!(
                segments
                    .iter()
                    .filter(|segment| segment.kind == ImRichSegmentKind::File)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn partial_copy_failure_and_unencodable_destination_keep_failed_objects_verbatim() {
        let raw = lossless_payload(ImRichOrigin::Qq, "first.png", "second.png");
        let persisted = persist_images_with(&raw, ImRichOrigin::Qq, |source| {
            if source == "first.png" {
                Some("Image/copied.png".into())
            } else {
                None
            }
        });
        assert_eq!(
            persisted,
            raw.replacen("filepath = 'first.png'", "filepath = 'Image/copied.png'", 1)
        );
        assert_eq!(
            persist_images_with(&raw, ImRichOrigin::Qq, |_| Some("invalid\0path".into())),
            raw
        );
        let malformed = "prefix<QQRichEditFormat><EditElement type='3' filepath='keep.png'></QQRichEditFormat>suffix";
        assert_eq!(
            persist_images_with(malformed, ImRichOrigin::Qq, |_| panic!(
                "malformed XML is not edited"
            )),
            malformed
        );
    }

    #[test]
    fn real_library_copy_keeps_source_bytes_and_failure_keeps_complete_payload() {
        let dir = isolated_rich_root();
        let source = dir.path().join("source & 中文's.png");
        let original = b"isolated-original-image-bytes";
        std::fs::write(&source, original).unwrap();
        let missing = dir.path().join("missing.png");
        let source_string = source.to_string_lossy();
        let missing_string = missing.to_string_lossy();
        let raw = lossless_payload(ImRichOrigin::Qq, &source_string, &missing_string);
        let library = dir.path().join("Image");
        let destination = library.join(source.file_name().unwrap());
        let persisted = persist_images_to_library_folder(&raw, ImRichOrigin::Qq, &library);
        let expected = raw.replacen(
            &format!("filepath = '{}'", escaped_attribute(&source_string, '\'')),
            &format!(
                "filepath = '{}'",
                escaped_attribute(&destination.to_string_lossy(), '\'')
            ),
            1,
        );
        assert_eq!(persisted, expected);
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read(&destination).unwrap(), original);
        assert_eq!(
            persist_images_to_library_folder(&persisted, ImRichOrigin::Qq, &library),
            persisted
        );
        let blocked_library = dir.path().join("not-a-folder");
        std::fs::write(&blocked_library, b"block folder creation").unwrap();
        assert_eq!(
            persist_images_to_library_folder(&raw, ImRichOrigin::Qq, &blocked_library),
            raw
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }

    #[test]
    fn host_patch_and_favorite_update_persist_lossless_payloads_in_isolated_appstore() {
        crate::clipboard::force_headless(true);
        for (origin, process) in [
            (ImRichOrigin::Qq, "qq.exe"),
            (ImRichOrigin::WeChat, "wechat.exe"),
            (ImRichOrigin::Wework, "wxwork.exe"),
        ] {
            let dir = isolated_rich_root();
            let paths = crate::AppPaths::from_data_root(dir.path().join("host"));
            let host = crate::HostState::open(paths.clone(), 28965).unwrap();
            let source = dir.path().join("source & 中文's.png");
            let original = b"real-host-persistence-original";
            std::fs::write(&source, original).unwrap();
            let missing = dir.path().join("missing.png");
            let raw = lossless_payload(
                origin,
                &source.to_string_lossy(),
                &missing.to_string_lossy(),
            );
            let destination = paths.image_library.join(source.file_name().unwrap());
            let expected = raw.replacen(
                &format!(
                    "filepath = '{}'",
                    escaped_attribute(&source.to_string_lossy(), '\'')
                ),
                &format!(
                    "filepath = '{}'",
                    escaped_attribute(&destination.to_string_lossy(), '\'')
                ),
                1,
            );
            let store = crate::AppStore::open(&paths.db).unwrap();
            let mut item = crate::ClipRecord::new_text("original body", process);
            item.id = "history-lossless".into();
            item.kind = crate::ClipKind::QQRichTextItem;
            item.qq_rich_text = Some("<QQRichEditFormat/>".into());
            store.insert_clips(std::slice::from_ref(&item)).unwrap();
            let mut favorite = crate::FavorRecord::from_clip(&item, None);
            favorite.id = "favorite-lossless".into();
            store.insert_favor(&favorite).unwrap();
            drop(store);
            for favorite in [false, true] {
                let id = if favorite {
                    "favorite-lossless"
                } else {
                    "history-lossless"
                };
                let detail = if favorite {
                    host.favor_get(&serde_json::json!({"id":id})).unwrap()
                } else {
                    host.detail(&serde_json::json!({"id":id})).unwrap()
                };
                let request = serde_json::json!({"id":id,"qqRichText":raw,"expectedRevision":detail["revision"]});
                let saved = if favorite {
                    host.favor_update(&request).unwrap()
                } else {
                    host.patch_item(&request).unwrap()
                };
                assert_eq!(saved["qqRichText"], expected);
                assert_eq!(saved["imRichOrigin"], origin.as_api_token());
                let again = serde_json::json!({"id":id,"qqRichText":expected,"expectedRevision":saved["revision"]});
                let saved_again = if favorite {
                    host.favor_update(&again).unwrap()
                } else {
                    host.patch_item(&again).unwrap()
                };
                assert_eq!(saved_again["qqRichText"], expected);
                let persisted = crate::AppStore::open(&paths.db).unwrap();
                let actual = if favorite {
                    persisted.get_favor(id).unwrap().qq_rich_text
                } else {
                    persisted.get_clip(id).unwrap().qq_rich_text
                };
                assert_eq!(actual.as_deref(), Some(expected.as_str()));
                assert_eq!(std::fs::read(&source).unwrap(), original);
                assert_eq!(std::fs::read(&destination).unwrap(), original);
            }
        }
    }

    #[test]
    fn escaped_rich_paths_and_text_decode_once_and_round_trip() {
        let path = r#"D:\图片 & 资料\quote".png"#;
        let raw = qq_image_envelope(path);
        assert!(raw.contains("&amp;") && raw.contains("&quot;"));
        let segments = parse_segments(&raw, ImRichOrigin::Qq);
        assert_eq!(segments[0].path, path);
        let rebuilt = build_envelope(&segments, ImRichOrigin::Qq, false);
        assert_eq!(parse_segments(&rebuilt, ImRichOrigin::Qq), segments);
        let self_closing = raw.replace("></EditElement>", "/>");
        assert_eq!(parse_segments(&self_closing, ImRichOrigin::Qq), segments);
        let numeric = r#"<QQRichEditFormat><EditElement type="3" filepath="D:\&#20013;&#x6587; &amp;lt;.png"></EditElement><EditElement type="0">&amp;lt;</EditElement></QQRichEditFormat>"#;
        let segments = parse_segments(numeric, ImRichOrigin::Qq);
        assert_eq!(segments[0].path, r"D:\中文 &lt;.png");
        assert_eq!(segments[1].text, "&lt;");
    }

    #[test]
    fn rich_html_file_uri_decodes_unicode_spaces_and_ampersands() {
        let temporary = isolated_rich_root();
        let path = temporary.path().join("图片 & file.png");
        std::fs::write(&path, b"fixture").unwrap();
        let uri = url::Url::from_file_path(&path).unwrap();
        let segments = parse_html_to_segments(&format!(r#"<img src="{uri}">"#));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].path, path.to_string_lossy());
    }

    #[test]
    fn extracts_qq_envelope_and_summary() {
        let raw = r#"noise<QQRichEditFormat><EditElement type="0"><![CDATA[hello]]></EditElement><EditElement type="3" filepath="C:\a.png"></EditElement></QQRichEditFormat>tail"#;
        let envelope = extract_envelope(raw).unwrap();
        assert!(envelope.starts_with(QQ_OPEN));
        let segs = parse_segments(raw, ImRichOrigin::Qq);
        assert_eq!(segs.len(), 2);
        assert_eq!(build_plain_summary(&segs), "hello[图片]");
        assert_eq!(resolve_origin("QQ.exe", raw), ImRichOrigin::Qq);
        assert_eq!(resolve_origin("", ""), ImRichOrigin::Qq);
        assert_eq!(resolve_origin("chrome", "plain"), ImRichOrigin::Qq);
        let many: Vec<ImRichSegment> = (0..15)
            .map(|i| ImRichSegment {
                kind: ImRichSegmentKind::Text,
                text: format!("t{i}"),
                path: String::new(),
            })
            .collect();
        assert_eq!(limit_for_list_preview(&many).len(), 12);
        let images: Vec<ImRichSegment> = (0..6)
            .map(|i| ImRichSegment {
                kind: ImRichSegmentKind::Image,
                text: String::new(),
                path: format!(r"C:\img{i}.png"),
            })
            .chain(std::iter::once(ImRichSegment {
                kind: ImRichSegmentKind::Image,
                text: String::new(),
                path: r"C:\img0.png".into(),
            }))
            .collect();
        assert_eq!(collect_image_paths(&images).len(), 6);
        assert_eq!(collect_image_paths_for_list(&images).len(), 4);
        let dto = segment_to_dto(&images[0]);
        assert_eq!(dto["kind"], "image");
        assert!(dto["text"].is_null());
        assert_eq!(dto["path"], r"C:\img0.png");
    }

    #[test]
    fn wechat_image_type_differs_from_qq() {
        let raw = r#"<WeChatRichEditFormat><EditElement type="1" filepath="D:\x.png"></EditElement></WeChatRichEditFormat>"#;
        let segs = parse_segments(raw, ImRichOrigin::WeChat);
        assert_eq!(segs[0].kind, ImRichSegmentKind::Image);
        assert_eq!(resolve_origin("", raw), ImRichOrigin::WeChat);
    }

    #[test]
    fn guess_origin_uses_edit_element_type_codes_like_csharp() {
        let qq_no_info = r#"<QQRichEditFormat><EditElement type="3" filepath="C:\a.png"></EditElement></QQRichEditFormat>"#;
        assert_eq!(guess_origin_from_xml(qq_no_info), ImRichOrigin::Qq);
        let mixed_wechat_code = r#"<QQRichEditFormat><EditElement type="1" filepath="C:\a.png"></EditElement></QQRichEditFormat>"#;
        assert_eq!(
            guess_origin_from_xml(mixed_wechat_code),
            ImRichOrigin::WeChat
        );
        let with_info = r#"<QQRichEditFormat><Info version="1001"></Info><EditElement type="1" filepath="C:\a.png"></EditElement></QQRichEditFormat>"#;
        assert_eq!(guess_origin_from_xml(with_info), ImRichOrigin::Qq);
    }

    #[test]
    fn emoji_and_png_face_are_placeholder_segments() {
        let wechat =
            r#"<WeChatRichEditFormat><EditElement type="2"></EditElement></WeChatRichEditFormat>"#;
        let segs = parse_segments(wechat, ImRichOrigin::WeChat);
        assert_eq!(segs[0].kind, ImRichSegmentKind::Placeholder);
        assert_eq!(segs[0].text, "[emoji]");
        let dto = segment_to_dto(&segs[0]);
        assert_eq!(dto["kind"], "placeholder");
        assert_eq!(dto["text"], "[emoji]");
        assert!(dto["path"].is_null());

        let qq = r#"<QQRichEditFormat><EditElement type="6" fileid="face-9"></EditElement></QQRichEditFormat>"#;
        let png = parse_segments(qq, ImRichOrigin::Qq);
        assert_eq!(png[0].kind, ImRichSegmentKind::Placeholder);
        assert_eq!(png[0].text, "face-9");
        let empty_id =
            r#"<QQRichEditFormat><EditElement type="6"></EditElement></QQRichEditFormat>"#;
        assert_eq!(
            parse_segments(empty_id, ImRichOrigin::Qq)[0].text,
            "[emoji]"
        );
    }

    #[test]
    fn rewrite_qq_image_to_wechat_type_codes() {
        let raw = r#"<QQRichEditFormat><EditElement type="0"><![CDATA[hi]]></EditElement><EditElement type="3" filepath="C:\a.png"></EditElement></QQRichEditFormat>"#;
        let wechat = rewrite_for_origin(raw, ImRichOrigin::WeChat);
        assert!(wechat.contains(WECHAT_OPEN));
        assert!(wechat.contains(r#"type="1" filepath="C:\a.png""#));
        assert!(!wechat.contains(r#"type="3""#));
    }

    #[test]
    fn persist_images_copies_into_blobs() {
        let dir = isolated_rich_root();
        let src = dir.path().join("src.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .save(&src)
            .unwrap();
        let blobs = dir.path().join("blobs");
        let raw = format!(
            r#"<QQRichEditFormat><EditElement type="3" filepath="{}"></EditElement></QQRichEditFormat>"#,
            src.display()
        );
        let persisted = persist_images(&raw, ImRichOrigin::Qq, &blobs);
        assert_ne!(persisted, raw);
        assert!(persisted.contains("blobs"));
        let segs = parse_segments(&persisted, ImRichOrigin::Qq);
        assert!(Path::new(&segs[0].path).is_file());
        assert!(already_in_blobs(Path::new(&segs[0].path), &blobs));
    }

    #[test]
    fn copy_image_to_library_folder_keeps_filename_like_csharp() {
        let dir = isolated_rich_root();
        let src = dir.path().join("temp").join("shot.png");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"png").unwrap();
        let library = dir.path().join("Image");
        let copied = copy_image_to_library_folder(src.to_str().unwrap(), &library);
        let dest = library.join("shot.png");
        assert_eq!(Path::new(&copied), dest.as_path());
        assert_eq!(std::fs::read(&dest).unwrap(), b"png");
        let again = copy_image_to_library_folder(src.to_str().unwrap(), &library);
        assert_eq!(Path::new(&again), dest.as_path());
        assert_eq!(std::fs::read(&again).unwrap(), b"png");
        // Equal size is insufficient: compare every byte before reusing it.
        std::fs::write(&src, b"new").unwrap();
        let changed = copy_image_to_library_folder(src.to_str().unwrap(), &library);
        assert_ne!(Path::new(&changed), dest.as_path());
        assert_eq!(std::fs::read(&changed).unwrap(), b"new");
        assert_eq!(std::fs::read(&dest).unwrap(), b"png");
        let same = copy_image_to_library_folder(dest.to_str().unwrap(), &library);
        assert_eq!(Path::new(&same), dest.as_path());
        let missing =
            copy_image_to_library_folder(dir.path().join("gone.png").to_str().unwrap(), &library);
        assert!(missing.is_empty());
    }

    #[test]
    fn legacy_image_wrapper_keeps_existing_location_policy_while_checked_is_strict() {
        let dir = isolated_rich_root();
        let source_root = dir.path().join("OneDrive-synthetic-existing-location");
        std::fs::create_dir_all(&source_root).unwrap();
        let source = source_root.join("legacy.png");
        std::fs::write(&source, b"legacy-image").unwrap();
        let library = dir.path().join("Image");
        assert!(copy_image_to_library_folder_checked(source.to_str().unwrap(), &library).is_err());
        let copied = copy_image_to_library_folder(source.to_str().unwrap(), &library);
        assert!(!copied.is_empty());
        assert_eq!(std::fs::read(&copied).unwrap(), b"legacy-image");
        let again = copy_image_to_library_folder(source.to_str().unwrap(), &library);
        assert_eq!(copied, again);
        assert_eq!(std::fs::read(again).unwrap(), b"legacy-image");
        assert_eq!(std::fs::read(source).unwrap(), b"legacy-image");
    }

    #[test]
    fn persist_images_to_library_folder_rewrites_filepath() {
        let dir = isolated_rich_root();
        let src = dir.path().join("src.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .save(&src)
            .unwrap();
        let library = dir.path().join("Image");
        let raw = format!(
            r#"<QQRichEditFormat><EditElement type="3" filepath="{}"></EditElement></QQRichEditFormat>"#,
            src.display()
        );
        let persisted = persist_images_to_library_folder(&raw, ImRichOrigin::Qq, &library);
        assert!(persisted.contains("Image"));
        let segs = parse_segments(&persisted, ImRichOrigin::Qq);
        assert_eq!(Path::new(&segs[0].path), library.join("src.png").as_path());
        assert!(library.join("src.png").is_file());
    }

    #[test]
    fn html_upgrade_requires_im_process_and_local_img() {
        let dir = isolated_rich_root();
        let src = dir.path().join("pic.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 8, 7, 255]))
            .save(&src)
            .unwrap();
        let html = format!(r#"<div>hi<img src="{}"></div>"#, src.display());
        assert!(try_upgrade_html_to_qq(&html, "hi", "chrome.exe", dir.path()).is_none());
        let upgraded = try_upgrade_html_to_qq(&html, "hi", "WeChat.exe", dir.path()).unwrap();
        assert!(
            upgraded.0.contains("QQRichEditFormat") || upgraded.0.contains("WeChatRichEditFormat")
        );
        assert!(upgraded.1.contains("hi") || upgraded.1.contains("[图片]"));
    }
}
