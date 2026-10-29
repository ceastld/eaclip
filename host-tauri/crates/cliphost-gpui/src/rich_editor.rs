//! Native, inert rich-content surface. Payloads remain source documents;
//! projection is never serialized. Text editing reuses TextState's IME
//! and UTF-16 state. One canvas spans text runs and atoms; plain source and
//! metadata fields use the same native input layer with independent histories.

use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::HashSet,
    ops::Range,
    path::PathBuf,
    time::{Duration, Instant},
};

use cliphost_core::i18n::tr;
use gpui::{
    div, prelude::*, px, App, Context, Entity, EntityInputHandler, EventEmitter, KeyDownEvent,
    Subscription, Window,
};

use super::rich_content::{ImRichDocument, RichNodeId, RichNodeKind, RichOrigin};
use crate::{
    editor_html::{
        HtmlBlock, HtmlBlockKind, HtmlBlockMark, HtmlCanvasBlock, HtmlCanvasKey, HtmlCanvasKind,
        HtmlDocument, HtmlEditError, HtmlImageReference, HtmlListKind, HtmlMark, HtmlStyle,
    },
    input::{from_utf16, to_utf16, InputColors},
    preview::DesignTokens,
    view::color,
};

#[derive(Clone, Debug)]
pub enum RichEditorEvent {
    Changed(String),
    Unapplied(String),
    Save,
    Escape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RichEditorKind {
    Html,
    Im,
}

#[derive(Clone)]
enum Document {
    Html(HtmlDocument),
    Im(ImRichDocument),
    Invalid(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selected {
    HtmlText(Range<usize>),
    Canvas { range: Range<usize>, reversed: bool },
    HtmlInsertion,
    HtmlImage(usize),
    Im(RichNodeId),
}

/// Document snapshots are undo data, not another text/IME state machine.
#[derive(Clone)]
struct HistoryFrame {
    // Only autofill's block-format history entry overrides both Undo and Redo
    // carets. Its live caret is at the list start; Quill history uses line end.
    counterpart_caret_utf16: Option<usize>,
    document: Document,
    source: String,
    selected: Option<Selected>,
    caret_utf16: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TypingGroup {
    Canvas,
    Html(usize),
    Im(RichNodeId),
}

#[derive(Clone)]
struct PendingFormat {
    mark: HtmlMark,
    enabled: bool,
}

fn same_format_family(a: &HtmlMark, b: &HtmlMark) -> bool {
    if matches!(a, HtmlMark::Subscript | HtmlMark::Superscript)
        && matches!(b, HtmlMark::Subscript | HtmlMark::Superscript)
    {
        return true;
    }
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

fn format_active(style: &HtmlStyle, mark: &HtmlMark) -> bool {
    match mark {
        HtmlMark::Bold => style.bold,
        HtmlMark::Italic => style.italic,
        HtmlMark::Underline => style.underline,
        HtmlMark::Strike => style.strike,
        HtmlMark::Subscript => style.subscript,
        HtmlMark::Superscript => style.superscript,
        HtmlMark::Code => style.code,
        HtmlMark::Color(value) => style
            .color
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(value)),
        HtmlMark::Background(value) => style
            .background
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(value)),
        HtmlMark::Link(value) => style.link.as_deref() == Some(value),
    }
}

/// Formatting a collapsed caret affects the next inserted text, retaining all
/// existing text and its formatting. No payload change occurs before insertion.
fn apply_pending_formats(
    document: &mut HtmlDocument,
    index: usize,
    range: Range<usize>,
    pending: &[PendingFormat],
) -> Result<Option<Range<usize>>, HtmlEditError> {
    if range.is_empty() || pending.is_empty() {
        return Ok(None);
    }
    let mut candidate = document.clone();
    let mut index = index;
    let mut range = range;
    let selected_text = candidate
        .runs()
        .get(index)
        .ok_or(HtmlEditError::MissingRun)?
        .text
        .get(range.clone())
        .ok_or(HtmlEditError::InvalidTextRange)?
        .to_string();
    let mut changed = false;
    for setting in pending {
        let active = format_active(&candidate.runs()[index].style, &setting.mark);
        if active == setting.enabled {
            continue;
        }
        let anchor =
            candidate.toggle_text_range_anchor(index, range.clone(), setting.mark.clone())?;
        index = candidate
            .runs()
            .iter()
            .position(|run| run.source_range == anchor)
            .ok_or(HtmlEditError::MissingRun)?;
        range = 0..selected_text.len();
        changed = true;
    }
    if changed {
        let anchor = candidate.runs()[index].source_range.clone();
        *document = candidate;
        Ok(Some(anchor))
    } else {
        Ok(None)
    }
}

struct HtmlTextUpdate {
    anchor: Range<usize>,
    text: String,
}

fn update_html_text(
    document: &mut HtmlDocument,
    span: Range<usize>,
    before: &str,
    after: &str,
    pending: &[PendingFormat],
    insertion_hint: Option<usize>,
) -> Result<HtmlTextUpdate, HtmlEditError> {
    let mut candidate = document.clone();
    if after.is_empty() {
        let anchor = candidate.replace_source_text_span(span, "")?;
        *document = candidate;
        return Ok(HtmlTextUpdate {
            anchor,
            text: String::new(),
        });
    }
    if before.is_empty() && span.is_empty() {
        candidate.replace_source_text_span(span.clone(), after)?;
    } else {
        let index = candidate
            .runs()
            .iter()
            .position(|run| run.source_range == span)
            .ok_or(HtmlEditError::MissingRun)?;
        let (old, new) = changed_text_ranges_at_insertion(before, after, insertion_hint);
        candidate.replace_text_range(index, old, &after[new])?;
    }
    let index = candidate
        .runs()
        .iter()
        .position(|run| run.source_range.start == span.start)
        .ok_or(HtmlEditError::MissingRun)?;
    let actual = candidate.runs()[index].text.clone();
    let (_, inserted) = changed_text_ranges_at_insertion(before, &actual, insertion_hint);
    let formatted = apply_pending_formats(&mut candidate, index, inserted, pending)?;
    let anchor = formatted.unwrap_or_else(|| candidate.runs()[index].source_range.clone());
    let text = candidate
        .runs()
        .iter()
        .find(|run| run.source_range == anchor)
        .ok_or(HtmlEditError::MissingRun)?
        .text
        .clone();
    *document = candidate;
    Ok(HtmlTextUpdate { anchor, text })
}

fn insert_html_text(
    document: &mut HtmlDocument,
    text: &str,
    pending: &[PendingFormat],
) -> Result<HtmlTextUpdate, HtmlEditError> {
    let mut candidate = document.clone();
    let anchor = candidate.append_paragraph(text);
    if text.is_empty() {
        *document = candidate;
        return Ok(HtmlTextUpdate {
            anchor,
            text: String::new(),
        });
    }
    let index = candidate
        .runs()
        .iter()
        .position(|run| run.source_range == anchor)
        .ok_or(HtmlEditError::MissingRun)?;
    let range = 0..candidate.runs()[index].text.len();
    let anchor = apply_pending_formats(&mut candidate, index, range, pending)?.unwrap_or(anchor);
    let text = candidate
        .runs()
        .iter()
        .find(|run| run.source_range == anchor)
        .ok_or(HtmlEditError::MissingRun)?
        .text
        .clone();
    *document = candidate;
    Ok(HtmlTextUpdate { anchor, text })
}

#[derive(Default)]
struct RichHistory {
    undo: Vec<HistoryFrame>,
    redo: Vec<HistoryFrame>,
    typing: Option<(TypingGroup, Instant)>,
}

impl RichHistory {
    // Quill 1.3.7 frontend defaults: 100 entries, 1000ms group window.
    fn record(&mut self, before: HistoryFrame, typing: Option<TypingGroup>, now: Instant) {
        self.redo.clear();
        let merge =
            typing
                .as_ref()
                .zip(self.typing.as_ref())
                .is_some_and(|(kind, (previous, started))| {
                    kind == previous
                        && now.saturating_duration_since(*started) < Duration::from_millis(1000)
                });
        if !merge {
            self.undo.push(before);
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
            self.typing = typing.map(|kind| (kind, now));
        }
    }
    fn cutoff(&mut self) {
        self.typing = None;
    }
    fn record_list_autofill(
        &mut self,
        before: HistoryFrame,
        mut space: HistoryFrame,
        final_caret_utf16: usize,
        now: Instant,
    ) {
        // The ordinary space joins preceding typing. Converting its marker and
        // line is a separate USER edit, isolated from subsequent list typing.
        self.record(before, Some(TypingGroup::Canvas), now);
        self.cutoff();
        space.counterpart_caret_utf16 = Some(final_caret_utf16);
        self.record(space, Some(TypingGroup::Canvas), now);
        self.cutoff();
    }
    fn swap_counterpart_caret(restored: &mut HistoryFrame, current: &mut HistoryFrame) {
        if let Some(caret) = restored.counterpart_caret_utf16.take() {
            current.counterpart_caret_utf16 = Some(restored.caret_utf16);
            current.caret_utf16 = caret;
        }
    }
    fn undo(&mut self, mut current: HistoryFrame) -> Option<HistoryFrame> {
        let mut previous = self.undo.pop()?;
        Self::swap_counterpart_caret(&mut previous, &mut current);
        self.redo.push(current);
        self.cutoff();
        Some(previous)
    }
    fn redo(&mut self, mut current: HistoryFrame) -> Option<HistoryFrame> {
        let mut next = self.redo.pop()?;
        Self::swap_counterpart_caret(&mut next, &mut current);
        self.undo.push(current);
        self.cutoff();
        Some(next)
    }
}

fn changed_text_ranges(before: &str, after: &str) -> (Range<usize>, Range<usize>) {
    let prefix = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    let suffix = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    (prefix..before.len() - suffix, prefix..after.len() - suffix)
}

/// Repeated characters do not reveal where insertion happened. Prefer the
/// native caret anchor when both untouched sides validate that insertion.
fn changed_text_ranges_at_insertion(
    before: &str,
    after: &str,
    insertion_hint: Option<usize>,
) -> (Range<usize>, Range<usize>) {
    if let Some(start) = insertion_hint {
        if let Some(added) = after.len().checked_sub(before.len()) {
            let end = start.saturating_add(added);
            if added > 0
                && before.is_char_boundary(start)
                && after.is_char_boundary(start)
                && after.is_char_boundary(end)
                && before[..start] == after[..start]
                && before[start..] == after[end..]
            {
                return (start..start, start..end);
            }
        }
    }
    changed_text_ranges(before, after)
}

const CANVAS_OBJECT: &str = "\u{fffc}";

#[derive(Clone, Debug)]
enum ImCanvasKind {
    Text(RichNodeId),
    Atom { id: RichNodeId, protected: bool },
}

#[derive(Clone, Debug)]
struct ImCanvasSpan {
    range: Range<usize>,
    source: Range<usize>,
    kind: ImCanvasKind,
}

struct ImCanvasProjection {
    text: String,
    spans: Vec<ImCanvasSpan>,
}

fn canvas_text(text: &str) -> String {
    // The existing IM DOM paints CRLF as one br; a real ZWSP is payload.
    text.replace("\r\n", "\n")
}

fn xml_opening_end(raw: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, ch) in raw.get(start..)?.char_indices() {
        if let Some(current) = quote {
            if ch == current {
                quote = None;
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch == '>' {
            return Some(start + offset + 1);
        }
    }
    None
}

fn im_canvas_projection(document: &ImRichDocument) -> ImCanvasProjection {
    let source = document.serialize();
    let mut source_start =
        xml_opening_end(&source, document.envelope_range().start).unwrap_or(source.len());
    let mut projection = ImCanvasProjection {
        text: String::new(),
        spans: Vec::new(),
    };
    for node in document.nodes() {
        let source_range = source_start..source_start + node.raw_xml().len();
        source_start = source_range.end;
        if node.is_metadata() {
            continue;
        }
        let start = projection.text.len();
        let kind = if node.can_edit_text() {
            projection.text.push_str(&canvas_text(node.text()));
            ImCanvasKind::Text(node.id())
        } else {
            projection.text.push_str(CANVAS_OBJECT);
            ImCanvasKind::Atom {
                id: node.id(),
                protected: node.kind() == RichNodeKind::Opaque || node.kind() == RichNodeKind::Text,
            }
        };
        projection.spans.push(ImCanvasSpan {
            range: start..projection.text.len(),
            source: source_range,
            kind,
        });
    }
    projection
}

fn canvas_offset_in_raw_text(raw: &str, target: usize) -> Option<usize> {
    let mut displayed = 0;
    let mut chars = raw.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if displayed == target {
            return Some(offset);
        }
        if ch == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            chars.next();
            displayed += 1;
        } else {
            displayed += ch.len_utf8();
        }
    }
    (displayed == target).then_some(raw.len())
}

struct XmlCanvasPart {
    raw: Range<usize>,
    decoded: Range<usize>,
    text: String,
    cdata: bool,
    entity: bool,
}

fn xml_canvas_parts(raw: &str, expected: &str) -> Result<Vec<XmlCanvasPart>, String> {
    let start = xml_opening_end(raw, 0).ok_or("Invalid text object opening tag")?;
    let end = raw.rfind("</").ok_or("Text object has no closing tag")?;
    let mut cursor = start;
    let mut decoded = String::new();
    let mut parts = Vec::new();
    while cursor < end {
        let tail = &raw[cursor..end];
        let (range, text, cdata, entity, next) = if tail.starts_with("<![CDATA[") {
            let content = cursor + 9;
            let close = raw[content..end].find("]]>").ok_or("Invalid text CDATA")? + content;
            (
                content..close,
                raw[content..close].to_string(),
                true,
                false,
                close + 3,
            )
        } else if tail.starts_with("<!--") {
            cursor += tail.find("-->").ok_or("Invalid text comment")? + 3;
            continue;
        } else if tail.starts_with("<?") {
            cursor += tail.find("?>").ok_or("Invalid text instruction")? + 2;
            continue;
        } else if tail.starts_with('<') {
            return Err("Nested text markup cannot be changed by the canvas".into());
        } else if tail.starts_with('&') {
            let close = tail.find(';').ok_or("Invalid XML character reference")?;
            let token = &tail[1..close];
            let character = match token {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                token if token.starts_with("#x") => u32::from_str_radix(&token[2..], 16)
                    .ok()
                    .and_then(char::from_u32),
                token if token.starts_with('#') => {
                    token[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            }
            .ok_or("Unknown XML character reference is protected")?;
            (
                cursor..cursor + close + 1,
                character.to_string(),
                false,
                true,
                cursor + close + 1,
            )
        } else {
            let length = tail.find(['<', '&']).unwrap_or(tail.len());
            (
                cursor..cursor + length,
                tail[..length].to_string(),
                false,
                false,
                cursor + length,
            )
        };
        let from = decoded.len();
        decoded.push_str(&text);
        parts.push(XmlCanvasPart {
            raw: range,
            decoded: from..decoded.len(),
            text,
            cdata,
            entity,
        });
        cursor = next;
    }
    if decoded != expected {
        return Err("Text object source no longer matches its canvas text".into());
    }
    Ok(parts)
}

fn escaped_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Patch character data only, retaining untouched references, CDATA wrappers,
/// comments, instructions and vendor attributes of the same text object.
fn patch_xml_canvas_text(
    raw: &str,
    before: &str,
    range: Range<usize>,
    inserted: &str,
) -> Result<String, String> {
    if range.start > range.end
        || !before.is_char_boundary(range.start)
        || !before.is_char_boundary(range.end)
    {
        return Err("Invalid XML text selection".into());
    }
    let parts = xml_canvas_parts(raw, before)?;
    let mut patches = Vec::new();
    let mut inserted_once = false;
    for part in &parts {
        let start = range.start.max(part.decoded.start);
        let end = range.end.min(part.decoded.end);
        let at_start = !inserted_once
            && range.start >= part.decoded.start
            && (range.start < part.decoded.end
                || range.is_empty() && range.start == part.decoded.end
                || part.decoded.is_empty() && range.start == part.decoded.start);
        if start >= end && !at_start {
            continue;
        }
        let local_start = start.min(part.decoded.end) - part.decoded.start;
        let local_end = end.max(start).min(part.decoded.end) - part.decoded.start;
        let replacement = if at_start {
            inserted_once = true;
            inserted
        } else {
            ""
        };
        if part.cdata {
            let text = format!(
                "{}{}{}",
                &part.text[..local_start],
                replacement,
                &part.text[local_end..]
            );
            patches.push((part.raw.clone(), text.replace("]]>", "]]]]><![CDATA[>")));
        } else {
            let raw_start = if part.entity {
                if local_start == 0 {
                    part.raw.start
                } else {
                    part.raw.end
                }
            } else {
                part.raw.start + local_start
            };
            let raw_end = if part.entity {
                if local_end == 0 {
                    part.raw.start
                } else {
                    part.raw.end
                }
            } else {
                part.raw.start + local_end
            };
            patches.push((raw_start..raw_end, escaped_xml_text(replacement)));
        }
    }
    if !inserted_once && !inserted.is_empty() {
        let at = xml_opening_end(raw, 0).ok_or("Invalid text opening tag")?;
        patches.push((at..at, escaped_xml_text(inserted)));
    }
    let mut result = raw.to_string();
    for (range, value) in patches.into_iter().rev() {
        result.replace_range(range, &value);
    }
    Ok(result)
}

fn replace_im_canvas_range(
    document: &mut ImRichDocument,
    range: Range<usize>,
    text: &str,
) -> Result<usize, String> {
    let projection = im_canvas_projection(document);
    if range.start > range.end
        || !projection.text.is_char_boundary(range.start)
        || !projection.text.is_char_boundary(range.end)
    {
        return Err("Invalid IM canvas selection".into());
    }
    let owner = projection.spans.iter().find_map(|span| match span.kind {
        ImCanvasKind::Text(id)
            if span.range.start <= range.start && range.start <= span.range.end =>
        {
            Some(id)
        }
        _ => None,
    });
    let mut patches = Vec::new();
    for span in &projection.spans {
        let overlaps = range.start < span.range.end && range.end > span.range.start;
        match span.kind {
            ImCanvasKind::Atom {
                protected: true, ..
            } if overlaps => {
                return Err(
                    "Unknown XML objects are protected; the original content was retained".into(),
                )
            }
            ImCanvasKind::Atom { .. } if overlaps => {
                if range.start > span.range.start || range.end < span.range.end {
                    return Err("An IM object must be selected as a whole".into());
                }
                patches.push((span.source.clone(), String::new()));
            }
            ImCanvasKind::Text(id) if overlaps || owner == Some(id) && !text.is_empty() => {
                let node = document.node(id).ok_or("Text object no longer exists")?;
                let from = range.start.max(span.range.start).min(span.range.end) - span.range.start;
                let to = range.end.max(span.range.start).min(span.range.end) - span.range.start;
                let raw_from = canvas_offset_in_raw_text(node.text(), from)
                    .ok_or("Invalid normalized text selection")?;
                let raw_to = canvas_offset_in_raw_text(node.text(), to)
                    .ok_or("Invalid normalized text selection")?;
                let insertion = if owner == Some(id) { text } else { "" };
                let changed = format!(
                    "{}{}{}",
                    &node.text()[..raw_from],
                    insertion,
                    &node.text()[raw_to..]
                );
                let mut validated = document.clone();
                validated
                    .replace_text(id, &changed)
                    .map_err(|error| error.to_string())?;
                let value = if node.raw_xml().trim_end().ends_with("/>") {
                    validated
                        .node(id)
                        .ok_or("Text object no longer exists")?
                        .raw_xml()
                        .to_string()
                } else {
                    patch_xml_canvas_text(node.raw_xml(), node.text(), raw_from..raw_to, insertion)?
                };
                patches.push((span.source.clone(), value));
            }
            _ => {}
        }
    }
    let mut source = document.serialize();
    for (span, value) in patches.into_iter().rev() {
        source.replace_range(span, &value);
    }
    let mut candidate =
        ImRichDocument::parse(&source, document.origin()).map_err(|error| error.to_string())?;
    if owner.is_none() && !text.is_empty() {
        let remaining = im_canvas_projection(&candidate);
        let before = remaining
            .spans
            .iter()
            .find(|span| span.range.start >= range.start)
            .map(|span| match span.kind {
                ImCanvasKind::Text(id) | ImCanvasKind::Atom { id, .. } => id,
            });
        candidate
            .insert_text(before, text)
            .map_err(|error| error.to_string())?;
    }
    let caret = range.start + canvas_text(text).len();
    *document = candidate;
    Ok(caret)
}

use canvas_native::{CanvasAtom, CanvasFragment, CanvasParagraph, NativeCanvas, NativeCanvasEvent};

fn document_canvas_text(document: &Document) -> String {
    match document {
        Document::Html(document) => document.canvas_projection().text,
        Document::Im(document) => im_canvas_projection(document).text,
        Document::Invalid(_) => String::new(),
    }
}

fn acknowledge_native_selection(
    selection: Range<usize>,
    native_edit_start: usize,
    inserted: &str,
    canonical_start: usize,
    canonical: &str,
    html: bool,
) -> Range<usize> {
    let normalize = |text: &str| {
        let text = canvas_text(text);
        if html {
            text.replace('\r', "\n")
        } else {
            text
        }
    };
    let inserted_end = native_edit_start + inserted.len();
    let canonical_end = canonical_start + normalize(inserted).len();
    let map = |offset: usize| {
        let mut mapped = if offset < native_edit_start {
            offset.min(canonical_start)
        } else if offset <= inserted_end {
            let mut relative = offset - native_edit_start;
            while !inserted.is_char_boundary(relative) {
                relative -= 1;
            }
            canonical_start + normalize(&inserted[..relative]).len()
        } else {
            canonical_end + offset - inserted_end
        };
        mapped = mapped.min(canonical.len());
        while !canonical.is_char_boundary(mapped) {
            mapped -= 1;
        }
        mapped
    };
    map(selection.start)..map(selection.end)
}

fn canvas_inline_ranges(document: &HtmlDocument, selection: Range<usize>) -> Vec<Range<usize>> {
    document
        .canvas_projection()
        .spans
        .into_iter()
        .filter_map(|span| {
            let HtmlCanvasKind::Text { .. } = span.kind else {
                return None;
            };
            let range = selection.start.max(span.range.start)..selection.end.min(span.range.end);
            (range.start < range.end).then_some(range)
        })
        .collect()
}

fn canvas_style_at(document: &HtmlDocument, selection: Range<usize>) -> Vec<HtmlStyle> {
    let projection = document.canvas_projection();
    let mut styles = Vec::new();
    for span in &projection.spans {
        let HtmlCanvasKind::Text { run } = span.kind else {
            continue;
        };
        let included = if selection.is_empty() {
            span.range.start <= selection.start && selection.start <= span.range.end
        } else {
            selection.start < span.range.end && selection.end > span.range.start
        };
        if included {
            styles.push(document.runs()[run].style.clone());
            if selection.is_empty() {
                break;
            }
        }
    }
    styles
}

fn apply_canvas_inline_mark(
    document: &mut HtmlDocument,
    selection: Range<usize>,
    mark: HtmlMark,
    enabled: bool,
) -> Result<(), HtmlEditError> {
    let mut candidate = document.clone();
    let before = candidate.canvas_projection().text;
    let ranges = canvas_inline_ranges(&candidate, selection);
    for target in ranges.into_iter().rev() {
        let projection = candidate.canvas_projection();
        let (span, run) = projection
            .spans
            .iter()
            .find_map(|span| match span.kind {
                HtmlCanvasKind::Text { run }
                    if span.range.start <= target.start && span.range.end >= target.end =>
                {
                    Some((span, run))
                }
                _ => None,
            })
            .ok_or(HtmlEditError::MissingRun)?;
        let style = &candidate.runs()[run].style;
        let actual_mark = if !enabled {
            match &mark {
                HtmlMark::Color(_) => style.color.clone().map(HtmlMark::Color),
                HtmlMark::Background(_) => style.background.clone().map(HtmlMark::Background),
                HtmlMark::Link(_) => style.link.clone().map(HtmlMark::Link),
                _ => format_active(style, &mark).then_some(mark.clone()),
            }
        } else {
            (!format_active(style, &mark)).then_some(mark.clone())
        };
        if let Some(mark) = actual_mark {
            candidate.toggle_text_range_anchor(
                run,
                target.start - span.range.start..target.end - span.range.start,
                mark,
            )?;
        }
        if candidate.canvas_projection().text != before {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    *document = candidate;
    Ok(())
}

fn clean_canvas_selection(
    document: &mut HtmlDocument,
    selection: Range<usize>,
) -> Result<(), HtmlEditError> {
    let mut candidate = document.clone();
    let before = candidate.canvas_projection().text;
    let block_targets = canvas_block_targets(&candidate, selection.clone());
    let ranges = canvas_inline_ranges(&candidate, selection);
    for target in ranges.into_iter().rev() {
        let projection = candidate.canvas_projection();
        let (span, run) = projection
            .spans
            .iter()
            .find_map(|span| match span.kind {
                HtmlCanvasKind::Text { run }
                    if span.range.start <= target.start && span.range.end >= target.end =>
                {
                    Some((span, run))
                }
                _ => None,
            })
            .ok_or(HtmlEditError::MissingRun)?;
        candidate.clear_selection_format_anchor(
            run,
            target.start - span.range.start..target.end - span.range.start,
        )?;
        if candidate.canvas_projection().text != before {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    // A selected empty or image-only line has no text run on which the existing
    // clear-selection helper could clear its structural formats.
    for (offset, _) in block_targets.into_iter().rev() {
        let mut cleared = false;
        for _ in 0..8 {
            let owner = canvas_block_owners(&candidate, offset..offset)
                .into_iter()
                .next()
                .ok_or(HtmlEditError::MissingBlock)?;
            let block = candidate
                .canvas_block_format(&owner)
                .ok_or(HtmlEditError::MissingBlock)?;
            let next = if block.code {
                Some(HtmlBlockMark::CodeBlock)
            } else if let HtmlBlockKind::Header(level) = block.kind {
                Some(HtmlBlockMark::Header(level))
            } else if let Some(kind) = block.list {
                Some(HtmlBlockMark::List(kind))
            } else if block.quote {
                Some(HtmlBlockMark::Blockquote)
            } else if block.indent > 0 {
                Some(HtmlBlockMark::Indent(0))
            } else {
                None
            };
            let Some(mark) = next else {
                cleared = true;
                break;
            };
            candidate.toggle_canvas_block(&owner, mark)?;
            if candidate.canvas_projection().text != before {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
        }
        if !cleared {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    *document = candidate;
    Ok(())
}

fn block_format_active(block: &HtmlBlock, mark: HtmlBlockMark) -> bool {
    match mark {
        HtmlBlockMark::Paragraph => {
            block.kind == HtmlBlockKind::Paragraph
                && block.list.is_none()
                && !block.quote
                && !block.code
        }
        HtmlBlockMark::Header(level) => block.kind == HtmlBlockKind::Header(level),
        HtmlBlockMark::List(kind) => block.list == Some(kind),
        HtmlBlockMark::Indent(level) => block.indent == level,
        HtmlBlockMark::Blockquote => block.quote,
        HtmlBlockMark::CodeBlock => block.code,
    }
}

fn canvas_block_targets(
    document: &HtmlDocument,
    selection: Range<usize>,
) -> Vec<(usize, HtmlCanvasBlock)> {
    let projection = document.canvas_projection();
    let mut owners = Vec::new();
    let mut paragraph_start = 0;
    for span in &projection.spans {
        let HtmlCanvasKind::Boundary { left, .. } = &span.kind else {
            continue;
        };
        let included = if selection.is_empty() {
            paragraph_start <= selection.start && selection.start < span.range.end
        } else {
            selection.start < span.range.end && selection.end > paragraph_start
        };
        if included
            && !owners.iter().any(|(_, owner): &(usize, HtmlCanvasBlock)| {
                owner.source_range == left.source_range
            })
        {
            owners.push((paragraph_start, left.clone()));
        }
        paragraph_start = span.range.end;
        if selection.is_empty() && included {
            break;
        }
    }
    owners
}

fn canvas_block_owners(document: &HtmlDocument, selection: Range<usize>) -> Vec<HtmlCanvasBlock> {
    canvas_block_targets(document, selection)
        .into_iter()
        .map(|(_, owner)| owner)
        .collect()
}

fn common_canvas_indent(document: &HtmlDocument, selection: Range<usize>) -> u8 {
    let levels = canvas_block_owners(document, selection)
        .into_iter()
        .map(|owner| {
            document
                .canvas_block_format(&owner)
                .map(|block| block.indent)
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let Some(first) = levels.first().copied().filter(|level| *level > 0) else {
        return 0;
    };
    // Mirror Quill 1.3.7 combineFormats and the toolbar's parseInt conversion.
    // Missing indent drops the field; a repeated value after a mixed array also
    // drops it in that implementation. Distinct arrays retain their first value.
    let mut seen = vec![first];
    for level in levels.into_iter().skip(1) {
        if level == 0 {
            return 0;
        }
        if seen.len() == 1 {
            if level != first {
                seen.push(level);
            }
        } else if seen.contains(&level) {
            return 0;
        } else {
            seen.push(level);
        }
    }
    first
}

fn validate_unapplied_recovery_snapshot(
    document: &Document,
    source: &str,
    proposed: &str,
    selection: Range<usize>,
) -> Result<(), String> {
    let canonical = match document {
        Document::Html(document) => document.source().to_string(),
        Document::Im(document) => document.serialize(),
        Document::Invalid(_) => {
            return Err("Recovery requires a valid applied source document.".into())
        }
    };
    if canonical != source {
        return Err("Applied document source no longer matches the editor.".into());
    }
    if selection.start > selection.end
        || selection.end > proposed.len()
        || !proposed.is_char_boundary(selection.start)
        || !proposed.is_char_boundary(selection.end)
    {
        return Err("Rejected-edit selection is outside the current canvas.".into());
    }
    Ok(())
}

#[cfg(test)]
fn canvas_block_ranges(document: &HtmlDocument, selection: Range<usize>) -> Vec<Range<usize>> {
    canvas_block_owners(document, selection)
        .into_iter()
        .map(|owner| owner.source_range)
        .collect()
}

fn apply_canvas_block_mark(
    document: &mut HtmlDocument,
    selection: Range<usize>,
    mark: HtmlBlockMark,
) -> Result<(), HtmlEditError> {
    let mut candidate = document.clone();
    let before = candidate.canvas_projection().text;
    let targets = canvas_block_targets(&candidate, selection);
    let enabled = !targets.iter().all(|(_, owner)| {
        candidate
            .canvas_block_format(owner)
            .is_some_and(|block| block_format_active(&block, mark))
    });
    for (offset, _) in targets.into_iter().rev() {
        let owner = canvas_block_owners(&candidate, offset..offset)
            .into_iter()
            .next()
            .ok_or(HtmlEditError::MissingBlock)?;
        let active = candidate
            .canvas_block_format(&owner)
            .is_some_and(|block| block_format_active(&block, mark));
        if matches!(mark, HtmlBlockMark::Paragraph | HtmlBlockMark::Indent(_)) || active != enabled
        {
            candidate.toggle_canvas_block(&owner, mark)?;
        }
        if candidate.canvas_projection().text != before {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    *document = candidate;
    Ok(())
}
pub(super) use canvas_native::{
    NativeCanvas as EditorTextInput, NativeCanvasEvent as EditorTextEvent,
};

#[path = "editor_im_insert.rs"]
mod im_insert;

#[derive(Clone, Debug, PartialEq, Eq)]
enum MetadataContext {
    Parameters,
    HtmlImage(usize),
    HtmlLink(Range<usize>),
    ImPath(RichNodeId),
}

fn metadata_field_sync(
    previous: Option<&MetadataContext>,
    current: &MetadataContext,
    value: &str,
    source_value: &str,
) -> Option<(String, bool)> {
    if previous != Some(current) {
        // Preserve a reusable parameter value when leaving an object, while
        // giving the new target its own undo lifecycle even for equal paths.
        Some((
            if source_value.is_empty() {
                value
            } else {
                source_value
            }
            .to_string(),
            true,
        ))
    } else if value.is_empty() || !source_value.is_empty() {
        Some((source_value.to_string(), false))
    } else {
        None
    }
}

#[path = "editor_html_paste.rs"]
mod clipboard_paste;
use clipboard_paste::{HtmlClipboardLine, HtmlClipboardPlan, HtmlClipboardSpan};
#[path = "editor_html_list_autofill.rs"]
mod html_list_autofill;
#[path = "editor_html_tab.rs"]
mod html_tab;
#[path = "rich_editor_list_autofill_apply.rs"]
mod list_autofill_apply;
#[path = "rich_editor_tab_apply.rs"]
mod tab_apply;

#[derive(Clone)]
struct PasteSnapshot {
    epoch: u64,
    kind: RichEditorKind,
    origin: RichOrigin,
    source: String,
    text: String,
    selection: Range<usize>,
    reversed: bool,
    unapplied: Option<String>,
}
impl PasteSnapshot {
    fn matches(&self, current: &Self) -> bool {
        self.epoch == current.epoch
            && self.kind == current.kind
            && self.origin == current.origin
            && self.source == current.source
            && self.text == current.text
            && self.selection == current.selection
            && self.reversed == current.reversed
            && self.unapplied == current.unapplied
            && self.selection.start <= self.selection.end
            && self.selection.end <= self.text.len()
            && self.text.is_char_boundary(self.selection.start)
            && self.text.is_char_boundary(self.selection.end)
    }
}
enum ClipboardPaste {
    Text(String),
    Html(HtmlClipboardPlan),
}

fn clipboard_paragraph() -> HtmlBlock {
    HtmlBlock {
        kind: HtmlBlockKind::Paragraph,
        list: None,
        indent: 0,
        quote: false,
        code: false,
        elements: Vec::new(),
        source_range: 0..0,
    }
}
fn plain_html_paste(text: &str, code: bool) -> HtmlClipboardPlan {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let spans = (!text.is_empty())
        .then(|| HtmlClipboardSpan {
            range: 0..text.len(),
            style: HtmlStyle::default(),
        })
        .into_iter()
        .collect();
    let lines = text
        .char_indices()
        .filter(|(_, ch)| *ch == '\n')
        .map(|(newline, _)| {
            let mut format = clipboard_paragraph();
            if code {
                format.kind = HtmlBlockKind::Code;
                format.code = true;
            }
            HtmlClipboardLine { newline, format }
        })
        .collect();
    HtmlClipboardPlan { text, spans, lines }
}
fn clipboard_paste_plan(
    kind: RichEditorKind,
    code: bool,
    recovering: bool,
    html: Option<&str>,
    text: Option<&str>,
) -> Result<Option<ClipboardPaste>, String> {
    if kind == RichEditorKind::Im || recovering {
        return Ok(text
            .filter(|text| !text.is_empty())
            .map(|text| ClipboardPaste::Text(text.into())));
    }
    let plan = if code {
        // Quill's current code-block branch takes plain visible text and keeps
        // that block; it never imports clipboard inline/block formatting.
        let text = match text {
            Some(text) => Some(text.to_string()),
            None => html
                .map(HtmlClipboardPlan::from_html)
                .transpose()?
                .map(|plan| plan.text),
        };
        plain_html_paste(text.as_deref().unwrap_or(""), true)
    } else if let Some(html) = html.filter(|html| !html.is_empty()) {
        HtmlClipboardPlan::from_html(html)?
    } else {
        plain_html_paste(text.unwrap_or(""), false)
    };
    Ok((!plan.text.is_empty()).then_some(ClipboardPaste::Html(plan)))
}

fn paste_line_owner(
    document: &HtmlDocument,
    newline: usize,
) -> Result<(HtmlCanvasBlock, HtmlBlock), HtmlEditError> {
    let owner = document
        .canvas_projection()
        .spans
        .into_iter()
        .find_map(|span| {
            if span.range.start != newline {
                return None;
            }
            if let HtmlCanvasKind::Boundary { left, .. } = span.kind {
                Some(left)
            } else {
                None
            }
        })
        .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
    let format = document
        .canvas_block_format(&owner)
        .ok_or(HtmlEditError::MissingBlock)?;
    Ok((owner, format))
}
fn paste_line_mark(
    document: &mut HtmlDocument,
    newline: usize,
    mark: HtmlBlockMark,
) -> Result<(), HtmlEditError> {
    let (owner, _) = paste_line_owner(document, newline)?;
    document.toggle_canvas_block(&owner, mark)
}
fn set_paste_line_format(
    document: &mut HtmlDocument,
    newline: usize,
    desired: &HtmlBlock,
) -> Result<(), HtmlEditError> {
    let desired_header = match desired.kind {
        HtmlBlockKind::Header(level) => Some(level),
        _ => None,
    };
    let (_, current) = paste_line_owner(document, newline)?;
    if current.code && !desired.code {
        paste_line_mark(document, newline, HtmlBlockMark::CodeBlock)?;
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if let HtmlBlockKind::Header(level) = current.kind {
        if Some(level) != desired_header {
            paste_line_mark(document, newline, HtmlBlockMark::Header(level))?;
        }
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if current.list != desired.list {
        if let Some(list) = current.list {
            paste_line_mark(document, newline, HtmlBlockMark::List(list))?;
        }
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if current.quote != desired.quote {
        paste_line_mark(document, newline, HtmlBlockMark::Blockquote)?;
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if desired.code && !current.code {
        paste_line_mark(document, newline, HtmlBlockMark::CodeBlock)?;
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if let Some(level) = desired_header {
        if current.kind != HtmlBlockKind::Header(level) {
            paste_line_mark(document, newline, HtmlBlockMark::Header(level))?;
        }
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if let Some(list) = desired.list {
        if current.list != Some(list) {
            paste_line_mark(document, newline, HtmlBlockMark::List(list))?;
        }
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if current.indent != desired.indent {
        paste_line_mark(document, newline, HtmlBlockMark::Indent(desired.indent))?;
    }
    let (_, current) = paste_line_owner(document, newline)?;
    if current.code != desired.code
        || current.list != desired.list
        || current.quote != desired.quote
        || current.indent != desired.indent
        || match current.kind {
            HtmlBlockKind::Header(level) => Some(level),
            _ => None,
        } != desired_header
    {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    Ok(())
}
fn set_paste_inline_style(
    document: &mut HtmlDocument,
    selection: Range<usize>,
    style: &HtmlStyle,
) -> Result<(), HtmlEditError> {
    for range in canvas_inline_ranges(document, selection).into_iter().rev() {
        let projection = document.canvas_projection();
        let (block, current_style) = projection
            .spans
            .iter()
            .find_map(|span| match span.kind {
                HtmlCanvasKind::Text { run }
                    if span.range.start <= range.start && span.range.end >= range.end =>
                {
                    Some((
                        &document.blocks()[document.runs()[run].block_index],
                        document.runs()[run].style.clone(),
                    ))
                }
                _ => None,
            })
            .ok_or(HtmlEditError::MissingRun)?;
        let heading = matches!(block.kind, HtmlBlockKind::Header(_));
        let code = block.code;
        for (mark, enabled) in [
            (HtmlMark::Bold, heading || style.bold),
            (HtmlMark::Italic, style.italic),
            (HtmlMark::Underline, style.underline),
            (HtmlMark::Strike, style.strike),
            (HtmlMark::Subscript, style.subscript),
            (HtmlMark::Superscript, style.superscript),
            (HtmlMark::Code, code),
            (
                HtmlMark::Color(style.color.clone().unwrap_or_default()),
                style.color.is_some(),
            ),
            (
                HtmlMark::Background(style.background.clone().unwrap_or_default()),
                style.background.is_some(),
            ),
            (
                HtmlMark::Link(style.link.clone().unwrap_or_default()),
                style.link.is_some(),
            ),
        ] {
            let active = match &mark {
                HtmlMark::Color(_) if !enabled => current_style.color.is_some(),
                HtmlMark::Background(_) if !enabled => current_style.background.is_some(),
                HtmlMark::Link(_) if !enabled => current_style.link.is_some(),
                _ => format_active(&current_style, &mark),
            };
            if active != enabled {
                apply_canvas_inline_mark(document, range.clone(), mark, enabled)?;
            }
        }
    }
    Ok(())
}
fn apply_html_paste(
    document: &mut HtmlDocument,
    selection: Range<usize>,
    plan: &HtmlClipboardPlan,
) -> Result<usize, HtmlEditError> {
    if plan.text.is_empty() {
        return Ok(selection.start);
    }
    if plan.text.contains('\r')
        || plan.spans.iter().any(|span| {
            span.range.start > span.range.end
                || span.range.end > plan.text.len()
                || !plan.text.is_char_boundary(span.range.start)
                || !plan.text.is_char_boundary(span.range.end)
        })
        || plan
            .lines
            .iter()
            .any(|line| plan.text.as_bytes().get(line.newline) != Some(&b'\n'))
    {
        return Err(HtmlEditError::InvalidTextRange);
    }
    let before = document.canvas_projection().text;
    let limit = before.len().saturating_sub(1);
    if selection.start > selection.end
        || selection.end > before.len()
        || !before.is_char_boundary(selection.start)
        || !before.is_char_boundary(selection.end)
    {
        return Err(HtmlEditError::InvalidTextRange);
    }
    let start = selection.start.min(limit);
    let end = selection.end.min(limit).max(start);
    let expected = format!("{}{}{}", &before[..start], plan.text, &before[end..]);
    let code_context = canvas_block_owners(document, start..start)
        .first()
        .and_then(|owner| document.canvas_block_format(owner))
        .is_some_and(|format| format.code);
    let mut candidate = document.clone();
    let caret = candidate.replace_canvas_range(selection, &plan.text)?;
    if caret != start + plan.text.len() || candidate.canvas_projection().text != expected {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    if !code_context {
        for line in &plan.lines {
            set_paste_line_format(&mut candidate, start + line.newline, &line.format)?;
        }
    }
    set_paste_inline_style(&mut candidate, start..caret, &HtmlStyle::default())?;
    for span in &plan.spans {
        set_paste_inline_style(
            &mut candidate,
            start + span.range.start..start + span.range.end,
            &span.style,
        )?;
    }
    if candidate.canvas_projection().text != expected {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    *document = candidate;
    Ok(caret)
}

pub struct RichEditor {
    tokens: DesignTokens,
    dark: bool,
    canvas_backgrounds: Option<(gpui::Hsla, gpui::Hsla)>,
    kind: RichEditorKind,
    source: String,
    origin: RichOrigin,
    document: Document,
    selected: Option<Selected>,
    metadata_path: Option<(String, bool)>,
    metadata_context: Option<MetadataContext>,
    canvas: Entity<NativeCanvas>,
    value: Entity<EditorTextInput>,
    error: Option<String>,
    history: RichHistory,
    pending_formats: Vec<PendingFormat>,
    pending_anchor: Option<(Selected, usize)>,
    unapplied_error: Option<String>,
    paste_epoch: u64,
    ui_hovered: HashSet<gpui::ElementId>,
    ui_motion: RefCell<crate::motion::Tweens<gpui::ElementId>>,
    ui_selection_motion: RefCell<crate::motion::Tweens<gpui::ElementId>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RichEditorEvent> for RichEditor {}

impl RichEditor {
    pub fn new(
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let canvas = cx.new(|cx| NativeCanvas::new(tokens.clone(), dark, window, cx));
        let value = cx.new(|cx| EditorTextInput::new_plain(tokens.clone(), dark, true, window, cx));
        canvas.update(cx, |input, cx| {
            input.set_fill_height(true, cx);
            input.set_padding(super::style::metric("richPaddingX", 15.), super::style::metric("richPaddingY", 14.), cx);
        });
        value.update(cx, |input, cx| {
            input.set_single_line_height(super::style::metric("controlHeight", 30.), cx)
        });
        let theme = tokens.theme(dark);
        value.update(cx, |input, cx| {
            input.set_colors(
                InputColors {
                    background: color(&theme.search_background),
                    foreground: color(&theme.text),
                    placeholder: color(&theme.text_muted),
                    border: color(&theme.border),
                    selection: color(&theme.selected),
                    caret: color(&theme.accent),
                },
                cx,
            );
            input.set_placeholder(
                tr(
                    "颜色、链接或图片/文件路径",
                    "Color, link or image/file path",
                ),
                cx,
            );
        });
        let subscriptions = vec![
            cx.subscribe_in(&canvas, window, |this, _, event, window, cx| match event {
                NativeCanvasEvent::Edited { range, text } => {
                    this.canvas_edited(range.clone(), text, window, cx)
                }
                NativeCanvasEvent::Paste => this.request_paste(window, cx),
                NativeCanvasEvent::BoundaryKey { backspace } => {
                    this.canvas_boundary_key(*backspace, window, cx)
                }
                NativeCanvasEvent::SelectionChanged => {
                    this.paste_epoch = this.paste_epoch.wrapping_add(1);
                    this.history.cutoff();
                    this.pending_formats.clear();
                    this.pending_anchor = None;
                    this.select_canvas_context(cx);
                    cx.notify();
                }
                NativeCanvasEvent::Save => cx.emit(RichEditorEvent::Save),
                NativeCanvasEvent::Escape => cx.emit(RichEditorEvent::Escape),
            }),
            cx.subscribe_in(&value, window, |this, _, event, _, cx| match event {
                EditorTextEvent::Save => cx.emit(RichEditorEvent::Save),
                EditorTextEvent::Escape => cx.emit(RichEditorEvent::Escape),
                EditorTextEvent::Edited { .. } => {
                    this.paste_epoch = this.paste_epoch.wrapping_add(1);
                    cx.notify();
                }
                _ => {}
            }),
        ];
        Self {
            tokens,
            dark,
            canvas_backgrounds: None,
            kind: RichEditorKind::Html,
            source: String::new(),
            origin: RichOrigin::Unknown,
            document: Document::Html(HtmlDocument::parse("")),
            selected: None,
            metadata_path: None,
            metadata_context: None,
            canvas,
            value,
            error: None,
            history: RichHistory::default(),
            pending_formats: Vec::new(),
            pending_anchor: None,
            unapplied_error: None,
            paste_epoch: 0,
            ui_hovered: HashSet::new(),
            ui_motion: RefCell::new(crate::motion::Tweens::default()),
            ui_selection_motion: RefCell::new(crate::motion::Tweens::default()),
            _subscriptions: subscriptions,
        }
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        self.canvas_backgrounds = None;
        self.ui_motion.borrow_mut().clear();
        self.ui_selection_motion.borrow_mut().clear();
        self.refresh_input_colors(cx);
    }

    pub(super) fn set_surface_backgrounds(
        &mut self,
        html: gpui::Hsla,
        im: gpui::Hsla,
        cx: &mut Context<Self>,
    ) {
        self.canvas_backgrounds = Some((html, im));
        self.refresh_input_colors(cx);
    }

    fn refresh_input_colors(&mut self, cx: &mut Context<Self>) {
        let theme = self.tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&theme.search_background),
            foreground: color(&theme.text),
            placeholder: color(&theme.text_muted),
            border: color(&theme.border),
            selection: color(&theme.selected),
            caret: color(&theme.accent),
        };
        let canvas_colors = InputColors {
            background: self
                .canvas_backgrounds
                .map_or(colors.background, |(html, im)| match self.kind {
                    RichEditorKind::Html => html,
                    RichEditorKind::Im => im,
                }),
            ..colors
        };
        let code_background = color(&theme.secondary);
        self.canvas.update(cx, |input, cx| {
            input.set_theme_colors(canvas_colors, code_background, cx)
        });
        self.value.update(cx, |input, cx| {
            input.set_theme_colors(colors, code_background, cx)
        });
        cx.notify();
    }

    pub fn is_composing(&self, cx: &App) -> bool {
        self.canvas.read(cx).is_composing() || self.value.read(cx).is_composing()
    }

    pub fn reset_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui_hovered.clear();
        self.ui_motion.borrow_mut().clear();
        self.ui_selection_motion.borrow_mut().clear();
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        self.history = RichHistory::default();
        self.pending_formats.clear();
        self.pending_anchor = None;
        self.unapplied_error = None;
        self.error = None;
        self.selected = None;
        self.metadata_path = None;
        self.metadata_context = None;
        self.value
            .update(cx, |input, cx| input.reset_text(String::new(), window, cx));
        cx.notify();
    }

    pub fn unapplied_error(&self) -> Option<&str> {
        self.unapplied_error.as_deref()
    }
    fn paste_snapshot(&self, cx: &App) -> PasteSnapshot {
        let canvas = self.canvas.read(cx);
        let (selection, reversed) = canvas.selection();
        PasteSnapshot {
            epoch: self.paste_epoch,
            kind: self.kind,
            origin: self.origin,
            source: self.source.clone(),
            text: canvas.text().into(),
            selection,
            reversed,
            unapplied: self.unapplied_error.clone(),
        }
    }
    fn request_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx)
            || matches!(self.document, Document::Invalid(_))
            || self.source != self.source_from_document()
            || !self.canvas.read(cx).focus_handle().is_focused(window)
            || self.value.read(cx).focus_handle().is_focused(window)
        {
            return;
        }
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        let request = self.paste_snapshot(cx);
        let kind = self.kind;
        let recovering = self.unapplied_error.is_some();
        let code = match &self.document {
            Document::Html(document) => {
                canvas_block_owners(document, request.selection.start..request.selection.start)
                    .first()
                    .and_then(|owner| document.canvas_block_format(owner))
                    .is_some_and(|format| format.code)
            }
            _ => false,
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // One coherent read only. No capture ordering, history insert,
                    // image persistence, or clipboard SourceURL changes are involved.
                    let Some(snapshot) = cliphost_core::clipboard::peek_snapshot() else {
                        return Ok(None);
                    };
                    let fragment = snapshot.html.as_deref().and_then(|raw| {
                        if cliphost_core::cf_html::looks_like_complete_format(raw) {
                            cliphost_core::cf_html::try_extract_fragment_and_source_url(raw)
                                .map(|(fragment, _)| fragment)
                        } else {
                            Some(raw.to_string())
                        }
                    });
                    clipboard_paste_plan(
                        kind,
                        code,
                        recovering,
                        fragment.as_deref(),
                        snapshot.text.as_deref(),
                    )
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !request.matches(&this.paste_snapshot(cx))
                    || this.is_composing(cx)
                    || matches!(this.document, Document::Invalid(_))
                    || this.source != this.source_from_document()
                    || !this.canvas.read(cx).focus_handle().is_focused(window)
                    || this.value.read(cx).focus_handle().is_focused(window)
                    || (this.unapplied_error.is_none()
                        && this.canvas.read(cx).text() != document_canvas_text(&this.document))
                {
                    return;
                }
                match result {
                    Ok(Some(ClipboardPaste::Text(text))) => {
                        // Includes IM text/plain and corrections to an existing
                        // rejected draft, using the real native range callback.
                        this.canvas.update(cx, |canvas, cx| {
                            canvas.replace_text_in_range(None, &text, window, cx)
                        });
                    }
                    Ok(Some(ClipboardPaste::Html(plan))) => {
                        let before = this.current_frame(cx);
                        let Document::Html(document) = &mut this.document else {
                            return;
                        };
                        match apply_html_paste(document, request.selection.clone(), &plan) {
                            Ok(caret) => {
                                this.pending_formats.clear();
                                this.pending_anchor = None;
                                if !matches!(
                                    this.metadata_context,
                                    Some(MetadataContext::Parameters)
                                ) {
                                    this.metadata_context = None;
                                }
                                this.error = None;
                                this.sync_canvas(caret..caret, false, cx);
                                this.publish_recorded(before, Some(TypingGroup::Canvas), cx);
                            }
                            Err(error) => {
                                this.error = Some(error.to_string());
                                cx.notify();
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
    pub(super) fn diagnostics(&self, cx: &App) -> serde_json::Value {
        serde_json::json!({
            "documentKind":match self.kind {RichEditorKind::Html=>"html",RichEditorKind::Im=>"im"},
            "documentValid":!matches!(self.document,Document::Invalid(_)),
            "composing":self.is_composing(cx),
            "unappliedError":self.unapplied_error.as_ref().map(|_|"source-edit-not-applied"),
            "hasError":self.error.is_some(),
            "history":{"canUndo":!self.history.undo.is_empty(),"canRedo":!self.history.redo.is_empty(),
                "undoDepth":self.history.undo.len(),"redoDepth":self.history.redo.len()},
            "canvas":self.canvas.read(cx).diagnostics(),
            "metadataInput":self.value.read(cx).diagnostics(),
        })
    }
    pub(super) fn set_paint_clock(
        &mut self,
        clock: std::sync::Arc<std::sync::atomic::AtomicU64>,
        cx: &mut Context<Self>,
    ) {
        self.canvas
            .update(cx, |canvas, _| canvas.set_paint_clock(clock.clone()));
        self.value
            .update(cx, |value, _| value.set_paint_clock(clock));
    }
    pub(super) fn content_qa_diagnostics(
        &self,
        expected_source: &str,
        cx: &App,
    ) -> serde_json::Value {
        if self.source != expected_source {
            return serde_json::json!({"available":false,"reason":"rich-source-not-current"});
        }
        serde_json::json!({"available":true,"canonicalSource":self.source_from_document(),
            "canvasProjection":document_canvas_text(&self.document),"canvasText":self.canvas.read(cx).text(),
            "metadataDraft":self.value.read(cx).text(),"unappliedError":self.unapplied_error,"error":self.error})
    }
    pub(super) fn apply_qa_history(
        &mut self,
        redo: bool,
        expected_frame: u64,
        expected_source: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.is_composing(cx) || matches!(self.document, Document::Invalid(_)) {
            return Err("Rich editor has an invalid draft or is composing.".into());
        }
        if self.source != expected_source || self.source_from_document() != expected_source {
            return Err("Rich source no longer matches the current editor draft.".into());
        }
        let canvas = self.canvas.read(cx);
        if !canvas.paint_is_current(expected_frame) {
            return Err("Rich canvas paint or selection changed.".into());
        }
        if !canvas.focus_handle().is_focused(window)
            || self.value.read(cx).focus_handle().is_focused(window)
        {
            return Err("Rich canvas is not the current logical keyboard target.".into());
        }
        if self.unapplied_error.is_some() {
            if redo {
                return Err("A rejected edit cannot be redone.".into());
            }
            validate_unapplied_recovery_snapshot(
                &self.document,
                &self.source,
                canvas.text(),
                canvas.selection().0,
            )?;
            let source = self.source.clone();
            let depths = (self.history.undo.len(), self.history.redo.len());
            // This is the same logical Ctrl+Z branch as normal key_down, not a
            // direct flag clear, proposed-source write, or physical key test.
            self.history_step(false, window, cx);
            if self.source != source
                || self.source_from_document() != source
                || self.unapplied_error.is_some()
                || self.error.is_some()
                || self.canvas.read(cx).text() != document_canvas_text(&self.document)
                || depths != (self.history.undo.len(), self.history.redo.len())
            {
                return Err("Rejected edit did not restore the applied canvas unchanged.".into());
            }
            return Ok(());
        }
        let projection = document_canvas_text(&self.document);
        let (selection, _) = canvas.selection();
        if canvas.text() != projection
            || selection.start > selection.end
            || selection.end > projection.len()
            || !projection.is_char_boundary(selection.start)
            || !projection.is_char_boundary(selection.end)
        {
            return Err("Rich canvas selection does not match the current document.".into());
        }
        if if redo {
            self.history.redo.is_empty()
        } else {
            self.history.undo.is_empty()
        } {
            return Err("Requested rich history step is unavailable.".into());
        }
        self.history_step(redo, window, cx);
        Ok(())
    }

    fn source_from_document(&self) -> String {
        match &self.document {
            Document::Html(document) => document.source().to_string(),
            Document::Im(document) => document.serialize(),
            Document::Invalid(_) => self.source.clone(),
        }
    }

    fn current_frame(&self, cx: &App) -> HistoryFrame {
        let (range, reversed) = self.canvas.read(cx).selection();
        let cursor = if reversed { range.start } else { range.end };
        HistoryFrame {
            counterpart_caret_utf16: None,
            document: self.document.clone(),
            source: self.source.clone(),
            selected: Some(Selected::Canvas { range, reversed }),
            caret_utf16: to_utf16(self.canvas.read(cx).text(), cursor..cursor).start,
        }
    }

    fn sync_canvas(&mut self, selection: Range<usize>, reversed: bool, cx: &mut Context<Self>) {
        if self.canvas.read(cx).is_composing()
            || self.value.read(cx).is_composing()
            || self.unapplied_error.is_some()
        {
            return;
        }
        let mut fragments = Vec::new();
        let mut atoms = Vec::new();
        let mut paragraphs = Vec::new();
        let text = match &self.document {
            Document::Html(document) => {
                let projection = document.canvas_projection();
                let mut paragraph_start = 0;
                for span in &projection.spans {
                    match &span.kind {
                        HtmlCanvasKind::Text { run } => fragments.push(CanvasFragment {
                            range: span.range.clone(),
                            style: document.runs()[*run].style.clone(),
                        }),
                        HtmlCanvasKind::Image { index } => {
                            let image = &document.images()[*index];
                            atoms.push(CanvasAtom {
                                range: span.range.clone(),
                                kind: "image",
                                html_image_layout: Some(image.display_layout()),
                                path: (image.reference() == HtmlImageReference::Local)
                                    .then(|| Self::local_image(&image.src))
                                    .flatten(),
                                label: format!("🖼 {}", image.src),
                            });
                        }
                        HtmlCanvasKind::Boundary { left, .. } => {
                            paragraphs.push(CanvasParagraph {
                                range: paragraph_start..span.range.end,
                                format: document.canvas_block_format(left),
                            });
                            paragraph_start = span.range.end;
                        }
                        HtmlCanvasKind::Break { .. } => {}
                    }
                }
                projection.text
            }
            Document::Im(document) => {
                let projection = im_canvas_projection(document);
                for span in &projection.spans {
                    match span.kind {
                        ImCanvasKind::Text(_) => fragments.push(CanvasFragment {
                            range: span.range.clone(),
                            style: HtmlStyle::default(),
                        }),
                        ImCanvasKind::Atom { id, protected } => {
                            let node = document.node(id).expect("canvas node");
                            let label = match node.kind() {
                                RichNodeKind::Image => format!("🖼 {}", node.path()),
                                RichNodeKind::File => {
                                    let name = node
                                        .path()
                                        .rsplit(['/', '\\'])
                                        .next()
                                        .unwrap_or(node.path());
                                    if name.is_empty() {
                                        tr("[文件]", "[File]").to_string()
                                    } else {
                                        name.to_string()
                                    }
                                }
                                RichNodeKind::Placeholder => {
                                    if node.text().is_empty() {
                                        tr("[表情]", "[Emoji]").to_string()
                                    } else {
                                        node.text().to_string()
                                    }
                                }
                                _ => tr("保留的 XML 对象", "Preserved XML object").to_string(),
                            };
                            atoms.push(CanvasAtom {
                                range: span.range.clone(),
                                kind: match node.kind() {
                                    RichNodeKind::Image => "image",
                                    RichNodeKind::File => "file",
                                    RichNodeKind::Placeholder => "placeholder",
                                    _ => "protected",
                                },
                                html_image_layout: None,
                                path: (!protected && node.kind() == RichNodeKind::Image)
                                    .then(|| Self::local_image(node.path()))
                                    .flatten(),
                                label,
                            });
                        }
                    }
                }
                projection.text
            }
            Document::Invalid(_) => String::new(),
        };
        self.canvas.update(cx, |canvas, cx| {
            canvas.set_read_only(matches!(self.document, Document::Invalid(_)), cx);
            canvas.set_terminal_newline(self.kind == RichEditorKind::Html, cx);
            canvas.sync_document(text, fragments, atoms, paragraphs, selection, reversed, cx);
        });
        self.select_canvas_context(cx);
    }

    fn sync_current_canvas(&mut self, cx: &mut Context<Self>) {
        let (range, reversed) = self.canvas.read(cx).selection();
        self.sync_canvas(range, reversed, cx);
    }

    fn select_canvas_context(&mut self, cx: &mut Context<Self>) {
        if self.canvas.read(cx).is_composing() || self.unapplied_error.is_some() {
            return;
        }
        let (range, reversed) = self.canvas.read(cx).selection();
        let cursor = if reversed { range.start } else { range.end };
        let mut path = String::new();
        self.selected = match &self.document {
            Document::Html(document) => {
                document.canvas_projection().spans.iter().find_map(|span| {
                    let included = if range.is_empty() {
                        span.range.start <= cursor && cursor <= span.range.end
                    } else {
                        range.start < span.range.end && range.end > span.range.start
                    };
                    if !included {
                        return None;
                    }
                    match span.kind {
                        HtmlCanvasKind::Text { run } => {
                            path = document.runs()[run].style.link.clone().unwrap_or_default();
                            Some(Selected::HtmlText(
                                document.runs()[run].source_range.clone(),
                            ))
                        }
                        HtmlCanvasKind::Image { index } => {
                            path = document.images()[index].src.clone();
                            Some(Selected::HtmlImage(index))
                        }
                        _ => None,
                    }
                })
            }
            Document::Im(document) => {
                im_canvas_projection(document)
                    .spans
                    .iter()
                    .find_map(|span| {
                        let included = if range.is_empty() {
                            span.range.start <= cursor && cursor <= span.range.end
                        } else {
                            range.start < span.range.end && range.end > span.range.start
                        };
                        if !included {
                            return None;
                        }
                        let id = match span.kind {
                            ImCanvasKind::Text(id) | ImCanvasKind::Atom { id, .. } => id,
                        };
                        path = document
                            .node(id)
                            .map(|node| node.path().to_string())
                            .unwrap_or_default();
                        Some(Selected::Im(id))
                    })
            }
            Document::Invalid(_) => None,
        };
        let context = match (&self.document, &self.selected) {
            (_, Some(Selected::HtmlImage(index))) => MetadataContext::HtmlImage(*index),
            (Document::Html(_), Some(Selected::HtmlText(range))) if !path.is_empty() => {
                MetadataContext::HtmlLink(range.clone())
            }
            (Document::Im(document), Some(Selected::Im(id)))
                if document.node(*id).is_some_and(|node| node.can_edit_path()) =>
            {
                MetadataContext::ImPath(*id)
            }
            _ => MetadataContext::Parameters,
        };
        if let Some((path, reset)) = metadata_field_sync(
            self.metadata_context.as_ref(),
            &context,
            self.value.read(cx).text(),
            &path,
        ) {
            let pending_reset = self.metadata_path.as_ref().is_some_and(|(_, reset)| *reset);
            self.metadata_path = Some((path, reset || pending_reset));
        }
        self.metadata_context = Some(context);
    }

    pub fn focus_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.canvas.read(cx).focus_handle());
    }

    pub fn set_source(
        &mut self,
        kind: RichEditorKind,
        source: String,
        origin: RichOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let padding = match kind {
            RichEditorKind::Html => (super::style::metric("richPaddingX", 15.), super::style::metric("richPaddingY", 14.)),
            RichEditorKind::Im => (super::style::metric("imContentPadding", 14.), super::style::metric("imContentPadding", 14.)),
        };
        self.canvas.update(cx, |input, cx| input.set_padding(padding.0, padding.1, cx));
        if self.kind == kind && self.source == source && self.origin == origin {
            self.sync_current_canvas(cx);
            return;
        }
        let kind_changed = self.kind != kind;
        self.kind = kind;
        if kind_changed {
            self.refresh_input_colors(cx);
        }
        self.source = source;
        self.origin = origin;
        // An incoming source change (for example from the source tab or path
        // normalization after saving) was not recorded in this document history.
        // Do not let an older whole-source snapshot roll it back.
        self.history = RichHistory::default();
        self.pending_formats.clear();
        self.pending_anchor = None;
        self.metadata_path = None;
        self.metadata_context = None;
        self.value
            .update(cx, |input, cx| input.reset_text(String::new(), window, cx));
        self.document = match kind {
            RichEditorKind::Html => Document::Html(HtmlDocument::parse(self.source.clone())),
            RichEditorKind::Im => match ImRichDocument::parse(&self.source, origin) {
                Ok(document) => Document::Im(document),
                Err(error) => Document::Invalid(error.to_string()),
            },
        };
        self.selected = None;
        self.error = None;
        self.sync_canvas(0..0, false, cx);
        cx.notify();
    }

    fn publish(&mut self, cx: &mut Context<Self>) {
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        self.source = self.source_from_document();
        self.error = None;
        cx.emit(RichEditorEvent::Changed(self.source.clone()));
        cx.notify();
    }

    fn publish_recorded(
        &mut self,
        before: HistoryFrame,
        typing: Option<TypingGroup>,
        cx: &mut Context<Self>,
    ) {
        if self.source_from_document() != before.source {
            self.history.record(before, typing, Instant::now());
            self.publish(cx);
        }
    }

    fn canvas_edited(
        &mut self,
        range: Range<usize>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        let recovering = self.unapplied_error.is_some();
        // Backspace at the beginning, Delete at the end, or a cancelled empty
        // composition must not consume a format waiting for actual insertion.
        if !recovering && range.is_empty() && text.is_empty() {
            return;
        }
        let mut before = self.current_frame(cx);
        let original = document_canvas_text(&self.document);
        let (range, inserted) = if recovering {
            let proposed = self.canvas.read(cx).text().to_string();
            let (old, new) = changed_text_ranges(&original, &proposed);
            (old, proposed[new].to_string())
        } else {
            (range, text.to_string())
        };
        if recovering && original == self.canvas.read(cx).text() {
            self.unapplied_error = None;
            self.error = None;
            self.sync_current_canvas(cx);
            self.publish(cx);
            return;
        }
        before.selected = Some(Selected::Canvas {
            range: range.clone(),
            reversed: false,
        });
        before.caret_utf16 = to_utf16(&original, range.end..range.end).end;
        let pending = self
            .pending_anchor
            .as_ref()
            .filter(|(_, caret)| range.is_empty() && *caret == range.start)
            .map(|_| self.pending_formats.clone())
            .unwrap_or_default();
        let (native_selection, native_reversed) = self.canvas.read(cx).selection();
        let mut candidate = self.document.clone();
        let result = match &mut candidate {
            Document::Html(document) => document
                .replace_canvas_range(range.clone(), &inserted)
                .map_err(|error| error.to_string()),
            Document::Im(document) => replace_im_canvas_range(document, range.clone(), &inserted),
            Document::Invalid(error) => Err(error.clone()),
        }
        .and_then(|caret| {
            if let Document::Html(document) = &mut candidate {
                let start = caret.saturating_sub(canvas_text(&inserted).replace('\r', "\n").len());
                for format in &pending {
                    apply_canvas_inline_mark(
                        document,
                        start..caret,
                        format.mark.clone(),
                        format.enabled,
                    )
                    .map_err(|error| error.to_string())?;
                }
            }
            Ok(caret)
        });
        match result {
            Ok(caret) => {
                let normalized = if self.kind == RichEditorKind::Html {
                    canvas_text(&inserted).replace('\r', "\n")
                } else {
                    canvas_text(&inserted)
                };
                let canonical_start = caret.saturating_sub(normalized.len());
                let acknowledged = acknowledge_native_selection(
                    native_selection,
                    range.start,
                    &inserted,
                    canonical_start,
                    &document_canvas_text(&candidate),
                    self.kind == RichEditorKind::Html,
                );
                self.document = candidate;
                if !matches!(self.metadata_context, Some(MetadataContext::Parameters)) {
                    self.metadata_context = None;
                }
                self.unapplied_error = None;
                self.error = None;
                self.pending_formats.clear();
                self.pending_anchor = None;
                self.sync_canvas(acknowledged, native_reversed, cx);
                let unchanged = self.source_from_document() == before.source;
                self.publish_recorded(before, Some(TypingGroup::Canvas), cx);
                if recovering && unchanged {
                    self.publish(cx);
                }
                cx.notify();
            }
            Err(error) => {
                self.pending_formats.clear();
                self.pending_anchor = None;
                self.unapplied_error = Some(error.clone());
                self.error = Some(error.clone());
                cx.emit(RichEditorEvent::Unapplied(error));
                cx.notify();
            }
        }
    }

    fn styles(&self, cx: &App) -> Vec<HtmlStyle> {
        let Document::Html(document) = &self.document else {
            return Vec::new();
        };
        canvas_style_at(document, self.canvas.read(cx).selection().0)
    }

    fn canvas_boundary_key(
        &mut self,
        backspace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let (range, _) = self.canvas.read(cx).selection();
        if !range.is_empty() {
            return;
        }
        let before = self.current_frame(cx);
        let Document::Html(document) = &mut self.document else {
            return;
        };
        let result = document.canvas_keyboard_edit(
            range.start,
            if backspace {
                HtmlCanvasKey::Backspace
            } else {
                HtmlCanvasKey::Enter
            },
        );
        match result {
            Ok(Some(caret)) => {
                if self.pending_anchor.is_some() {
                    self.pending_anchor = Some((
                        Selected::Canvas {
                            range: caret..caret,
                            reversed: false,
                        },
                        caret,
                    ));
                }
                self.sync_canvas(caret..caret, false, cx);
                self.publish_recorded(before, None, cx);
            }
            Ok(None) => {
                let (selected, reversed) = self.canvas.read(cx).selection();
                self.canvas.update(cx, |canvas, cx| {
                    canvas.set_selection(selected, reversed, cx)
                });
                // Only a normal Enter reaches this fallback; HTML Backspace
                // special commands are announced only for an active list/indent.
                if !backspace {
                    self.canvas.update(cx, |canvas, cx| {
                        canvas.replace_text_in_range(None, "\n", window, cx)
                    });
                }
            }
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
        window.focus(&self.canvas.read(cx).focus_handle());
    }

    fn mark_active(&self, mark: &HtmlMark, cx: &App) -> bool {
        if self.canvas.read(cx).selection().0.is_empty() {
            if let Some(setting) = self
                .pending_formats
                .iter()
                .find(|setting| same_format_family(&setting.mark, mark))
            {
                return setting.enabled && setting.mark == *mark;
            }
        }
        let styles = self.styles(cx);
        !styles.is_empty() && styles.iter().all(|style| format_active(style, mark))
    }

    fn block_active(&self, mark: HtmlBlockMark, cx: &App) -> bool {
        let Document::Html(document) = &self.document else {
            return false;
        };
        let owners = canvas_block_owners(document, self.canvas.read(cx).selection().0);
        !owners.is_empty()
            && owners.iter().all(|owner| {
                document
                    .canvas_block_format(owner)
                    .is_some_and(|block| block_format_active(&block, mark))
            })
    }

    fn mark(&mut self, mark: HtmlMark, window: &mut Window, cx: &mut Context<Self>) {
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let (range, reversed) = self.canvas.read(cx).selection();
        let enabled = match &mark {
            HtmlMark::Color(value) | HtmlMark::Background(value) if !value.trim().is_empty() => {
                true
            }
            HtmlMark::Color(value) | HtmlMark::Background(value) | HtmlMark::Link(value)
                if value.trim().is_empty() =>
            {
                false
            }
            _ => !self.mark_active(&mark, cx),
        };
        if enabled {
            let mut probe = HtmlDocument::parse("<p>x</p>");
            if let Err(error) = probe.wrap_text(0, mark.clone()) {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        }
        if range.is_empty() {
            let anchor = (
                Selected::Canvas {
                    range: range.clone(),
                    reversed,
                },
                range.start,
            );
            if self.pending_anchor.as_ref() != Some(&anchor) {
                self.pending_formats.clear();
            }
            self.pending_formats
                .retain(|setting| !same_format_family(&setting.mark, &mark));
            self.pending_formats.push(PendingFormat { mark, enabled });
            self.pending_anchor = Some(anchor);
            self.history.cutoff();
            self.error = None;
        } else {
            let before = self.current_frame(cx);
            let Document::Html(document) = &mut self.document else {
                return;
            };
            match apply_canvas_inline_mark(document, range.clone(), mark, enabled) {
                Ok(()) => {
                    self.sync_canvas(range, reversed, cx);
                    self.publish_recorded(before, None, cx);
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        window.focus(&self.canvas.read(cx).focus_handle());
        cx.notify();
    }

    fn format_block(
        &mut self,
        mark: Option<HtmlBlockMark>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let (range, reversed) = self.canvas.read(cx).selection();
        if mark.is_none() && range.is_empty() {
            self.pending_formats = [
                HtmlMark::Bold,
                HtmlMark::Italic,
                HtmlMark::Underline,
                HtmlMark::Strike,
                HtmlMark::Subscript,
                HtmlMark::Superscript,
                HtmlMark::Code,
                HtmlMark::Color(String::new()),
                HtmlMark::Background(String::new()),
                HtmlMark::Link(String::new()),
            ]
            .into_iter()
            .map(|mark| PendingFormat {
                mark,
                enabled: false,
            })
            .collect();
            self.pending_anchor = Some((
                Selected::Canvas {
                    range: range.clone(),
                    reversed,
                },
                range.start,
            ));
            self.history.cutoff();
            self.error = None;
        } else {
            let before = self.current_frame(cx);
            let Document::Html(document) = &mut self.document else {
                return;
            };
            let result = if let Some(mark) = mark {
                apply_canvas_block_mark(document, range.clone(), mark)
            } else {
                clean_canvas_selection(document, range.clone())
            };
            match result {
                Ok(()) => {
                    self.sync_canvas(range, reversed, cx);
                    self.publish_recorded(before, None, cx);
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        window.focus(&self.canvas.read(cx).focus_handle());
        cx.notify();
    }

    fn history_step(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) {
            return;
        }
        if self.unapplied_error.is_some() {
            if redo {
                return;
            }
            let caret = self.canvas.read(cx).selection().0.start;
            self.unapplied_error = None;
            self.error = None;
            self.history.cutoff();
            self.sync_canvas(caret..caret, false, cx);
            self.publish(cx);
            window.focus(&self.canvas.read(cx).focus_handle());
            return;
        }
        let current = self.current_frame(cx);
        let restored = if redo {
            self.history.redo(current)
        } else {
            self.history.undo(current)
        };
        let Some(frame) = restored else {
            return;
        };
        self.document = frame.document;
        self.source = frame.source;
        self.metadata_context = None;
        self.pending_formats.clear();
        self.pending_anchor = None;
        let projection = document_canvas_text(&self.document);
        let caret = from_utf16(&projection, frame.caret_utf16..frame.caret_utf16).start;
        self.sync_canvas(caret..caret, false, cx);
        self.publish(cx);
        window.focus(&self.canvas.read(cx).focus_handle());
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if self.is_composing(cx)
            || event.is_held
            || modifiers.alt
            || modifiers.platform
            || self.value.read(cx).focus_handle().is_focused(window)
        {
            return;
        }
        if event.keystroke.key == "tab"
            && !modifiers.control
            && self.kind == RichEditorKind::Html
            && self.canvas.read(cx).focus_handle().is_focused(window)
        {
            if self.canvas_tab(modifiers.shift, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if event.keystroke.key == "space"
            && !modifiers.control
            && !modifiers.shift
            && self.kind == RichEditorKind::Html
            && self.canvas.read(cx).focus_handle().is_focused(window)
        {
            if self.canvas_list_autofill(cx) {
                // On Windows a consumed keydown prevents WM_CHAR. A non-match
                // keeps the normal native character/selection replacement path.
                cx.stop_propagation();
            }
            return;
        }
        if !modifiers.control {
            return;
        }
        match event.keystroke.key.as_str() {
            "z" => {
                cx.stop_propagation();
                self.history_step(modifiers.shift, window, cx);
            }
            "y" if !modifiers.shift => {
                cx.stop_propagation();
                self.history_step(true, window, cx);
            }
            "b" | "i" | "u" if !modifiers.shift && self.kind == RichEditorKind::Html => {
                cx.stop_propagation();
                self.mark(
                    match event.keystroke.key.as_str() {
                        "b" => HtmlMark::Bold,
                        "i" => HtmlMark::Italic,
                        _ => HtmlMark::Underline,
                    },
                    window,
                    cx,
                );
            }
            _ => {}
        }
    }
    fn canvas_list_autofill(&mut self, cx: &mut Context<Self>) -> bool {
        if self.canvas.read(cx).is_read_only()
            || self.unapplied_error.is_some()
            || self.source != self.source_from_document()
            || self.canvas.read(cx).text() != document_canvas_text(&self.document)
        {
            return false;
        }
        let selection = self.canvas.read(cx).selection().0;
        let Document::Html(document) = &self.document else {
            return false;
        };
        let plan = match html_list_autofill::plan_html_list_autofill(
            document,
            selection.clone(),
            " ",
            true,
        ) {
            Ok(Some(plan)) => plan,
            Ok(None) => return false,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return true;
            }
        };
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        let pending = self
            .pending_anchor
            .as_ref()
            .filter(|(_, caret)| selection.is_empty() && *caret == selection.start)
            .map(|_| self.pending_formats.as_slice())
            .unwrap_or_default();
        let edit = match list_autofill_apply::apply_html_list_autofill(document, &plan, pending) {
            Ok(edit) => edit,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return true;
            }
        };
        // Stage both complete source snapshots before changing the live editor.
        // Only the final transaction is published; Undo can restore the exact
        // intermediate marker+space, including pending inline formatting.
        let before = self.current_frame(cx);
        let space_text = edit.space_document.canvas_projection().text;
        let final_text = edit.document.canvas_projection().text;
        let space = HistoryFrame {
            counterpart_caret_utf16: None,
            source: edit.space_document.source().to_string(),
            document: Document::Html(edit.space_document),
            selected: Some(Selected::Canvas {
                range: edit.undo_caret..edit.undo_caret,
                reversed: false,
            }),
            caret_utf16: to_utf16(&space_text, edit.undo_caret..edit.undo_caret).start,
        };
        let final_caret = to_utf16(&final_text, edit.redo_caret..edit.redo_caret).start;
        self.history
            .record_list_autofill(before, space, final_caret, Instant::now());
        self.document = Document::Html(edit.document);
        self.pending_formats.clear();
        self.pending_anchor = None;
        if !matches!(self.metadata_context, Some(MetadataContext::Parameters)) {
            self.metadata_context = None;
        }
        self.error = None;
        self.sync_canvas(edit.selection, false, cx);
        self.publish(cx);
        true
    }

    fn canvas_tab(&mut self, shift: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.canvas.read(cx).is_read_only() {
            return false;
        }
        self.paste_epoch = self.paste_epoch.wrapping_add(1);
        if self.unapplied_error.is_some() {
            // Ordinary input can still correct a rejected native proposal.
            // Structural shortcuts cannot format its retained valid document.
            if shift {
                return false;
            }
            self.history.cutoff();
            self.canvas.update(cx, |canvas, cx| {
                canvas.replace_text_in_range(None, "\t", window, cx)
            });
            self.history.cutoff();
            return true;
        }
        if matches!(self.document, Document::Invalid(_))
            || self.source != self.source_from_document()
            || self.canvas.read(cx).text() != document_canvas_text(&self.document)
        {
            return false;
        }
        let (selection, reversed) = self.canvas.read(cx).selection();
        let Document::Html(document) = &self.document else {
            return false;
        };
        let plan = match html_tab::plan_html_tab(document, selection, shift) {
            Ok(Some(plan)) => plan,
            Ok(None) => return false,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return true;
            }
        };
        let before = self.current_frame(cx);
        let Document::Html(document) = &mut self.document else {
            return false;
        };
        match tab_apply::apply_html_tab_plan(document, &plan) {
            Ok(()) => {
                if plan.history_cutoff {
                    self.history.cutoff();
                }
                self.pending_formats.clear();
                self.pending_anchor = None;
                if !matches!(self.metadata_context, Some(MetadataContext::Parameters)) {
                    self.metadata_context = None;
                }
                self.error = None;
                let reversed = reversed && !plan.selection.is_empty();
                self.sync_canvas(plan.selection, reversed, cx);
                self.publish_recorded(before, Some(TypingGroup::Canvas), cx);
                if plan.history_cutoff {
                    self.history.cutoff();
                }
            }
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
        true
    }

    fn apply_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let before = self.current_frame(cx);
        let path = self.value.read(cx).text().to_string();
        let result = match (&mut self.document, &self.selected) {
            (Document::Html(document), Some(Selected::HtmlImage(index))) => document
                .replace_image_reference(*index, &path)
                .map_err(|error| error.to_string()),
            (Document::Im(document), Some(Selected::Im(id))) => document
                .set_path(*id, &path)
                .map_err(|error| error.to_string()),
            _ => return,
        };
        match result {
            Ok(()) => {
                self.sync_current_canvas(cx);
                self.publish_recorded(before, None, cx);
            }
            Err(error) => self.error = Some(error),
        }
        window.focus(&self.canvas.read(cx).focus_handle());
        cx.notify();
    }

    fn add_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        window.focus(&self.canvas.read(cx).focus_handle());
    }

    fn add_atom(&mut self, image: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let before = self.current_frame(cx);
        let path = self.value.read(cx).text().to_string();
        let (range, _) = self.canvas.read(cx).selection();
        let result = match &mut self.document {
            Document::Im(document) => {
                im_insert::insert_im_canvas_atom(document, range, image, &path)
            }
            Document::Html(document) if image => document
                .insert_canvas_image(range, &path, "")
                .map_err(|error| error.to_string()),
            _ => return,
        };
        match result {
            Ok(caret) => {
                self.metadata_context = None;
                self.sync_canvas(caret..caret, false, cx);
                self.publish_recorded(before, None, cx);
            }
            Err(error) => self.error = Some(error),
        }
        window.focus(&self.canvas.read(cx).focus_handle());
        cx.notify();
    }

    fn remove_atom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let before = self.current_frame(cx);
        let caret = match (&self.document, &self.selected) {
            (Document::Html(document), Some(Selected::HtmlImage(selected))) => document
                .canvas_projection()
                .spans
                .iter()
                .find_map(|span| match span.kind {
                    HtmlCanvasKind::Image { index } if index == *selected => Some(span.range.start),
                    _ => None,
                }),
            (Document::Im(document), Some(Selected::Im(selected))) => {
                im_canvas_projection(document)
                    .spans
                    .iter()
                    .find_map(|span| match span.kind {
                        ImCanvasKind::Atom { id, .. } if id == *selected => Some(span.range.start),
                        _ => None,
                    })
            }
            _ => None,
        };
        let Some(caret) = caret else {
            return;
        };
        let result = match (&mut self.document, &self.selected) {
            (Document::Html(document), Some(Selected::HtmlImage(index))) => document
                .remove_image(*index)
                .map_err(|error| error.to_string()),
            (Document::Im(document), Some(Selected::Im(id)))
                if document.node(*id).is_some_and(|node| {
                    matches!(
                        node.kind(),
                        RichNodeKind::Image | RichNodeKind::File | RichNodeKind::Placeholder
                    )
                }) =>
            {
                document.remove(*id).map_err(|error| error.to_string())
            }
            _ => return,
        };
        match result {
            Ok(()) => {
                self.selected = None;
                self.metadata_path = Some((String::new(), true));
                self.metadata_context = None;
                self.sync_canvas(caret..caret, false, cx);
                self.publish_recorded(before, None, cx);
            }
            Err(error) => self.error = Some(error),
        }
        window.focus(&self.canvas.read(cx).focus_handle());
        cx.notify();
    }

    fn move_atom(&mut self, direction: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) || self.unapplied_error.is_some() {
            return;
        }
        let before = self.current_frame(cx);
        let (Document::Im(document), Some(Selected::Im(id))) = (&mut self.document, &self.selected)
        else {
            return;
        };
        if !document.node(*id).is_some_and(|node| {
            matches!(
                node.kind(),
                RichNodeKind::Image | RichNodeKind::File | RichNodeKind::Placeholder
            )
        }) {
            return;
        }
        let nodes = document
            .content_nodes()
            .map(|node| node.id())
            .collect::<Vec<_>>();
        let Some(index) = nodes.iter().position(|node| node == id) else {
            return;
        };
        let target = if direction < 0 && index > 0 {
            Some(nodes[index - 1])
        } else if direction > 0 && index + 1 < nodes.len() {
            nodes.get(index + 2).copied()
        } else {
            return;
        };
        match document.move_before(*id, target) {
            Ok(()) => {
                self.sync_current_canvas(cx);
                self.publish_recorded(before, None, cx);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        window.focus(&self.canvas.read(cx).focus_handle());
        cx.notify();
    }

    fn local_image(reference: &str) -> Option<PathBuf> {
        let trimmed = reference.trim();
        if !trimmed.to_ascii_lowercase().starts_with("file:///") {
            let path = PathBuf::from(trimmed);
            return path.is_absolute().then_some(path);
        }
        let value = &trimmed[8..];
        let mut decoded = Vec::new();
        let bytes = value.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' && index + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        let path = PathBuf::from(String::from_utf8(decoded).ok()?);
        path.is_absolute().then_some(path)
    }

    fn button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.button_selected(id, label, false, cx)
    }

    fn button_selected(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        selected: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = self.tokens.theme(self.dark);
        let id = id.into();
        let now = Instant::now();
        let hover = self.ui_motion.borrow_mut().value(id.clone(), if self.ui_hovered.contains(&id) { 1. } else { 0. }, crate::motion::HOVER, crate::motion::EASE, now);
        let active = self.ui_selection_motion.borrow_mut().value(id.clone(), if selected { 1. } else { 0. }, crate::motion::SURFACE, crate::motion::EASE, now);
        let normal = crate::motion::mix_hsla(color(&theme.secondary), color(&theme.selected), active);
        let background = crate::motion::mix_hsla(normal, color(&theme.hover), hover);
        rsx! {
            <div id={id.clone()} flex flex_none items_center justify_center
                h={px(super::style::metric("richToolHeight", 25.))}
                min_w={px(super::style::metric("richToolWidth", 27.))}
                px={px(super::style::metric("richToolPaddingX", 4.))}
                py={px(super::style::metric("richToolPaddingY", 3.))}
                rounded={px(super::style::metric("richToolRadius", 3.))}
                text_size={px(self.tokens.typography.small_font_size)} bg={background}
                cursor_pointer />
        }
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                if *hovered { this.ui_hovered.insert(id.clone()); }
                else { this.ui_hovered.remove(&id); }
                cx.notify();
            }))
            .child(label.into())
    }
}

impl Render for RichEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((path, reset)) = self.metadata_path.take() {
            if self.value.read(cx).is_composing()
                || !reset && self.value.read(cx).focus_handle().is_focused(window)
            {
                self.metadata_path = Some((path, reset));
            } else if reset || self.value.read(cx).text() != path {
                self.value
                    .update(cx, |input, cx| input.reset_text(path, window, cx));
            }
        }
        let theme = self.tokens.theme(self.dark);
        let mut toolbar = rsx! {
            <div flex flex_none flex_wrap
                gap={px(super::style::metric("richToolbarGroupGap", 6.))}
                px={px(super::style::metric("richToolbarPaddingX", 3.))}
                py={px(super::style::metric("richToolbarPaddingY", 5.))} border_1
                border_color={color(&theme.border)}
                rounded={px(super::style::metric("contentControlRadius", 5.))} />
        }
            .when(self.kind == RichEditorKind::Html, |toolbar| {
                toolbar.bg(color(&theme.secondary))
            });
        if self.kind == RichEditorKind::Html {
            for (label, mark) in [
                ("B", HtmlMark::Bold),
                ("I", HtmlMark::Italic),
                ("U", HtmlMark::Underline),
                ("S", HtmlMark::Strike),
                ("x₂", HtmlMark::Subscript),
                ("x²", HtmlMark::Superscript),
            ] {
                toolbar = toolbar.child(
                    self.button_selected(label, label, self.mark_active(&mark, cx), cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.mark(mark.clone(), window, cx)
                        })),
                );
            }
            for (id, label, kind) in [
                ("rich-color", tr("文字色", "Color"), 0),
                ("rich-bg", tr("背景色", "Background"), 1),
                ("rich-link", tr("链接", "Link"), 2),
            ] {
                toolbar = toolbar.child(self.button(id, label, cx).on_click(cx.listener(
                    move |this, _, window, cx| {
                        let value = this.value.read(cx).text().to_string();
                        this.mark(
                            match kind {
                                0 => HtmlMark::Color(value),
                                1 => HtmlMark::Background(value),
                                _ => HtmlMark::Link(value),
                            },
                            window,
                            cx,
                        )
                    },
                )));
            }
            for (label, mark) in [
                ("P", HtmlBlockMark::Paragraph),
                ("H1", HtmlBlockMark::Header(1)),
                ("H2", HtmlBlockMark::Header(2)),
                ("H3", HtmlBlockMark::Header(3)),
                ("1.", HtmlBlockMark::List(HtmlListKind::Ordered)),
                ("•", HtmlBlockMark::List(HtmlListKind::Bullet)),
                ("❝", HtmlBlockMark::Blockquote),
                ("pre", HtmlBlockMark::CodeBlock),
            ] {
                toolbar = toolbar.child(
                    self.button_selected(label, label, self.block_active(mark, cx), cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.format_block(Some(mark), window, cx)
                        })),
                );
            }
            for (id, label, direction) in
                [("rich-outdent", "←", -1isize), ("rich-indent", "→", 1isize)]
            {
                toolbar = toolbar.child(self.button(id, label, cx).on_click(cx.listener(
                    move |this, _, window, cx| {
                        let Document::Html(document) = &this.document else {
                            return;
                        };
                        let level =
                            common_canvas_indent(document, this.canvas.read(cx).selection().0);
                        this.format_block(
                            Some(HtmlBlockMark::Indent(
                                (isize::from(level) + direction).clamp(0, 8) as u8,
                            )),
                            window,
                            cx,
                        );
                    },
                )));
            }
            toolbar = toolbar.child(
                self.button("rich-clean", tr("清除格式", "Clear format"), cx)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.format_block(None, window, cx)),
                    ),
            );
        } else {
            toolbar =
                toolbar
                    .child(
                        self.button("im-text", tr("添加文本", "Add text"), cx)
                            .on_click(cx.listener(|this, _, window, cx| this.add_text(window, cx))),
                    )
                    .child(self.button("im-up", "↑", cx).on_click(
                        cx.listener(|this, _, window, cx| this.move_atom(-1, window, cx)),
                    ))
                    .child(self.button("im-down", "↓", cx).on_click(
                        cx.listener(|this, _, window, cx| this.move_atom(1, window, cx)),
                    ));
        }
        let mut atoms = rsx! {
            <div flex flex_none flex_wrap gap={px(super::style::metric("contentControlGap", 5.))} />
        }
            .child(
                self.button("rich-image", tr("添加图片引用", "Add image reference"), cx)
                    .on_click(cx.listener(|this, _, window, cx| this.add_atom(true, window, cx))),
            )
            .child(
                self.button("rich-path", tr("更新引用", "Update reference"), cx)
                    .on_click(cx.listener(|this, _, window, cx| this.apply_path(window, cx))),
            )
            .child(
                self.button("rich-delete", tr("删除对象", "Remove object"), cx)
                    .on_click(cx.listener(|this, _, window, cx| this.remove_atom(window, cx))),
            );
        if self.kind == RichEditorKind::Im {
            atoms = atoms.child(
                self.button("rich-file", tr("添加文件引用", "Add file reference"), cx)
                    .on_click(cx.listener(|this, _, window, cx| this.add_atom(false, window, cx))),
            );
        }
        let mut result = div()
            .on_key_down(cx.listener(Self::key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .gap(px(super::style::metric("contentFieldGap", 7.)))
            .child(toolbar)
            .child(
                rsx! {
                    <div flex flex_col flex_1
                        min_h={px(super::style::metric("contentBodyMinHeight", 72.))}
                        min_w={px(0.)} border_1 border_color={color(&theme.border)}
                        rounded={px(super::style::metric("contentControlRadius", 5.))}
                        overflow_hidden>
                        {self.canvas.clone()}
                    </div>
                },
            )
            .child(
                rsx! {
                    <div flex_none h={px(super::style::metric("controlHeight", 30.))} border_1
                        border_color={color(&theme.border)}
                        rounded={px(super::style::metric("contentControlRadius", 5.))}
                        overflow_hidden>
                        {self.value.clone()}
                    </div>
                },
            )
            .child(atoms);
        if let Document::Invalid(error) = &self.document {
            result = result.child(div().text_color(color(&theme.error)).child(error.clone()));
        }
        if let Some(error) = &self.error {
            result = result.child(div().text_color(color(&theme.error)).child(error.clone()));
        }
        if self.ui_motion.borrow().any_active(Instant::now()) || self.ui_selection_motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        result
    }
}
fn css_color(value: &str) -> Option<gpui::Hsla> {
    let value = value.trim().to_ascii_lowercase();
    let named = match value.as_str() {
        "black" => Some(0x000000),
        "white" => Some(0xffffff),
        "red" => Some(0xff0000),
        "green" => Some(0x008000),
        "blue" => Some(0x0000ff),
        "yellow" => Some(0xffff00),
        "gray" | "grey" => Some(0x808080),
        _ => None,
    };
    if let Some(color) = named {
        return Some(gpui::rgb(color).into());
    }
    if value == "transparent" {
        return Some(gpui::rgba(0).into());
    }
    if let Some(hex) = value.strip_prefix('#') {
        let expanded = if matches!(hex.len(), 3 | 4) {
            hex.chars().flat_map(|ch| [ch, ch]).collect::<String>()
        } else {
            hex.into()
        };
        let number = u32::from_str_radix(&expanded, 16).ok()?;
        return match expanded.len() {
            6 => Some(gpui::rgb(number).into()),
            8 => Some(gpui::rgba(number).into()),
            _ => None,
        };
    }
    let components = value
        .strip_prefix("rgb(")
        .or_else(|| value.strip_prefix("rgba("))?
        .strip_suffix(')')?
        .split(',')
        .map(|part| {
            let part = part.trim();
            if let Some(percent) = part.strip_suffix('%') {
                percent.parse::<f32>().ok().map(|number| number / 100.)
            } else {
                part.parse::<f32>().ok().map(|number| number / 255.)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    if !matches!(components.len(), 3 | 4) {
        return None;
    }
    let alpha = if components.len() == 4 {
        value
            .rsplit(',')
            .next()?
            .trim_end_matches(')')
            .trim()
            .parse::<f32>()
            .ok()?
    } else {
        1.
    };
    Some(
        gpui::Rgba {
            r: components[0].clamp(0., 1.),
            g: components[1].clamp(0., 1.),
            b: components[2].clamp(0., 1.),
            a: alpha.clamp(0., 1.),
        }
        .into(),
    )
}

#[path = "rich_editor_native.rs"]
mod canvas_native;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_tab_isolated_history_undo_restores_each_complete_source() {
        let original = "<p vendor='keep'>&#00065;</p>";
        let mut document = HtmlDocument::parse(original);
        let mut history = RichHistory::default();
        let now = Instant::now();
        history.record(html_frame(original), Some(TypingGroup::Canvas), now);
        document.replace_canvas_range(1..1, "B").unwrap();
        let before_tab = document.source().to_string();
        let plan = html_tab::plan_html_tab(&document, 2..2, false)
            .unwrap()
            .unwrap();
        assert!(plan.history_cutoff);
        tab_apply::apply_html_tab_plan(&mut document, &plan).unwrap();
        history.cutoff();
        history.record(
            html_frame(&before_tab),
            Some(TypingGroup::Canvas),
            now + Duration::from_millis(10),
        );
        history.cutoff();
        let after_tab = document.source().to_string();
        document.replace_canvas_range(plan.selection, "C").unwrap();
        history.record(
            html_frame(&after_tab),
            Some(TypingGroup::Canvas),
            now + Duration::from_millis(20),
        );
        assert_eq!(history.undo.len(), 3);
        let mut current = html_frame(document.source());
        for expected in [&after_tab, &before_tab, original] {
            current = history.undo(current).unwrap();
            assert_eq!(current.source, expected);
        }
        assert!(before_tab.contains("&#00065;"));
        assert!(after_tab.contains("&#00065;"));
    }
    #[test]
    fn paste_request_epoch_rejects_same_source_item_history_and_selection_aba() {
        let before = PasteSnapshot {
            epoch: 7,
            kind: RichEditorKind::Html,
            origin: RichOrigin::Unknown,
            source: "<p>A😀B</p>".into(),
            text: "A😀B\n".into(),
            selection: 1..5,
            reversed: true,
            unapplied: None,
        };
        assert!(before.matches(&before));
        let mut stale = before.clone();
        stale.epoch += 1;
        assert!(!before.matches(&stale));
        stale = before.clone();
        stale.reversed = false;
        assert!(!before.matches(&stale));
        stale = before.clone();
        stale.selection = 2..4;
        assert!(!stale.matches(&stale));
        stale = before.clone();
        stale.unapplied = Some("protected".into());
        assert!(!before.matches(&stale));
    }
    #[test]
    fn clipboard_paste_modes_keep_im_plain_whitespace_and_empty_noop() {
        assert!(clipboard_paste_plan(
            RichEditorKind::Im,
            false,
            false,
            Some("<b>ignored</b>"),
            Some("")
        )
        .unwrap()
        .is_none());
        assert!(
            clipboard_paste_plan(RichEditorKind::Html, false, false, None, None)
                .unwrap()
                .is_none()
        );
        let Some(ClipboardPaste::Text(text)) = clipboard_paste_plan(
            RichEditorKind::Im,
            false,
            false,
            Some("<b>ignored</b>"),
            Some(" \r\n\u{200b}😀"),
        )
        .unwrap() else {
            panic!("IM must remain text/plain")
        };
        assert_eq!(text, " \r\n\u{200b}😀");
        let Some(ClipboardPaste::Text(text)) = clipboard_paste_plan(
            RichEditorKind::Html,
            false,
            true,
            Some("<b>ignored</b>"),
            Some("fix"),
        )
        .unwrap() else {
            panic!("rejected drafts use normal native correction")
        };
        assert_eq!(text, "fix");
    }
    #[test]
    fn html_plain_paste_clears_only_inserted_inline_marks_and_keeps_raw_neighbors() {
        let source = "<!--keep--><p vendor='raw'><strong>&#00065;😀B</strong><img src='keep.png' width='32' height='20'></p>";
        let mut document = HtmlDocument::parse(source);
        let plan = plain_html_paste("X", false);
        let caret = apply_html_paste(&mut document, 1..5, &plan).unwrap();
        assert_eq!(caret, 2);
        assert_eq!(document.canvas_projection().text, "AXB\u{fffc}\n");
        assert!(document.source().contains("<!--keep--><p vendor='raw'>"));
        assert!(document.source().contains("&#00065;"));
        assert!(document
            .source()
            .contains("<img src='keep.png' width='32' height='20'>"));
        assert!(canvas_style_at(&document, 0..1)
            .iter()
            .all(|style| style.bold));
        assert!(canvas_style_at(&document, 1..2)
            .iter()
            .all(|style| !style.bold));
        assert!(canvas_style_at(&document, 2..3)
            .iter()
            .all(|style| style.bold));
    }
    #[test]
    fn html_paste_preserves_heading_base_and_code_context_but_ignores_caret_marks() {
        let mut document = HtmlDocument::parse("<h2 vendor='keep'>AB</h2><p tail='keep'>C</p>");
        apply_html_paste(&mut document, 1..1, &plain_html_paste("x", false)).unwrap();
        assert!(document.source().starts_with("<h2 vendor='keep'>"));
        assert!(document.source().ends_with("<p tail='keep'>C</p>"));
        assert!(canvas_style_at(&document, 1..2)
            .iter()
            .all(|style| style.bold));
        let Some(ClipboardPaste::Html(plan)) = clipboard_paste_plan(
            RichEditorKind::Html,
            true,
            false,
            Some("<h1><b>ignored style</b></h1>"),
            Some("a\r\nb"),
        )
        .unwrap() else {
            panic!("code block plan")
        };
        assert_eq!(plan.text, "a\nb");
        assert!(plan
            .spans
            .iter()
            .all(|span| span.style == HtmlStyle::default()));
        let mut document = HtmlDocument::parse("<pre vendor='keep'>XY</pre>");
        apply_html_paste(&mut document, 1..1, &plan).unwrap();
        assert_eq!(document.canvas_projection().text, "Xa\nbY\n");
        assert!(canvas_style_at(&document, 1..4)
            .iter()
            .all(|style| style.code));
        assert!(document.source().contains("vendor='keep'"));
    }
    #[test]
    fn html_paste_invalid_plan_is_atomic_and_empty_plan_keeps_source() {
        let source = "<p vendor='keep'>A😀B</p>";
        let mut document = HtmlDocument::parse(source);
        let mut invalid = plain_html_paste("😀", false);
        invalid.spans[0].range = 1..3;
        assert!(apply_html_paste(&mut document, 1..1, &invalid).is_err());
        assert_eq!(document.source(), source);
        let empty = plain_html_paste("", false);
        assert_eq!(apply_html_paste(&mut document, 1..5, &empty).unwrap(), 1);
        assert_eq!(document.source(), source);
    }
    #[test]
    fn html_clipboard_formats_replace_unicode_selection_in_one_lossless_transaction() {
        let source = "<!--keep--><p vendor='keep'>L😀R</p><p tail='raw'>&#00084;</p>";
        let fragment = "<h4><strong>B</strong></h4><ol><li class='ql-indent-2'>i</li><li>j</li></ol><blockquote>Q</blockquote><pre>C</pre><p><a href='javascript:ignored'>Z</a><img src='https://ignored.invalid/image.png'></p>";
        let plan = HtmlClipboardPlan::from_html(fragment).unwrap();
        let mut document = HtmlDocument::parse(source);
        let caret = apply_html_paste(&mut document, 1..5, &plan).unwrap();
        assert_eq!(caret, 1 + plan.text.len());
        assert_eq!(
            document.canvas_projection().text,
            format!("L{}R\nT\n", plan.text)
        );
        assert!(document.source().contains("<!--keep-->"));
        assert!(document.source().ends_with("<p tail='raw'>&#00084;</p>"));
        assert!(!document.source().contains("javascript:"));
        assert!(!document.source().contains("ignored.invalid"));
        assert!(document.source().contains("about:blank"));
        for line in &plan.lines {
            let (_, actual) = paste_line_owner(&document, 1 + line.newline).unwrap();
            assert_eq!(actual.kind, line.format.kind);
            assert_eq!(actual.list, line.format.list);
            assert_eq!(actual.indent, line.format.indent);
            assert_eq!(actual.quote, line.format.quote);
            assert_eq!(actual.code, line.format.code);
        }
        let before = html_frame(source);
        let mut history = RichHistory::default();
        history.record(before, Some(TypingGroup::Canvas), Instant::now());
        let after = HistoryFrame {
            counterpart_caret_utf16: None,
            document: Document::Html(document.clone()),
            source: document.source().into(),
            selected: Some(Selected::Canvas {
                range: caret..caret,
                reversed: false,
            }),
            caret_utf16: document.canvas_projection().text[..caret]
                .encode_utf16()
                .count(),
        };
        let undo = history.undo(after).unwrap();
        assert_eq!(undo.source, source);
        assert_eq!(history.redo(undo).unwrap().source, document.source());
    }
    #[test]
    fn plain_pasted_newline_clears_only_its_line_block_attributes() {
        let mut document = HtmlDocument::parse("<h2 vendor='keep'>AB</h2><p tail='keep'>T</p>");
        let plan = plain_html_paste("x\ny", false);
        apply_html_paste(&mut document, 1..1, &plan).unwrap();
        assert_eq!(document.canvas_projection().text, "Ax\nyB\nT\n");
        let (_, first) = paste_line_owner(&document, 2).unwrap();
        assert_eq!(first.kind, HtmlBlockKind::Paragraph);
        let (_, second) = paste_line_owner(&document, 5).unwrap();
        assert_eq!(second.kind, HtmlBlockKind::Header(2));
        assert!(document.source().ends_with("<p tail='keep'>T</p>"));
    }
    #[test]
    fn mixed_selection_indent_uses_the_common_level_without_rewriting_other_markup() {
        let source="<ul vendor='keep'><li class='ql-indent-2 vendor'><span style='color:red!important;color:blue' data-u='keep'>A</span></li><li>B</li></ul>";
        let mut document = HtmlDocument::parse(source);
        let projection = document.canvas_projection().text;
        let selection = 0..projection.len().saturating_sub(1);
        assert_eq!(common_canvas_indent(&document, selection.clone()), 0);
        apply_canvas_block_mark(&mut document, selection.clone(), HtmlBlockMark::Indent(1))
            .unwrap();
        assert_eq!(common_canvas_indent(&document, selection), 1);
        assert_eq!(document.canvas_projection().text, projection);
        for retained in [
            "vendor='keep'",
            "color:red!important;color:blue",
            "data-u='keep'",
        ] {
            assert!(document.source().contains(retained));
        }
        let document =
            HtmlDocument::parse("<p class='ql-indent-2'>A</p><p class='ql-indent-2'>B</p>");
        assert_eq!(common_canvas_indent(&document, 0..3), 2);
        let document =
            HtmlDocument::parse("<p class='ql-indent-2'>A</p><p class='ql-indent-3'>B</p>");
        assert_eq!(common_canvas_indent(&document, 0..3), 2);
        for (third, expected) in [(2, 0), (3, 0), (4, 2)] {
            let document=HtmlDocument::parse(format!("<p class='ql-indent-2'>A</p><p class='ql-indent-3'>B</p><p class='ql-indent-{third}'>C</p>"));
            assert_eq!(common_canvas_indent(&document, 0..5), expected);
        }
    }

    #[test]
    fn html_image_layout_preserves_original_markup_and_independent_axes() {
        use crate::editor_html::HtmlImageDimension::{Auto, Pixels};
        let source = "<p>A<img src='qa.png' width='32' height='20' data-opaque='keep'>B</p>";
        let document = HtmlDocument::parse(source);
        assert_eq!(
            document.images()[0].display_layout(),
            crate::editor_html::HtmlImageLayout {
                width: Pixels(32.),
                height: Pixels(20.),
                ..Default::default()
            }
        );
        assert_eq!(document.source(), source);
        for (source, width, height) in [
            ("<img src='qa.png' width='32'>", Pixels(32.), Auto),
            (
                "<img src='qa.png' width='0' height='20'>",
                Pixels(0.),
                Pixels(20.),
            ),
            (
                "<img src='qa.png' width='32' style='height:12.5px;width:auto'>",
                Auto,
                Pixels(12.5),
            ),
        ] {
            let document = HtmlDocument::parse(source);
            assert_eq!(document.images()[0].display_layout().width, width);
            assert_eq!(document.images()[0].display_layout().height, height);
            assert_eq!(document.source(), source);
        }
    }
    #[test]
    fn rejected_edit_recovery_validates_applied_source_and_actual_proposed_selection() {
        let source = "<QQRichEditFormat vendor='keep'><EditElement type='0'><![CDATA[A😀B]]></EditElement><Other vendor='protected'/></QQRichEditFormat>";
        let document = Document::Im(ImRichDocument::parse(source, RichOrigin::Qq).unwrap());
        let proposed = "A😀B";
        // The rejected native draft can differ from the canonical projection;
        // recovery must still use the retained valid document, never this text.
        assert!(validate_unapplied_recovery_snapshot(
            &document,
            source,
            proposed,
            proposed.len()..proposed.len()
        )
        .is_ok());
        assert!(
            validate_unapplied_recovery_snapshot(&document, "changed", proposed, 0..0).is_err()
        );
        assert!(validate_unapplied_recovery_snapshot(&document, source, proposed, 2..3).is_err());
        assert!(validate_unapplied_recovery_snapshot(
            &document,
            source,
            proposed,
            0..proposed.len() + 1
        )
        .is_err());
        assert!(validate_unapplied_recovery_snapshot(
            &Document::Invalid(source.into()),
            source,
            proposed,
            0..0
        )
        .is_err());
        assert_eq!(
            match document {
                Document::Im(document) => document.serialize(),
                _ => unreachable!(),
            },
            source
        );
        let html = Document::Html(HtmlDocument::parse("<p vendor='keep'>A😀B</p>"));
        assert!(validate_unapplied_recovery_snapshot(
            &html,
            "<p vendor='keep'>A😀B</p>",
            "A",
            1..1
        )
        .is_ok());
    }
    #[test]
    fn metadata_same_path_new_object_has_a_separate_field_history() {
        use crate::editor::text_history::{EditorTextHistory, TextEditKind, TextSnapshot};
        let frame = |text: &str| TextSnapshot {
            text: text.into(),
            selection: text.len()..text.len(),
            reversed: false,
        };
        let mut history = EditorTextHistory::default();
        history.reset(frame("p"));
        history.record(
            frame("p"),
            frame("q"),
            0..1,
            "q",
            TextEditKind::Typing,
            Instant::now(),
        );
        assert!(history.can_undo());
        let first = MetadataContext::HtmlImage(0);
        let second = MetadataContext::HtmlImage(1);
        let (value, reset) = metadata_field_sync(Some(&first), &second, "q", "q").unwrap();
        assert!(reset);
        assert_eq!(value, "q");
        history.reset(frame(&value));
        assert!(!history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(
            metadata_field_sync(Some(&second), &second, "q", "q"),
            Some(("q".into(), false))
        );
        assert_eq!(
            metadata_field_sync(
                Some(&MetadataContext::Parameters),
                &MetadataContext::Parameters,
                "#ff0000",
                ""
            ),
            None
        );
    }

    #[test]
    fn exact_selected_atom_insertion_history_restores_complete_raw_payload() {
        let original = "<!--head--><p vendor='left'>A&amp;B</p><p vendor='right'>C<img src='old' vendor='keep'>D</p><!--tail-->";
        let mut html = HtmlDocument::parse(original);
        let projection = html.canvas_projection().text;
        let end = projection.find('D').unwrap();
        let before = HistoryFrame {
            counterpart_caret_utf16: None,
            document: Document::Html(html.clone()),
            source: original.into(),
            selected: Some(Selected::Canvas {
                range: 1..end,
                reversed: true,
            }),
            caret_utf16: projection[..1].encode_utf16().count(),
        };
        let mut history = RichHistory::default();
        let caret = html
            .insert_canvas_image(1..end, "C:/new & image.png", "")
            .unwrap();
        history.record(before, None, Instant::now());
        let changed = html.source().to_string();
        let undo = history
            .undo(HistoryFrame {
                counterpart_caret_utf16: None,
                document: Document::Html(html),
                source: changed.clone(),
                selected: Some(Selected::Canvas {
                    range: caret..caret,
                    reversed: false,
                }),
                caret_utf16: 2,
            })
            .unwrap();
        assert_eq!(undo.source, original);
        assert_eq!(history.redo(undo).unwrap().source, changed);

        let original = "prefix<QQRichEditFormat><Info version='1001'/><EditElement type='0' vendor='text'>A&amp;B</EditElement><EditElement type='3' filepath='old' vendor='image'/><EditElement type='0'>C</EditElement></QQRichEditFormat>suffix";
        let mut im = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        let before = HistoryFrame {
            counterpart_caret_utf16: None,
            document: Document::Im(im.clone()),
            source: original.into(),
            selected: Some(Selected::Canvas {
                range: 1..6,
                reversed: true,
            }),
            caret_utf16: 1,
        };
        let mut history = RichHistory::default();
        let caret =
            im_insert::insert_im_canvas_atom(&mut im, 1..6, false, "C:/new & file.txt").unwrap();
        history.record(before, None, Instant::now());
        assert_eq!(im_canvas_projection(&im).text, "A\u{fffc}C");
        let changed = im.serialize();
        let undo = history
            .undo(HistoryFrame {
                counterpart_caret_utf16: None,
                document: Document::Im(im),
                source: changed.clone(),
                selected: Some(Selected::Canvas {
                    range: caret..caret,
                    reversed: false,
                }),
                caret_utf16: 2,
            })
            .unwrap();
        assert_eq!(undo.source, original);
        assert_eq!(history.redo(undo).unwrap().source, changed);
    }

    fn html_frame(source: &str) -> HistoryFrame {
        HistoryFrame {
            counterpart_caret_utf16: None,
            document: Document::Html(HtmlDocument::parse(source)),
            source: source.into(),
            selected: None,
            caret_utf16: 0,
        }
    }

    #[test]
    fn list_autofill_history_restores_marker_space_and_quill_line_end_carets() {
        let original = "<!--head--><p vendor='keep'>T&#x1F600;</p><!--tail-->";
        let mut marker = HtmlDocument::parse(original);
        let now = Instant::now();
        let mut history = RichHistory::default();
        history.record(html_frame(original), Some(TypingGroup::Canvas), now);
        marker.replace_canvas_range(0..0, "-").unwrap();
        let plan = html_list_autofill::plan_html_list_autofill(&marker, 1..1, " ", true)
            .unwrap()
            .unwrap();
        let edit = list_autofill_apply::apply_html_list_autofill(&marker, &plan, &[]).unwrap();
        let mut space = html_frame(edit.space_document.source());
        space.caret_utf16 = edit.space_document.canvas_projection().text[..edit.undo_caret]
            .encode_utf16()
            .count();
        let final_caret = edit.document.canvas_projection().text[..edit.redo_caret]
            .encode_utf16()
            .count();
        let final_source = edit.document.source().to_string();
        let space_source = edit.space_document.source().to_string();
        assert_eq!(
            space_source,
            original.replacen("T&#x1F600;", "- T&#x1F600;", 1)
        );
        history.record_list_autofill(html_frame(marker.source()), space, final_caret, now);
        assert_eq!(history.undo.len(), 2);
        assert_eq!(edit.selection, 0..0);
        // Moving the actual caret after Undo must not change block-format
        // history's redo target; the operation's Delta still ends at newline.
        for _ in 0..2 {
            let mut undo = history.undo(html_frame(&final_source)).unwrap();
            assert_eq!(undo.source, space_source);
            assert_eq!(undo.caret_utf16, 5);
            undo.caret_utf16 = 0;
            let redo = history.redo(undo).unwrap();
            assert_eq!(redo.source, final_source);
            assert_eq!(redo.caret_utf16, 3);
        }
        let space = history.undo(html_frame(&final_source)).unwrap();
        assert_eq!(history.undo(space).unwrap().source, original);
    }

    #[test]
    fn list_autofill_cuts_conversion_off_from_following_typing() {
        let original = "<p vendor='keep'>1.</p>";
        let marker = HtmlDocument::parse(original);
        let plan = html_list_autofill::plan_html_list_autofill(&marker, 2..2, " ", true)
            .unwrap()
            .unwrap();
        let edit = list_autofill_apply::apply_html_list_autofill(&marker, &plan, &[]).unwrap();
        let mut history = RichHistory::default();
        let now = Instant::now();
        let mut space = html_frame(edit.space_document.source());
        space.caret_utf16 = edit.undo_caret;
        history.record_list_autofill(html_frame(original), space, edit.redo_caret, now);
        let converted = edit.document.source().to_string();
        let mut typed = edit.document;
        history.record(html_frame(&converted), Some(TypingGroup::Canvas), now);
        typed.replace_canvas_range(0..0, "body").unwrap();
        assert_eq!(history.undo.len(), 3);
        let converted_frame = history.undo(html_frame(typed.source())).unwrap();
        assert_eq!(converted_frame.source, converted);
        assert_eq!(converted_frame.caret_utf16, 0);
        let marker_space = history.undo(converted_frame).unwrap();
        assert_eq!(marker_space.source, "<p vendor='keep'>1. </p>");
        assert_eq!(marker_space.caret_utf16, 3);
        assert_eq!(history.undo(marker_space).unwrap().source, original);
        // A fresh ordinary edit after Undo clears the paired Redo with the
        // existing branch semantics; it does not retain a conversion shortcut.
        history.record(html_frame(original), Some(TypingGroup::Canvas), now);
        assert!(history.redo.is_empty());
    }

    #[test]
    fn typing_group_uses_the_first_change_time_and_starts_new_group_after_one_second() {
        let mut history = RichHistory::default();
        let now = Instant::now();
        history.record(html_frame("A"), Some(TypingGroup::Html(0)), now);
        history.record(
            html_frame("AB"),
            Some(TypingGroup::Html(0)),
            now + Duration::from_millis(900),
        );
        assert_eq!(history.undo.len(), 1);
        history.record(
            html_frame("ABC"),
            Some(TypingGroup::Html(0)),
            now + Duration::from_millis(1200),
        );
        assert_eq!(history.undo(html_frame("ABCD")).unwrap().source, "ABC");
        assert_eq!(history.undo(html_frame("ABC")).unwrap().source, "A");
    }

    #[test]
    fn undo_redo_roundtrip_keeps_complete_html_and_unknown_source_bytes() {
        let original =
            "<!--keep--><p data-vendor='x'><b>甲 &amp; 😀</b></p><style>.x{color:red}</style>";
        let mut changed = HtmlDocument::parse(original);
        changed
            .toggle_text_range(0, 0.."甲".len(), HtmlMark::Bold)
            .unwrap();
        let modified = changed.source().to_string();
        let mut history = RichHistory::default();
        history.record(html_frame(original), None, Instant::now());
        let restored = history.undo(html_frame(&modified)).unwrap();
        assert_eq!(restored.source, original);
        let Document::Html(document) = &restored.document else {
            panic!()
        };
        assert_eq!(document.source(), original);
        assert_eq!(history.redo(restored).unwrap().source, modified);
    }

    #[test]
    fn new_edit_clears_redo_and_selection_change_cuts_typing_group() {
        let mut history = RichHistory::default();
        let now = Instant::now();
        history.record(html_frame("A"), Some(TypingGroup::Html(0)), now);
        let restored = history.undo(html_frame("AB")).unwrap();
        assert_eq!(history.redo.len(), 1);
        history.record(restored, Some(TypingGroup::Html(0)), now);
        assert!(history.redo.is_empty());
        history.cutoff();
        history.record(
            html_frame("AC"),
            Some(TypingGroup::Html(0)),
            now + Duration::from_millis(10),
        );
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn document_history_retains_at_most_quill_default_max_stack() {
        let mut history = RichHistory::default();
        for index in 0..101 {
            history.record(html_frame(&index.to_string()), None, Instant::now());
        }
        assert_eq!(history.undo.len(), 100);
        assert_eq!(history.undo.first().unwrap().source, "1");
    }

    #[test]
    fn object_edit_undo_redo_retains_xml_metadata_attributes_and_stable_node_ids() {
        let original = "prefix<QQRichEditFormat><Info version='9'/><!--keep--><EditElement type='0' vendor='a'><![CDATA[文本]]></EditElement><EditElement type='3' filepath='D:/old.png' extra='keep'/><Other a='b'/></QQRichEditFormat>suffix";
        let before = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        let image = before
            .content_nodes()
            .find(|node| node.kind() == RichNodeKind::Image)
            .unwrap()
            .id();
        let mut after = before.clone();
        after.set_path(image, "D:/new.png").unwrap();
        let modified = after.serialize();
        let mut history = RichHistory::default();
        history.record(
            HistoryFrame {
                counterpart_caret_utf16: None,
                document: Document::Im(before),
                source: original.into(),
                selected: Some(Selected::Im(image)),
                caret_utf16: 0,
            },
            None,
            Instant::now(),
        );
        let restored = history
            .undo(HistoryFrame {
                counterpart_caret_utf16: None,
                document: Document::Im(after),
                source: modified.clone(),
                selected: Some(Selected::Im(image)),
                caret_utf16: 0,
            })
            .unwrap();
        let Document::Im(document) = &restored.document else {
            panic!()
        };
        assert_eq!(document.serialize(), original);
        assert_eq!(document.node(image).unwrap().path(), "D:/old.png");
        let redo = history.redo(restored).unwrap();
        assert_eq!(redo.source, modified);
    }

    #[test]
    fn unicode_diff_spans_do_not_split_surrogate_pairs_or_utf8_and_keep_crlf() {
        let (old, new) = changed_text_ranges("A😀中\nB", "A😀新\r\nB");
        assert_eq!(&"A😀中\nB"[old], "中");
        assert_eq!(&"A😀新\r\nB"[new], "新\r");
        assert_eq!(changed_text_ranges("😀", "😀x"), (4..4, 4..5));
    }

    #[test]
    fn empty_document_first_edit_can_undo_to_the_original_empty_source() {
        let mut history = RichHistory::default();
        let mut before = html_frame("");
        before.selected = Some(Selected::HtmlInsertion);
        let mut after = HtmlDocument::parse("");
        let anchor = after.append_paragraph("中😀");
        history.record(
            before,
            Some(TypingGroup::Html(anchor.start)),
            Instant::now(),
        );
        let restored = history
            .undo(HistoryFrame {
                counterpart_caret_utf16: None,
                source: after.source().into(),
                document: Document::Html(after),
                selected: Some(Selected::HtmlText(anchor)),
                caret_utf16: 3,
            })
            .unwrap();
        assert_eq!(restored.source, "");
        assert_eq!(restored.selected, Some(Selected::HtmlInsertion));
    }

    #[test]
    fn caret_format_applies_only_inserted_text_and_keeps_original_entities_and_attributes() {
        let original = "<p vendor='keep'>A &amp; B</p><!--tail-->";
        let mut document = HtmlDocument::parse(original);
        let span = document.runs()[0].source_range.clone();
        let old = document.runs()[0].text.clone();
        let pending = [
            PendingFormat {
                mark: HtmlMark::Bold,
                enabled: true,
            },
            PendingFormat {
                mark: HtmlMark::Italic,
                enabled: true,
            },
        ];
        let update =
            update_html_text(&mut document, span, &old, "A & X😀B", &pending, None).unwrap();
        assert_eq!(update.text, "X😀");
        assert!(document.source().contains("vendor='keep'"));
        assert!(document.source().contains("A &amp; "));
        assert!(document.source().ends_with("</p><!--tail-->"));
        let inserted = document
            .runs()
            .iter()
            .find(|run| run.source_range == update.anchor)
            .unwrap();
        assert!(inserted.style.bold && inserted.style.italic);
        assert!(
            !document
                .runs()
                .iter()
                .find(|run| run.text == "A & ")
                .unwrap()
                .style
                .bold
        );
        let next = update_html_text(
            &mut document,
            update.anchor,
            &update.text,
            "X😀中",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(next.text, "X😀中");
        assert!(
            document
                .runs()
                .iter()
                .find(|run| run.source_range == next.anchor)
                .unwrap()
                .style
                .bold
        );
    }

    #[test]
    fn caret_format_can_disable_bold_without_removing_italic_from_existing_or_new_text() {
        let mut document = HtmlDocument::parse("<p><b vendor='x'><i>AB</i></b></p>");
        let span = document.runs()[0].source_range.clone();
        let update = update_html_text(
            &mut document,
            span,
            "AB",
            "AXB",
            &[PendingFormat {
                mark: HtmlMark::Bold,
                enabled: false,
            }],
            None,
        )
        .unwrap();
        assert_eq!(update.text, "X");
        let inserted = document
            .runs()
            .iter()
            .find(|run| run.source_range == update.anchor)
            .unwrap();
        assert!(!inserted.style.bold && inserted.style.italic);
        assert!(document
            .runs()
            .iter()
            .filter(|run| run.text == "A" || run.text == "B")
            .all(|run| run.style.bold && run.style.italic));
        assert!(document.source().contains("vendor='x'"));
    }

    #[test]
    fn text_diff_preserves_unedited_entity_spelling_and_empty_anchor_continues_input() {
        let mut document = HtmlDocument::parse("<p>A &amp; &#x4e2d; B</p>");
        let span = document.runs()[0].source_range.clone();
        let old = document.runs()[0].text.clone();
        let update = update_html_text(&mut document, span, &old, "A & 中 XB", &[], None).unwrap();
        assert!(document.source().contains("&amp; &#x4e2d; XB"));
        let empty =
            update_html_text(&mut document, update.anchor, &update.text, "", &[], None).unwrap();
        assert_eq!(document.source(), "<p></p>");
        let next = update_html_text(&mut document, empty.anchor, "", "新😀", &[], None).unwrap();
        assert_eq!(next.text, "新😀");
    }

    #[test]
    fn bad_pending_format_does_not_partially_commit_a_text_edit() {
        let original = "<p attrs='keep'>AB</p>";
        let mut document = HtmlDocument::parse(original);
        let span = document.runs()[0].source_range.clone();
        assert!(update_html_text(
            &mut document,
            span,
            "AB",
            "AXB",
            &[PendingFormat {
                mark: HtmlMark::Color("red;display:none".into()),
                enabled: true
            }],
            None,
        )
        .is_err());
        assert_eq!(document.source(), original);
    }

    #[test]
    fn first_text_with_caret_format_keeps_empty_paragraph_attributes_and_can_be_undone() {
        let original = "<p vendor='keep'><br></p>";
        let mut document = HtmlDocument::parse(original);
        let update = insert_html_text(
            &mut document,
            "中😀",
            &[PendingFormat {
                mark: HtmlMark::Underline,
                enabled: true,
            }],
        )
        .unwrap();
        assert_eq!(update.text, "中😀");
        assert!(document.source().contains("vendor='keep'"));
        assert!(
            document
                .runs()
                .iter()
                .find(|run| run.source_range == update.anchor)
                .unwrap()
                .style
                .underline
        );
        let mut history = RichHistory::default();
        history.record(html_frame(original), None, Instant::now());
        assert_eq!(
            history.undo(html_frame(document.source())).unwrap().source,
            original
        );
    }

    #[test]
    fn repeated_insertions_follow_the_native_caret_and_style_only_the_inserted_character() {
        for (source, before, after, start) in [
            ("<p>a</p>", "a", "aa", 0),
            ("<p>aaa</p>", "aaa", "aaaa", 1),
            ("<p>&#97;</p>", "a", "aa", 0),
            ("<p>😀😀</p>", "😀😀", "😀😀😀", "😀".len()),
        ] {
            let mut document = HtmlDocument::parse(source);
            let span = document.runs()[0].source_range.clone();
            let pending = [
                PendingFormat {
                    mark: HtmlMark::Bold,
                    enabled: true,
                },
                PendingFormat {
                    mark: HtmlMark::Italic,
                    enabled: true,
                },
            ];
            let update =
                update_html_text(&mut document, span, before, after, &pending, Some(start))
                    .unwrap();
            let inserted = document
                .runs()
                .iter()
                .find(|run| run.source_range == update.anchor)
                .unwrap();
            assert!(inserted.style.bold && inserted.style.italic);
            assert_eq!(
                inserted.text,
                if before.contains('😀') { "😀" } else { "a" }
            );
            assert!(document
                .runs()
                .iter()
                .filter(|run| run.source_range != update.anchor)
                .all(|run| !run.style.bold && !run.style.italic));
            let original_prefix = before[..start].to_string();
            let combined = document
                .runs()
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>();
            assert_eq!(combined, after);
            assert!(document.source()[..update.anchor.start].ends_with("<strong><em>"));
            if source.contains("&#97;") {
                assert!(document.source().ends_with("&#97;</p>"));
            }
            if !original_prefix.is_empty() {
                assert_eq!(document.runs()[0].text, original_prefix);
            }
            let next = update_html_text(&mut document, update.anchor, &update.text, "X", &[], None)
                .unwrap();
            assert_eq!(next.text, "X");
            assert_eq!(
                document
                    .runs()
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>(),
                format!("{}X{}", &before[..start], &before[start..])
            );
        }
    }

    #[test]
    fn stale_insertion_hint_falls_back_to_valid_diff_without_slicing_past_utf8() {
        assert_eq!(
            changed_text_ranges_at_insertion("abc", "aXbc", Some(99)),
            (1..1, 1..2)
        );
        assert_eq!(
            changed_text_ranges_at_insertion("abc", "aXbc", Some(0)),
            (1..1, 1..2)
        );
        assert_eq!(
            changed_text_ranges_at_insertion("😀", "😀x", Some(1)),
            (4..4, 4..5)
        );
        assert_eq!(
            changed_text_ranges_at_insertion("abc", "bc", Some(0)),
            (0..1, 0..0)
        );
        assert_eq!(
            changed_text_ranges_at_insertion("aaa", "aaaa", Some(1)),
            (1..1, 1..2)
        );
    }

    #[test]
    fn first_whitespace_can_receive_pending_format_and_continue_typing() {
        for (input, projected) in [
            (" ", " "),
            ("\r\n", "\n"),
            ("\u{a0}", "\u{a0}"),
            ("\u{200d}", "\u{200d}"),
        ] {
            let mut document = HtmlDocument::parse("<p vendor='keep'><br></p>");
            let update = insert_html_text(
                &mut document,
                input,
                &[PendingFormat {
                    mark: HtmlMark::Bold,
                    enabled: true,
                }],
            )
            .unwrap();
            assert_eq!(update.text, projected);
            assert!(
                document
                    .runs()
                    .iter()
                    .find(|run| run.source_range == update.anchor)
                    .unwrap()
                    .style
                    .bold
            );
            assert!(document.source().contains("vendor='keep'"));
            let text = format!("{projected}a");
            let continued = update_html_text(
                &mut document,
                update.anchor,
                projected,
                &text,
                &[],
                Some(projected.len()),
            )
            .unwrap();
            assert_eq!(continued.text, text);
            assert!(
                document
                    .runs()
                    .iter()
                    .find(|run| run.source_range == continued.anchor)
                    .unwrap()
                    .style
                    .bold
            );
        }
        let mut document = HtmlDocument::parse("<p vendor='keep'>abc</p>");
        let span = document.runs()[0].source_range.clone();
        let update = update_html_text(&mut document, span, "abc", "    ", &[], None).unwrap();
        assert_eq!(update.text, "    ");
        assert_eq!(document.source(), "<p vendor='keep'>    </p>");
    }

    #[test]
    fn im_canvas_cross_object_replacement_keeps_xml_metadata_and_untouched_nodes() {
        let original = "prefix<QQRichEditFormat vendor='root'><Info keep='1'/><!--keep--><EditElement type='0' vendor='left'><![CDATA[A😀B]]></EditElement><EditElement type='3' filepath='x'/><EditElement type='4' filepath='file'/><EditElement type='0' vendor='right'>C&amp;D</EditElement><Other vendor='protected'/></QQRichEditFormat>suffix";
        let mut document = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        let projection = im_canvas_projection(&document);
        assert_eq!(
            projection.text,
            format!("A😀B{CANVAS_OBJECT}{CANVAS_OBJECT}C&D{CANVAS_OBJECT}")
        );
        let end = projection.text.find("&D").unwrap();
        let caret = replace_im_canvas_range(&mut document, 1..end, "X").unwrap();
        assert_eq!(caret, 2);
        assert_eq!(
            im_canvas_projection(&document).text,
            format!("AX&D{CANVAS_OBJECT}")
        );
        let changed = document.serialize();
        assert!(changed
            .starts_with("prefix<QQRichEditFormat vendor='root'><Info keep='1'/><!--keep-->"));
        assert!(changed.contains("vendor='left'><![CDATA[AX]]>"));
        assert!(changed.contains("vendor='right'>&amp;D"));
        assert!(changed.ends_with("<Other vendor='protected'/></QQRichEditFormat>suffix"));
        assert!(!changed.contains("filepath="));
    }

    #[test]
    fn im_canvas_unknown_objects_and_invalid_xml_replacements_fail_atomically() {
        let original = "<QQRichEditFormat><EditElement type='0'>A</EditElement><Unknown/><EditElement type='0'>B</EditElement></QQRichEditFormat>";
        let mut document = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        let range = 0..im_canvas_projection(&document).text.len();
        assert!(replace_im_canvas_range(&mut document, range, "X").is_err());
        assert_eq!(document.serialize(), original);
        assert!(replace_im_canvas_range(&mut document, 0..1, "\0").is_err());
        assert_eq!(document.serialize(), original);
        assert_eq!(
            replace_im_canvas_range(&mut document, 1..1, "x").unwrap(),
            2
        );
        assert_eq!(
            im_canvas_projection(&document).text,
            format!("Ax{CANVAS_OBJECT}B")
        );
        assert!(document.serialize().contains("<Unknown/>"));
    }

    #[test]
    fn im_canvas_partial_edits_preserve_entity_spelling_cdata_and_inner_metadata() {
        let original = "<QQRichEditFormat><EditElement type='0' vendor='x'>A&#x4e2d;<!--keep--><?vendor x?><![CDATA[B]]>&amp;C</EditElement></QQRichEditFormat>";
        let mut document = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        assert_eq!(im_canvas_projection(&document).text, "A中B&C");
        replace_im_canvas_range(&mut document, "A中".len().."A中B".len(), "X]]>").unwrap();
        assert_eq!(im_canvas_projection(&document).text, "A中X]]>&C");
        assert_eq!(document.serialize(), "<QQRichEditFormat><EditElement type='0' vendor='x'>A&#x4e2d;<!--keep--><?vendor x?><![CDATA[X]]]]><![CDATA[>]]>&amp;C</EditElement></QQRichEditFormat>");
        let mut empty = ImRichDocument::parse(
            "<QQRichEditFormat><EditElement type='0' vendor='x'/></QQRichEditFormat>",
            RichOrigin::Qq,
        )
        .unwrap();
        replace_im_canvas_range(&mut empty, 0..0, "中😀").unwrap();
        assert_eq!(im_canvas_projection(&empty).text, "中😀");
        assert!(empty.serialize().contains("vendor='x'"));
    }

    #[test]
    fn im_canvas_normalizes_display_crlf_without_editing_real_zwsp_or_unselected_newlines() {
        let original = "<QQRichEditFormat><EditElement type='0'><![CDATA[A\r\nB\u{200b}C\r\nD]]></EditElement></QQRichEditFormat>";
        let mut document = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        assert_eq!(im_canvas_projection(&document).text, "A\nB\u{200b}C\nD");
        replace_im_canvas_range(&mut document, 2..3, "X").unwrap();
        assert_eq!(document.serialize(), "<QQRichEditFormat><EditElement type='0'><![CDATA[A\r\nX\u{200b}C\r\nD]]></EditElement></QQRichEditFormat>");
        replace_im_canvas_range(&mut document, 1..2, "").unwrap();
        assert_eq!(im_canvas_projection(&document).text, "AX\u{200b}C\nD");
        assert!(document.serialize().contains("C\r\nD"));
        let mut empty =
            ImRichDocument::parse("before<QQRichEditFormat/>after", RichOrigin::Qq).unwrap();
        replace_im_canvas_range(&mut empty, 0..0, "A\nB").unwrap();
        assert_eq!(im_canvas_projection(&empty).text, "A\nB");
        assert!(empty.serialize().starts_with("before<QQRichEditFormat>"));
        assert!(empty.serialize().ends_with("</QQRichEditFormat>after"));
    }

    #[test]
    fn canvas_empty_line_toolbar_and_clear_keep_siblings_and_source_payload() {
        let mut document =
            HtmlDocument::parse("<h1 vendor='empty'><br></h1><p vendor='text'><b>A</b></p>");
        let before = document.canvas_projection().text;
        assert_eq!(before, "\nA\n");
        apply_canvas_block_mark(&mut document, 0..0, HtmlBlockMark::Header(2)).unwrap();
        assert!(document.source().contains("vendor='empty'"));
        assert_eq!(
            document
                .canvas_block_format(&canvas_block_owners(&document, 0..0)[0])
                .unwrap()
                .kind,
            HtmlBlockKind::Header(2)
        );
        clean_canvas_selection(&mut document, 0..before.len() - 1).unwrap();
        assert_eq!(document.canvas_projection().text, before);
        for owner in canvas_block_owners(&document, 0..before.len()) {
            let block = document.canvas_block_format(&owner).unwrap();
            assert_eq!(block.kind, HtmlBlockKind::Paragraph);
            assert!(block.list.is_none() && !block.quote && !block.code && block.indent == 0);
        }
        assert!(document.runs().iter().all(|run| !run.style.bold));
        assert!(document.source().contains("vendor='empty'"));
        assert!(document.source().contains("vendor='text'"));

        let mut empty = HtmlDocument::parse("");
        apply_canvas_block_mark(&mut empty, 0..0, HtmlBlockMark::Header(3)).unwrap();
        assert_eq!(empty.canvas_projection().text, "\n");
        assert_eq!(
            empty
                .canvas_block_format(&canvas_block_owners(&empty, 0..0)[0])
                .unwrap()
                .kind,
            HtmlBlockKind::Header(3)
        );
    }

    #[test]
    fn canvas_block_caret_at_next_line_start_formats_that_line_only() {
        let original = "<p vendor='first'>A</p><p vendor='second'>B</p>";
        let mut document = HtmlDocument::parse(original);
        let selection = 2..2;
        assert_eq!(
            canvas_block_ranges(&document, selection.clone()),
            vec![document.blocks()[1].source_range.clone()]
        );
        apply_canvas_block_mark(&mut document, selection, HtmlBlockMark::Header(2)).unwrap();
        assert_eq!(document.blocks()[0].kind, HtmlBlockKind::Paragraph);
        assert_eq!(document.blocks()[1].kind, HtmlBlockKind::Header(2));
        assert!(document.source().starts_with("<p vendor='first'>A</p>"));
        assert!(document.source().contains("vendor='second'"));
    }

    #[test]
    fn canvas_format_spans_runs_paragraphs_and_images_without_losing_source() {
        let original = "<!--head--><p vendor='left'>A<b>B</b>C</p><p vendor='right'>D<img src='x' vendor='image'>E</p><!--tail-->";
        let mut document = HtmlDocument::parse(original);
        let before = document.canvas_projection().text;
        apply_canvas_inline_mark(&mut document, 1..before.len() - 1, HtmlMark::Bold, true).unwrap();
        assert_eq!(document.canvas_projection().text, before);
        for span in document.canvas_projection().spans {
            if let HtmlCanvasKind::Text { run } = span.kind {
                if span.range.end <= 1 {
                    assert!(!document.runs()[run].style.bold);
                } else {
                    assert!(document.runs()[run].style.bold);
                }
            }
        }
        assert!(document
            .source()
            .starts_with("<!--head--><p vendor='left'>A"));
        assert!(document.source().contains("src='x' vendor='image'"));
        assert!(document.source().ends_with("</p><!--tail-->"));
        apply_canvas_inline_mark(&mut document, 1..before.len() - 1, HtmlMark::Bold, false)
            .unwrap();
        assert!(document.runs().iter().all(|run| !run.style.bold));
    }

    #[test]
    fn canvas_clean_and_block_toggles_preserve_global_selection_positions() {
        let original = "<h1 vendor='left'>A<i>B</i>C</h1><p vendor='right'><b>D</b>E</p>";
        let mut document = HtmlDocument::parse(original);
        let before = document.canvas_projection().text;
        clean_canvas_selection(&mut document, 1..before.len() - 1).unwrap();
        assert_eq!(document.canvas_projection().text, before);
        assert!(document
            .blocks()
            .iter()
            .all(|block| block.kind == HtmlBlockKind::Paragraph));
        assert!(document
            .runs()
            .iter()
            .all(|run| !run.style.bold && !run.style.italic));
        apply_canvas_block_mark(&mut document, 0..before.len(), HtmlBlockMark::Blockquote).unwrap();
        assert_eq!(document.canvas_projection().text, before);
        assert!(document.blocks().iter().all(|block| block.quote));
        apply_canvas_block_mark(&mut document, 0..before.len(), HtmlBlockMark::Blockquote).unwrap();
        assert_eq!(document.canvas_projection().text, before);
        assert!(document.blocks().iter().all(|block| !block.quote));
        assert!(document.source().contains("vendor='left'"));
        assert!(document.source().contains("vendor='right'"));
    }

    #[test]
    fn cross_paragraph_edit_undo_redo_restores_exact_source_and_canvas_caret() {
        let original = "<p vendor='left'>A&amp;B</p><!--keep--><p vendor='right'>C😀D</p>";
        let mut document = HtmlDocument::parse(original);
        let before = document.canvas_projection().text;
        let end = before.find('D').unwrap();
        let mut history = RichHistory::default();
        history.record(
            HistoryFrame {
                counterpart_caret_utf16: None,
                source: original.into(),
                document: Document::Html(document.clone()),
                selected: Some(Selected::Canvas {
                    range: 1..end,
                    reversed: true,
                }),
                caret_utf16: before[..end].encode_utf16().count(),
            },
            None,
            Instant::now(),
        );
        let caret = document.replace_canvas_range(1..end, "X").unwrap();
        assert_eq!(document.canvas_projection().text, "AXD\n");
        assert_eq!(caret, 2);
        let changed = document.source().to_string();
        let frame = HistoryFrame {
            counterpart_caret_utf16: None,
            source: changed.clone(),
            document: Document::Html(document),
            selected: Some(Selected::Canvas {
                range: caret..caret,
                reversed: false,
            }),
            caret_utf16: 2,
        };
        let undo = history.undo(frame).unwrap();
        assert_eq!(undo.source, original);
        assert!(
            matches!(&undo.selected,Some(Selected::Canvas{range,reversed:true})if range== &(1..end))
        );
        assert_eq!(history.redo(undo).unwrap().source, changed);
    }

    #[test]
    fn qq_atomic_object_delete_preserves_combining_neighbors_and_undo_roundtrip() {
        let original="<QQRichEditFormat><Info keep='1'/><EditElement type='0'>A</EditElement><EditElement type='3' filepath='x' vendor='i'/><EditElement type='0'>&#x301;B</EditElement></QQRichEditFormat>";
        let mut document = ImRichDocument::parse(original, RichOrigin::Qq).unwrap();
        let mut history = RichHistory::default();
        history.record(
            HistoryFrame {
                counterpart_caret_utf16: None,
                source: original.into(),
                document: Document::Im(document.clone()),
                selected: Some(Selected::Canvas {
                    range: 4..4,
                    reversed: false,
                }),
                caret_utf16: 2,
            },
            None,
            Instant::now(),
        );
        assert_eq!(replace_im_canvas_range(&mut document, 1..4, "").unwrap(), 1);
        assert_eq!(im_canvas_projection(&document).text, "A\u{301}B");
        assert!(document.serialize().contains("&#x301;B"));
        assert!(document.serialize().contains("<Info keep='1'/>"));
        let changed = document.serialize();
        let restored = history
            .undo(HistoryFrame {
                counterpart_caret_utf16: None,
                source: changed.clone(),
                document: Document::Im(document),
                selected: Some(Selected::Canvas {
                    range: 1..1,
                    reversed: false,
                }),
                caret_utf16: 1,
            })
            .unwrap();
        assert_eq!(restored.source, original);
        assert_eq!(history.redo(restored).unwrap().source, changed);
    }

    #[test]
    fn native_ime_ack_keeps_committed_selection_and_maps_only_canonical_newlines() {
        assert_eq!(
            acknowledge_native_selection(0..0, 0, "中文", 0, "中文\n", true),
            0..0
        );
        assert_eq!(
            acknowledge_native_selection(0..3, 0, "中文", 0, "中文\n", true),
            0..3
        );
        assert_eq!(
            acknowledge_native_selection(3..6, 0, "中文", 0, "中文\n", true),
            3..6
        );
        assert_eq!(
            acknowledge_native_selection(3..3, 0, "A\r\nB", 0, "A\nB\n", true),
            2..2
        );
        assert_eq!(
            acknowledge_native_selection(3..3, 2, "X", 1, "AX\n", true),
            2..2
        );
        assert_eq!(
            acknowledge_native_selection(6..6, 1, "😀", 1, "A😀B", false),
            6..6
        );
    }
}
