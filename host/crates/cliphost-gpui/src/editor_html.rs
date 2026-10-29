//! The same DOM-to-text rules as frontend/core/htmlToPlainText.ts.
//! The HTML5 parser is used only to derive the text projection. Saving retains
//! the original HTML bytes and SourceURL; this never serializes a parsed DOM.

use html5ever::{
    parse_document,
    tendril::{StrTendril, TendrilSink},
    tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink},
    Attribute, ExpandedName, QualName,
};
use std::{
    borrow::Cow,
    cell::RefCell,
    ops::Range,
    rc::{Rc, Weak},
};

/// A source-preserving, inert projection for the native rich editor. This is
/// deliberately separate from the HTML5 DOM used for the plain-text projection:
/// malformed/unknown markup remains in `source` and is never reserialized.
/// All indices are valid only until the next mutation; text ranges are UTF-8
/// byte ranges in `HtmlRun::text` (the same coordinates as MultilineInput).
#[derive(Debug, Clone)]
pub struct HtmlDocument {
    source: String,
    runs: Vec<HtmlRun>,
    blocks: Vec<HtmlBlock>,
    images: Vec<HtmlImage>,
    opaque: Vec<HtmlOpaque>,
    segments: Vec<Vec<TextSegment>>,
    run_ancestors: Vec<Vec<(String, String)>>,
    block_ancestors: Vec<Vec<(String, String)>>,
    block_ancestor_starts: Vec<Vec<usize>>,
    empty_text_anchor: Option<usize>,
    break_sources: Vec<(usize, usize, Range<usize>)>,
}

/// UTF-8 canvas coordinates. An image is U+FFFC (one UTF-16 unit); every line
/// has a structural newline, including the protected final document newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlCanvasProjection {
    pub text: String,
    pub spans: Vec<HtmlCanvasSpan>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlCanvasSpan {
    pub range: Range<usize>,
    pub kind: HtmlCanvasKind,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlCanvasBlock {
    pub index: Option<usize>,
    pub source_range: Range<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlCanvasKind {
    Text {
        run: usize,
    },
    Image {
        index: usize,
    },
    Break {
        block: usize,
        element: usize,
        source_range: Range<usize>,
    },
    Boundary {
        left: HtmlCanvasBlock,
        right: Option<HtmlCanvasBlock>,
        terminal: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtmlCanvasKey {
    Backspace,
    Enter,
}

#[derive(Clone, Copy)]
enum CanvasBlockEdit {
    Toggle(HtmlBlockMark),
    Indent(u8),
    CancelList(HtmlListKind, bool),
    CancelHeader(u8),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Option<String>,
    pub background: Option<String>,
    pub subscript: bool,
    pub superscript: bool,
    pub code: bool,
    /// Untrusted reference; rendering a run must not open this automatically.
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlRun {
    pub text: String,
    pub source_range: Range<usize>,
    pub block_index: usize,
    pub style: HtmlStyle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HtmlBlockKind {
    #[default]
    Paragraph,
    Header(u8),
    ListItem,
    Code,
    HorizontalRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtmlListKind {
    Ordered,
    Bullet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtmlElement {
    Text(usize),
    Image(usize),
    Break,
    HorizontalRule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlBlock {
    pub kind: HtmlBlockKind,
    pub list: Option<HtmlListKind>,
    pub indent: u8,
    pub quote: bool,
    pub code: bool,
    pub elements: Vec<HtmlElement>,
    /// Includes the owning block tag when available. It is not a DOM range;
    /// nested structures may overlap and must not be batch-rewritten by callers.
    pub source_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlImage {
    pub src: String,
    pub alt: String,
    pub title: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_range: Range<usize>,
    pub block_index: usize,
    display_style: Option<String>,
    src_attribute: Option<SourceAttribute>,
    insert_at: usize,
}

impl HtmlImage {
    /// Declared image geometry only. Percent stores the original percentage
    /// (50% is Percent(50.)); height percentages lack a definite height base.
    /// Auto clears an attr-sized axis, and means no constraint for max axes.
    /// Unresolved preserves valid-looking CSS requiring a real layout context.
    /// The renderer supplies intrinsic dimensions; this method performs no IO.
    pub fn display_layout(&self) -> HtmlImageLayout {
        let mut layout = HtmlImageLayout {
            width: self.width.map_or(HtmlImageDimension::Auto, |width| {
                HtmlImageDimension::Pixels(width as f32)
            }),
            height: self.height.map_or(HtmlImageDimension::Auto, |height| {
                HtmlImageDimension::Pixels(height as f32)
            }),
            ..Default::default()
        };
        if let Some(css) = &self.display_style {
            let declarations = css.split(';').filter_map(|part| part.split_once(':'));
            let ordinary = declarations
                .clone()
                .filter(|(_, value)| css_value_without_priority(value) == value.trim());
            let important =
                declarations.filter(|(_, value)| css_value_without_priority(value) != value.trim());
            for (name, value) in ordinary.chain(important) {
                let name = name.trim().to_ascii_lowercase();
                let (target, maximum, height) = match name.as_str() {
                    "width" => (&mut layout.width, false, false),
                    "height" => (&mut layout.height, false, true),
                    "max-width" => (&mut layout.max_width, true, false),
                    "max-height" => (&mut layout.max_height, true, true),
                    _ => continue,
                };
                if let Some(dimension) =
                    image_css_dimension(css_value_without_priority(value), maximum, height)
                {
                    *target = dimension;
                }
            }
        }
        layout
    }

    /// Classifies a reference without reading a file, decoding data, or making
    /// a network request. The host still owns authorization and path resolution.
    pub fn reference(&self) -> HtmlImageReference {
        let value = self.src.trim();
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") || value.starts_with("//")
        {
            HtmlImageReference::Remote
        } else if lower.starts_with("data:image/") {
            HtmlImageReference::Embedded
        } else if lower.starts_with("file:")
            || (!value.is_empty() && !value.contains(':'))
            || (value.as_bytes().get(1) == Some(&b':')
                && value
                    .as_bytes()
                    .get(2)
                    .is_some_and(|c| matches!(c, b'/' | b'\\')))
        {
            HtmlImageReference::Local
        } else {
            HtmlImageReference::Opaque
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum HtmlImageDimension {
    #[default]
    Auto,
    Pixels(f32),
    Percent(f32),
    Unresolved,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HtmlImageLayout {
    pub width: HtmlImageDimension,
    pub height: HtmlImageDimension,
    pub max_width: HtmlImageDimension,
    pub max_height: HtmlImageDimension,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtmlImageReference {
    Local,
    Remote,
    Embedded,
    Opaque,
}

/// Markup that the native projection cannot represent completely. The source
/// editor remains available for these ranges, including CSS and active content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlOpaque {
    pub tag: String,
    pub source_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlMark {
    Bold,
    Italic,
    Underline,
    Strike,
    Color(String),
    Background(String),
    Subscript,
    Superscript,
    Link(String),
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtmlBlockMark {
    Paragraph,
    Header(u8),
    List(HtmlListKind),
    Indent(u8),
    Blockquote,
    CodeBlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlEditError {
    MissingRun,
    MissingImage,
    MissingBlock,
    InvalidTextRange,
    InvalidSourceRange,
    InvalidHeader,
    InvalidColor,
    UnsafeReference,
    UnsupportedBlockStructure,
}

impl std::fmt::Display for HtmlEditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MissingRun => "HTML text run no longer exists",
            Self::MissingImage => "HTML image no longer exists",
            Self::MissingBlock => "HTML block no longer exists",
            Self::InvalidTextRange => "HTML text selection is not a valid UTF-8 range",
            Self::InvalidSourceRange => {
                "HTML source selection no longer refers to an editable text span"
            }
            Self::InvalidHeader => "HTML header level must be between 1 and 6",
            Self::InvalidColor => "HTML color is not a supported inert CSS color",
            Self::UnsafeReference => {
                "HTML reference uses an unsafe scheme or contains control characters"
            }
            Self::UnsupportedBlockStructure => {
                "HTML block formatting cannot be changed safely in this source structure"
            }
        })
    }
}
impl std::error::Error for HtmlEditError {}

#[derive(Debug, Clone)]
struct TextSegment {
    source: Range<usize>,
    text: Range<usize>,
    /// Literal spans map byte-for-byte and can be stored as one segment, rather
    /// than allocating one range per character in a large clipboard payload.
    direct: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceAttribute {
    name: String,
    value: String,
    /// Attribute value excluding quotes, or None for a boolean attribute.
    value_range: Option<Range<usize>>,
    quote: Option<u8>,
    source_range: Range<usize>,
}

#[derive(Debug)]
struct SourceTag {
    name: String,
    attrs: Vec<SourceAttribute>,
    range: Range<usize>,
    closing: bool,
    self_closing: bool,
    insert_at: usize,
}

#[derive(Clone, Default)]
struct BlockFormat {
    kind: HtmlBlockKind,
    list: Option<HtmlListKind>,
    indent: u8,
    quote: bool,
    code: bool,
}

struct SourceFrame {
    tag: String,
    style: HtmlStyle,
    format: BlockFormat,
    suppressed: bool,
    block_owner: bool,
    source_start: usize,
    blocks: Vec<usize>,
}

impl HtmlDocument {
    pub fn parse(source: impl Into<String>) -> Self {
        let mut document = Self {
            source: source.into(),
            runs: Vec::new(),
            blocks: Vec::new(),
            images: Vec::new(),
            opaque: Vec::new(),
            segments: Vec::new(),
            run_ancestors: Vec::new(),
            block_ancestors: Vec::new(),
            block_ancestor_starts: Vec::new(),
            empty_text_anchor: None,
            break_sources: Vec::new(),
        };
        document.project();
        document
    }

    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn runs(&self) -> &[HtmlRun] {
        &self.runs
    }

    /// Inline-format identity for Quill text-leaf keyboard context. Heading
    /// bold and PRE monospace are canvas font bases, not inline attributes;
    /// using the painted run style would merge distinct leaves in a heading.
    /// This reads only supported source metadata and never normalizes the DOM.
    pub fn canvas_inline_style(&self, run: usize) -> Option<HtmlStyle> {
        let run = self.runs.get(run)?;
        if self.blocks.get(run.block_index)?.code {
            // Quill CodeBlock has a plain Text leaf, even when imported source
            // still contains inline wrappers that our lossless model retains.
            return Some(HtmlStyle::default());
        }
        let mut style = HtmlStyle::default();
        let mut format = BlockFormat::default();
        for start in canvas_parent_starts_at(&self.source, run.source_range.start).ok()? {
            let mut tag = source_tag(&self.source, start)?;
            if is_block(&tag.name) {
                tag.name = "span".into();
            }
            apply_tag_style(&tag, &mut style, &mut format);
        }
        // The current front-end formats whitelist excludes inline code/font.
        style.code = false;
        Some(style)
    }
    pub fn blocks(&self) -> &[HtmlBlock] {
        &self.blocks
    }
    pub fn images(&self) -> &[HtmlImage] {
        &self.images
    }
    pub fn opaque(&self) -> &[HtmlOpaque] {
        &self.opaque
    }

    pub fn canvas_projection(&self) -> HtmlCanvasProjection {
        let mut lines = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.canvas_container_indentation(*index))
            .map(|(index, block)| HtmlCanvasBlock {
                index: Some(index),
                source_range: block.source_range.clone(),
            })
            .collect::<Vec<_>>();
        for (range, _) in empty_line_source_ranges(&self.source, true) {
            if !lines.iter().any(|line| line.source_range == range) {
                lines.push(HtmlCanvasBlock {
                    index: None,
                    source_range: range,
                });
            }
        }
        lines.sort_by_key(|line| {
            line.index
                .and_then(|index| {
                    self.blocks[index]
                        .elements
                        .iter()
                        .find_map(|element| match element {
                            HtmlElement::Text(run) => Some(self.runs[*run].source_range.start),
                            HtmlElement::Image(image) => {
                                Some(self.images[*image].source_range.start)
                            }
                            _ => self
                                .break_sources
                                .iter()
                                .find(|(block, _, _)| *block == index)
                                .map(|(_, _, range)| range.start),
                        })
                })
                .unwrap_or(line.source_range.start)
        });
        if lines.is_empty() {
            lines.push(HtmlCanvasBlock {
                index: None,
                source_range: 0..0,
            });
        }
        let mut projection = HtmlCanvasProjection {
            text: String::new(),
            spans: Vec::new(),
        };
        for (line_index, line) in lines.iter().enumerate() {
            if let Some(block_index) = line.index {
                let block = &self.blocks[block_index];
                for (element_index, element) in block.elements.iter().enumerate() {
                    let start = projection.text.len();
                    let kind = match element {
                        HtmlElement::Text(run) => {
                            projection.text.push_str(&self.runs[*run].text);
                            HtmlCanvasKind::Text { run: *run }
                        }
                        HtmlElement::Image(index) => {
                            projection.text.push('\u{fffc}');
                            HtmlCanvasKind::Image { index: *index }
                        }
                        HtmlElement::Break | HtmlElement::HorizontalRule => {
                            // A final br is Quill's line placeholder/terminator;
                            // the Boundary below represents that one newline.
                            if matches!(element, HtmlElement::Break)
                                && element_index + 1 == block.elements.len()
                            {
                                continue;
                            }
                            let Some((_, _, range)) =
                                self.break_sources.iter().find(|(index, position, _)| {
                                    *index == block_index && *position == element_index
                                })
                            else {
                                continue;
                            };
                            projection.text.push('\n');
                            HtmlCanvasKind::Break {
                                block: block_index,
                                element: element_index,
                                source_range: range.clone(),
                            }
                        }
                    };
                    projection.spans.push(HtmlCanvasSpan {
                        range: start..projection.text.len(),
                        kind,
                    });
                }
            }
            let start = projection.text.len();
            projection.text.push('\n');
            projection.spans.push(HtmlCanvasSpan {
                range: start..start + 1,
                kind: HtmlCanvasKind::Boundary {
                    left: line.clone(),
                    right: lines.get(line_index + 1).cloned(),
                    terminal: line_index + 1 == lines.len(),
                },
            });
        }
        projection
    }

    fn canvas_container_indentation(&self, index: usize) -> bool {
        let block = &self.blocks[index];
        !block.code
            && !block.elements.is_empty()
            && block.elements.iter().all(|element| match element {
                HtmlElement::Text(run) => self.runs[*run]
                    .text
                    .chars()
                    .all(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n' | '\u{c}')),
                _ => false,
            })
            && self.blocks.iter().enumerate().any(|(other, child)| {
                other != index
                    && !child.elements.is_empty()
                    && child.source_range.start >= block.source_range.start
                    && child.source_range.end <= block.source_range.end
                    && child.source_range != block.source_range
            })
    }

    /// Actual owner/ancestor formatting for both populated and virtual empty
    /// canvas lines. Empty li/header rows must not fall back to Paragraph merely
    /// because there is no text run from which to retrieve a block index.
    pub fn canvas_block_format(&self, block: &HtmlCanvasBlock) -> Option<HtmlBlock> {
        if let Some(index) = block.index {
            return self
                .blocks
                .get(index)
                .filter(|current| current.source_range == block.source_range)
                .cloned();
        }
        if block.source_range.is_empty() {
            return Some(HtmlBlock {
                kind: HtmlBlockKind::Paragraph,
                list: None,
                indent: 0,
                quote: false,
                code: false,
                elements: Vec::new(),
                source_range: block.source_range.clone(),
            });
        }
        let owner = source_tag(&self.source, block.source_range.start)?;
        let close = matching_close_tag(&self.source, owner.range.start)?;
        if owner.closing || close.range.end != block.source_range.end {
            return None;
        }
        let mut style = HtmlStyle::default();
        let mut format = BlockFormat::default();
        for start in canvas_parent_starts_at(&self.source, owner.range.start).ok()? {
            apply_tag_style(&source_tag(&self.source, start)?, &mut style, &mut format);
        }
        apply_tag_style(&owner, &mut style, &mut format);
        Some(HtmlBlock {
            kind: format.kind,
            list: format.list,
            indent: format.indent,
            quote: format.quote,
            code: format.code,
            elements: Vec::new(),
            source_range: block.source_range.clone(),
        })
    }

    /// Format an exact current canvas owner, including a virtual empty row.
    /// The temporary leaf is removed before publication, and a structural edit
    /// that changes the canvas text is rejected atomically. Callers retrieve the
    /// new owner from canvas_projection after success; source offsets may move.
    pub fn toggle_canvas_block(
        &mut self,
        block: &HtmlCanvasBlock,
        mark: HtmlBlockMark,
    ) -> Result<(), HtmlEditError> {
        let projection = self.canvas_projection();
        if !projection.spans.iter().any(
            |span| matches!(&span.kind, HtmlCanvasKind::Boundary { left, .. } if left == block),
        ) {
            return Err(HtmlEditError::InvalidSourceRange);
        }
        let mut candidate = self.clone();
        let line = if block.index.is_none() && block.source_range.is_empty() {
            candidate.append_paragraph("");
            candidate
                .canvas_projection()
                .spans
                .into_iter()
                .find_map(|span| match span.kind {
                    HtmlCanvasKind::Boundary { left, .. } => Some(left),
                    _ => None,
                })
                .ok_or(HtmlEditError::UnsupportedBlockStructure)?
        } else {
            block.clone()
        };
        let operation = match mark {
            HtmlBlockMark::Indent(level) => CanvasBlockEdit::Indent(level),
            _ => CanvasBlockEdit::Toggle(mark),
        };
        candidate.canvas_edit_line(&line, operation)?;
        if candidate.canvas_projection().text != projection.text {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        *self = candidate;
        Ok(())
    }

    /// Quill's collapsed-selection rules precede ordinary character deletion or
    /// newline insertion. Some means this atomic source operation consumed the
    /// key; None delegates to replace_canvas_range. Unknown/malformed source is
    /// never repaired by rebuilding a DOM or by guessing which empty line moved.
    pub fn canvas_keyboard_edit(
        &mut self,
        caret: usize,
        key: HtmlCanvasKey,
    ) -> Result<Option<usize>, HtmlEditError> {
        let projection = self.canvas_projection();
        if caret > projection.text.len() || !projection.text.is_char_boundary(caret) {
            return Err(HtmlEditError::InvalidTextRange);
        }
        let caret = caret.min(projection.text.len().saturating_sub(1));
        let mut start = 0;
        let mut line = None;
        for span in &projection.spans {
            if let HtmlCanvasKind::Boundary { left, .. } = &span.kind {
                if caret >= start && caret <= span.range.start {
                    line = Some((start, span.range.start, left.clone()));
                    break;
                }
                start = span.range.end;
            }
        }
        let Some((start, end, block)) = line else {
            return Ok(None);
        };
        let format = self
            .canvas_block_format(&block)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let operation = match key {
            HtmlCanvasKey::Backspace if caret == start && format.indent > 0 => {
                Some(CanvasBlockEdit::Indent(format.indent - 1))
            }
            HtmlCanvasKey::Backspace if caret == start && format.list.is_some() => Some(
                CanvasBlockEdit::CancelList(format.list.expect("active list"), false),
            ),
            HtmlCanvasKey::Enter if start == end && format.list.is_some() => Some(
                CanvasBlockEdit::CancelList(format.list.expect("active list"), true),
            ),
            _ => None,
        };
        if let Some(operation) = operation {
            let mut candidate = self.clone();
            candidate.canvas_edit_line(&block, operation)?;
            if candidate.canvas_projection().text != projection.text {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
            *self = candidate;
            return Ok(Some(caret));
        }
        if key == HtmlCanvasKey::Enter && caret == end {
            if let HtmlBlockKind::Header(level) = format.kind {
                let mut candidate = self.clone();
                let next_caret = candidate.replace_canvas_range(caret..caret, "\n")?;
                let next = candidate.canvas_projection();
                let new_line = next
                    .spans
                    .iter()
                    .find_map(|span| match &span.kind {
                        HtmlCanvasKind::Boundary { left, .. } if span.range.start == next_caret => {
                            Some(left.clone())
                        }
                        _ => None,
                    })
                    .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
                candidate.canvas_edit_line(&new_line, CanvasBlockEdit::CancelHeader(level))?;
                if candidate.canvas_projection().text != next.text {
                    return Err(HtmlEditError::UnsupportedBlockStructure);
                }
                *self = candidate;
                return Ok(Some(next_caret));
            }
        }
        Ok(None)
    }

    fn canvas_edit_line(
        &mut self,
        line: &HtmlCanvasBlock,
        operation: CanvasBlockEdit,
    ) -> Result<(), HtmlEditError> {
        let (open, _) = self.canvas_paragraph_tags(line)?;
        // A temporary text leaf makes the existing source-preserving structural
        // helpers available for images/empty lines too. Track its source range
        // and run ordinal, never text equality; remove it before publication.
        let marker = open.range.end;
        self.patch(marker..marker, "x");
        let ordinal = self
            .runs
            .iter()
            .position(|run| run.source_range.start == marker && run.source_range.end > marker)
            .ok_or(HtmlEditError::MissingRun)?;
        let block_index = self.runs[ordinal].block_index;
        match operation {
            CanvasBlockEdit::Toggle(mark) => {
                let block = &self.blocks[block_index];
                let active = match mark {
                    HtmlBlockMark::Header(level) => block.kind == HtmlBlockKind::Header(level),
                    HtmlBlockMark::List(kind) => block.list == Some(kind),
                    HtmlBlockMark::Blockquote => block.quote,
                    HtmlBlockMark::CodeBlock => block.code,
                    HtmlBlockMark::Paragraph | HtmlBlockMark::Indent(_) => false,
                };
                if !active || !self.canvas_cancel_single_owner(block_index, mark)? {
                    self.toggle_block(block_index, mark)?;
                }
            }
            CanvasBlockEdit::Indent(level) => {
                self.format_block(block_index, HtmlBlockMark::Indent(level))?;
                let index = self
                    .runs
                    .get(ordinal)
                    .ok_or(HtmlEditError::MissingRun)?
                    .block_index;
                if self.blocks[index].indent != level {
                    self.cancel_block_format(index, HtmlBlockMark::Indent(0))?;
                    if level > 0 {
                        let index = self
                            .runs
                            .get(ordinal)
                            .ok_or(HtmlEditError::MissingRun)?
                            .block_index;
                        self.format_block(index, HtmlBlockMark::Indent(level))?;
                    }
                }
            }
            CanvasBlockEdit::CancelList(kind, clear_indent) => {
                self.toggle_block(block_index, HtmlBlockMark::List(kind))?;
                let index = self
                    .runs
                    .get(ordinal)
                    .ok_or(HtmlEditError::MissingRun)?
                    .block_index;
                if clear_indent && self.blocks[index].indent > 0 {
                    self.cancel_block_format(index, HtmlBlockMark::Indent(0))?;
                }
            }
            CanvasBlockEdit::CancelHeader(level) => {
                self.toggle_block(block_index, HtmlBlockMark::Header(level))?;
            }
        }
        let marker_run = self.runs.get(ordinal).ok_or(HtmlEditError::MissingRun)?;
        let marker_range = marker_run.source_range.start..marker_run.source_range.start + 1;
        let emptied = marker_run.text.len() == 1
            && self.blocks[marker_run.block_index]
                .elements
                .iter()
                .all(|element| matches!(element, HtmlElement::Text(run) if *run == ordinal));
        self.replace_source_text_span(marker_range.clone(), "")?;
        if emptied {
            // Structural cancellation may neutralize li/header to div. Keep a
            // standard br placeholder so the now-empty line remains addressable,
            // without deleting any original attrs/comments or manufacturing text.
            if !source_tag(&self.source, marker_range.start)
                .is_some_and(|tag| !tag.closing && tag.name == "br")
            {
                self.patch(marker_range.start..marker_range.start, "<br>");
            }
        }
        Ok(())
    }

    /// A format wrapper containing just this owner needs no split/reopen seam.
    /// Neutralize its exact tags in place so an empty nested list cannot acquire
    /// extra synthetic empty li rows. Gaps may contain comments/ASCII indentation
    /// only; any sibling or unknown object takes the ordinary isolation path.
    fn canvas_cancel_single_owner(
        &mut self,
        index: usize,
        mark: HtmlBlockMark,
    ) -> Result<bool, HtmlEditError> {
        let range = self.check_block_scope(index)?;
        let ancestors = &self.block_ancestors[index];
        let Some(first) = ancestors
            .iter()
            .position(|(_, markup)| block_markup_has_format(markup, mark))
        else {
            return Ok(false);
        };
        let owner = source_tag(&self.source, range.start)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let owner_close = matching_close_tag(&self.source, range.start)
            .filter(|close| close.range.end == range.end)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let mut tags = Vec::new();
        for start in &self.block_ancestor_starts[index][first..] {
            let open =
                source_tag(&self.source, *start).ok_or(HtmlEditError::UnsupportedBlockStructure)?;
            let close = matching_close_tag(&self.source, *start)
                .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
            tags.push((open, close));
        }
        let mut inner_start = owner.range.start;
        let mut inner_end = owner_close.range.end;
        for (open, close) in tags.iter().rev() {
            if open.range.end > inner_start
                || close.range.start < inner_end
                || !inert_paragraph_gap(&self.source[open.range.end..inner_start])
                || !inert_paragraph_gap(&self.source[inner_end..close.range.start])
            {
                return Ok(false);
            }
            inner_start = open.range.start;
            inner_end = close.range.end;
        }
        tags.push((owner, owner_close));
        let mut patches = Vec::new();
        for (open, close) in tags {
            let (name, opening) = neutralize_block_markup(&self.source[open.range.clone()], mark);
            let closing =
                renamed_closing_markup(&self.source[close.range.clone()], &close.name, &name);
            if opening != self.source[open.range.clone()] {
                patches.push((open.range, opening));
            }
            if closing != self.source[close.range.clone()] {
                patches.push((close.range, closing));
            }
        }
        if patches.is_empty() {
            return Ok(false);
        }
        apply_canvas_patches(&mut self.source, patches)?;
        *self = Self::parse(std::mem::take(&mut self.source));
        Ok(true)
    }

    /// Replace a current canvas UTF-8 range atomically. Text/entity patches and
    /// explicitly selected images are local source edits; unknown tags/comments
    /// are retained. Only safe ordinary paragraph seams are joined/split. The
    /// final newline is protected, matching Quill's minimum one-line document.
    pub fn replace_canvas_range(
        &mut self,
        range: Range<usize>,
        text: &str,
    ) -> Result<usize, HtmlEditError> {
        self.replace_canvas_range_markup(range, text, None)
    }

    /// Insert one inert image reference at the exact current canvas selection.
    /// Source patches replace selected text/objects and retain unrelated tags,
    /// entities and comments. No reference is loaded or opened by this model.
    pub fn insert_canvas_image(
        &mut self,
        selection: Range<usize>,
        src: &str,
        alt: &str,
    ) -> Result<usize, HtmlEditError> {
        validate_reference(src, true)?;
        if src.trim().is_empty() {
            return Err(HtmlEditError::UnsafeReference);
        }
        let before = self.canvas_projection();
        if selection.start > selection.end
            || selection.end > before.text.len()
            || !before.text.is_char_boundary(selection.start)
            || !before.text.is_char_boundary(selection.end)
        {
            return Err(HtmlEditError::InvalidTextRange);
        }
        let terminal = before.text.len().saturating_sub(1);
        let start = selection.start.min(terminal);
        let end = selection.end.min(terminal).max(start);
        let expected = format!("{}\u{fffc}{}", &before.text[..start], &before.text[end..]);
        let markup = format!(
            "<img src=\"{}\" alt=\"{}\">",
            escape_html_attribute(src),
            escape_html_attribute(alt)
        );
        let mut candidate = self.clone();
        let caret = candidate.replace_canvas_range_markup(selection, "\u{fffc}", Some(&markup))?;
        let after = candidate.canvas_projection();
        if after.text != expected
            || !after.spans.iter().any(|span| {
                span.range == (start..start + '\u{fffc}'.len_utf8())
                    && matches!(span.kind, HtmlCanvasKind::Image { .. })
            })
        {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        *self = candidate;
        Ok(caret)
    }

    fn replace_canvas_range_markup(
        &mut self,
        range: Range<usize>,
        text: &str,
        image_markup: Option<&str>,
    ) -> Result<usize, HtmlEditError> {
        let projection = self.canvas_projection();
        if range.start > range.end
            || range.end > projection.text.len()
            || !projection.text.is_char_boundary(range.start)
            || !projection.text.is_char_boundary(range.end)
        {
            return Err(HtmlEditError::InvalidTextRange);
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let start = range.start.min(projection.text.len().saturating_sub(1));
        let end = range.end.min(projection.text.len().saturating_sub(1));
        let range = start..end.max(start);
        if range.is_empty() && text.is_empty() {
            return Ok(start);
        }
        let right = projection
            .spans
            .iter()
            .find(|span| span.range.start <= start && start < span.range.end)
            .ok_or(HtmlEditError::InvalidTextRange)?;
        // Collapsed input keeps the effective style at the caret: the left text
        // leaf owns its trailing edge, just as toolbar lookup does. Structural
        // newlines separate paragraphs, so this cannot borrow the prior row's
        // formatting. A nonempty replacement retains the selected right leaf;
        // newline insertion retains the structural anchor so a new empty row
        // does not become an unaddressable nest of empty inline wrappers.
        let first = if range.is_empty() && !text.contains('\n') {
            projection
                .spans
                .iter()
                .rev()
                .find(|span| {
                    span.range.end == start && matches!(span.kind, HtmlCanvasKind::Text { .. })
                })
                .unwrap_or(right)
        } else {
            right
        };
        let (insert_source, insertion_block, insertion_run) = match &first.kind {
            HtmlCanvasKind::Text { run } => {
                let local = start - first.range.start;
                let (source, _, _, _) = self.selection_source(*run, local..local)?;
                (
                    source.start,
                    Some(HtmlCanvasBlock {
                        index: Some(self.runs[*run].block_index),
                        source_range: self.blocks[self.runs[*run].block_index]
                            .source_range
                            .clone(),
                    }),
                    Some(*run),
                )
            }
            HtmlCanvasKind::Image { index } => (
                self.images[*index].source_range.start,
                Some(HtmlCanvasBlock {
                    index: Some(self.images[*index].block_index),
                    source_range: self.blocks[self.images[*index].block_index]
                        .source_range
                        .clone(),
                }),
                None,
            ),
            HtmlCanvasKind::Break {
                block,
                source_range,
                ..
            } => (
                source_range.start,
                Some(HtmlCanvasBlock {
                    index: Some(*block),
                    source_range: self.blocks[*block].source_range.clone(),
                }),
                None,
            ),
            HtmlCanvasKind::Boundary { left, .. } => {
                if left.source_range.is_empty() {
                    (paragraph_insertion_point(&self.source), None, None)
                } else {
                    let at = source_tag(&self.source, left.source_range.start)
                        .filter(|tag| !tag.closing && is_block(&tag.name))
                        .and_then(|open| matching_close_tag(&self.source, open.range.start))
                        .filter(|close| close.range.end == left.source_range.end)
                        .map_or(left.source_range.end, |close| close.range.start);
                    (at, Some(left.clone()), None)
                }
            }
        };
        if image_markup.is_some() {
            if let Some(block) = insertion_block.as_ref().filter(|block| {
                source_tag(&self.source, block.source_range.start)
                    .is_some_and(|tag| !tag.closing && is_block(&tag.name))
            }) {
                self.canvas_paragraph_tags(block)?;
            }
            // A caret in an unclosed/crossed source path cannot safely create
            // an object. Balanced unknown inline wrappers remain byte-identical.
            for start in canvas_parent_starts_at(&self.source, insert_source)? {
                matching_close_tag(&self.source, start)
                    .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
            }
        }
        let insertion_markup = if let Some(markup) = image_markup {
            if insertion_block.is_some() {
                markup.to_string()
            } else {
                format!("<p>{markup}</p>")
            }
        } else if let Some(block) = &insertion_block {
            if text.contains('\n') {
                self.canvas_split_markup(block, insertion_run, insert_source, &text)?
            } else {
                escape_html_text(&text)
            }
        } else {
            format!(
                "<p>{}</p>",
                text.split('\n')
                    .map(escape_html_text)
                    .collect::<Vec<_>>()
                    .join("</p><p>")
            )
        };
        let mut patches: Vec<(Range<usize>, String)> = Vec::new();
        let mut inserted = false;
        let mut merges: Vec<Vec<HtmlCanvasBlock>> = Vec::new();
        for span in &projection.spans {
            let from = range.start.max(span.range.start);
            let to = range.end.min(span.range.end);
            let starts_here = std::ptr::eq(span, first);
            if from >= to && !(starts_here && range.is_empty()) {
                continue;
            }
            match &span.kind {
                HtmlCanvasKind::Text { run } => {
                    let local = (from - span.range.start)..(to - span.range.start);
                    let (source, prefix, _, suffix) = self.selection_source(*run, local)?;
                    let value = format!(
                        "{}{}{}",
                        escape_html_text(&prefix),
                        if starts_here {
                            insertion_markup.as_str()
                        } else {
                            ""
                        },
                        escape_html_text(&suffix)
                    );
                    patches.push((source, value));
                    inserted |= starts_here;
                }
                HtmlCanvasKind::Image { index } => {
                    let image = &self.images[*index];
                    let source = if from < to {
                        image.source_range.clone()
                    } else {
                        image.source_range.start..image.source_range.start
                    };
                    patches.push((
                        source,
                        if starts_here {
                            insertion_markup.clone()
                        } else {
                            String::new()
                        },
                    ));
                    inserted |= starts_here;
                }
                HtmlCanvasKind::Break { source_range, .. } => {
                    patches.push((
                        if from < to {
                            source_range.clone()
                        } else {
                            source_range.start..source_range.start
                        },
                        if starts_here {
                            insertion_markup.clone()
                        } else {
                            String::new()
                        },
                    ));
                    inserted |= starts_here;
                }
                HtmlCanvasKind::Boundary {
                    left,
                    right,
                    terminal,
                } => {
                    if !terminal && from < to {
                        let right = right
                            .as_ref()
                            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
                        self.canvas_paragraph_tags(left)?;
                        self.canvas_paragraph_tags(right)?;
                        if self.canvas_parent_starts(left)? != self.canvas_parent_starts(right)?
                            || left.source_range.end > right.source_range.start
                            || !inert_paragraph_gap(
                                &self.source[left.source_range.end..right.source_range.start],
                            )
                        {
                            return Err(HtmlEditError::UnsupportedBlockStructure);
                        }
                        if let Some(group) =
                            merges.last_mut().filter(|group| group.last() == Some(left))
                        {
                            group.push(right.clone());
                        } else {
                            merges.push(vec![left.clone(), right.clone()]);
                        }
                    }
                }
            }
        }
        if !inserted {
            patches.push((insert_source..insert_source, insertion_markup));
        }
        // Remove a safe empty p's br placeholder on first input. Unknown br
        // attributes remain protected rather than being silently discarded.
        if matches!(first.kind, HtmlCanvasKind::Boundary { .. }) && !text.is_empty() {
            if let Some(block) = &insertion_block {
                self.canvas_placeholder_patch(block, &mut patches)?;
            }
        }
        for group in merges {
            let (_, first_close) = self.canvas_paragraph_tags(&group[0])?;
            patches.push((first_close.range.clone(), String::new()));
            self.canvas_placeholder_patch(&group[0], &mut patches)?;
            for right in &group[1..] {
                let (open, close) = self.canvas_paragraph_tags(right)?;
                patches.push((
                    open.range.start + 1..open.range.start + 1 + open.name.len(),
                    "span".into(),
                ));
                patches.push((
                    close.range.start + 2..close.range.start + 2 + close.name.len(),
                    "span".into(),
                ));
                self.canvas_placeholder_patch(right, &mut patches)?;
            }
            for pair in group.windows(2) {
                // Formatter indentation belonged to the deleted paragraph seam,
                // not either payload. Keep comment bytes while removing only
                // that ASCII whitespace so it cannot become new in-line text.
                for range in paragraph_gap_whitespace(
                    &self.source,
                    pair[0].source_range.end..pair[1].source_range.start,
                ) {
                    patches.push((range, String::new()));
                }
            }
            let last = group.last().expect("merge paragraphs");
            patches.push((
                last.source_range.end..last.source_range.end,
                self.source[first_close.range].to_string(),
            ));
        }
        let mut candidate = self.clone();
        apply_canvas_patches(&mut candidate.source, patches)?;
        candidate = Self::parse(candidate.source);
        let new_projection = candidate.canvas_projection();
        let caret = (start + text.len()).min(new_projection.text.len().saturating_sub(1));
        *self = candidate;
        Ok(caret)
    }

    fn canvas_paragraph_tags(
        &self,
        block: &HtmlCanvasBlock,
    ) -> Result<(SourceTag, SourceTag), HtmlEditError> {
        if let Some(index) = block.index {
            self.check_block_scope(index)?;
        }
        let open = source_tag(&self.source, block.source_range.start)
            .filter(|tag| {
                !tag.closing
                    && matches!(
                        tag.name.as_str(),
                        "p" | "div"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "li"
                            | "blockquote"
                            | "pre"
                    )
            })
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let close = matching_close_tag(&self.source, open.range.start)
            .filter(|tag| tag.range.end == block.source_range.end)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        Ok((open, close))
    }

    fn canvas_parent_starts(&self, block: &HtmlCanvasBlock) -> Result<Vec<usize>, HtmlEditError> {
        let starts = if let Some(index) = block.index {
            self.block_ancestor_starts[index].clone()
        } else {
            canvas_parent_starts_at(&self.source, block.source_range.start)?
        };
        for start in &starts {
            if matching_close_tag(&self.source, *start).is_none() {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
        }
        Ok(starts)
    }

    fn canvas_split_markup(
        &self,
        block: &HtmlCanvasBlock,
        run: Option<usize>,
        at: usize,
        text: &str,
    ) -> Result<String, HtmlEditError> {
        let (open, close) = self.canvas_paragraph_tags(block)?;
        let path;
        let ancestors = if let Some(run) = run {
            self.run_ancestors[run].as_slice()
        } else {
            path = canvas_inline_path(&self.source, open.range.end, at)?;
            path.as_slice()
        };
        let close_inline = ancestors
            .iter()
            .rev()
            .map(|(tag, _)| format!("</{tag}>"))
            .collect::<String>();
        let open_inline = ancestors
            .iter()
            .map(|(_, markup)| markup.as_str())
            .collect::<String>();
        let seam = format!(
            "{close_inline}{}{}{open_inline}",
            &self.source[close.range], &self.source[open.range]
        );
        Ok(text
            .split('\n')
            .map(escape_html_text)
            .collect::<Vec<_>>()
            .join(&seam))
    }

    fn canvas_placeholder_patch(
        &self,
        block: &HtmlCanvasBlock,
        patches: &mut Vec<(Range<usize>, String)>,
    ) -> Result<(), HtmlEditError> {
        let Some(index) = block.index else {
            return Ok(());
        };
        let elements = &self.blocks[index].elements;
        if !matches!(elements.last(), Some(HtmlElement::Break)) {
            return Ok(());
        }
        let (_, _, range) = self
            .break_sources
            .iter()
            .find(|(block, element, _)| *block == index && *element + 1 == elements.len())
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let tag = source_tag(&self.source, range.start)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        if !tag.attrs.is_empty() {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        if !patches.iter().any(|(other, _)| other == range) {
            patches.push((range.clone(), String::new()));
        }
        Ok(())
    }

    pub fn replace_text(&mut self, index: usize, text: &str) -> Result<(), HtmlEditError> {
        let length = self
            .runs
            .get(index)
            .ok_or(HtmlEditError::MissingRun)?
            .text
            .len();
        self.replace_text_range(index, 0..length, text)
    }

    /// For an active MultilineInput: keep this returned anchor after each edit,
    /// including empty input, rather than resolving a now-deleted run index.
    /// Callers must reset the anchor after an unrelated source/format mutation.
    pub fn replace_source_text_span(
        &mut self,
        range: Range<usize>,
        text: &str,
    ) -> Result<Range<usize>, HtmlEditError> {
        if range.start > range.end
            || range.end > self.source.len()
            || !self.source.is_char_boundary(range.start)
            || !self.source.is_char_boundary(range.end)
            || (!range.is_empty()
                && !self.runs.iter().any(|run| {
                    range.start >= run.source_range.start && range.end <= run.source_range.end
                }))
            || (range.is_empty()
                && self.empty_text_anchor != Some(range.start)
                && !self.runs.iter().any(|run| {
                    range.start >= run.source_range.start && range.start <= run.source_range.end
                }))
        {
            return Err(HtmlEditError::InvalidSourceRange);
        }
        let escaped = escape_html_text(text);
        let result = range.start..range.start + escaped.len();
        self.patch(range, &escaped);
        self.empty_text_anchor = result.is_empty().then_some(result.start);
        Ok(result)
    }

    /// Apply block formatting using a local source wrapper. Existing element
    /// attributes and unknown contents remain byte-identical. List-item owners
    /// are changed to div before wrapping to avoid nesting li directly in li.
    pub fn format_block(&mut self, index: usize, mark: HtmlBlockMark) -> Result<(), HtmlEditError> {
        let range = self
            .blocks
            .get(index)
            .ok_or(HtmlEditError::MissingBlock)?
            .source_range
            .clone();
        let original = self.source[range.clone()].to_string();
        let owner = source_tag(&self.source, range.start)
            .filter(|tag| tag.range.end <= range.end && !tag.closing && is_block(&tag.name));
        let closing = owner.as_ref().and_then(|tag| {
            let relative = original.rfind("</")?;
            let closing = source_tag(&self.source, range.start + relative)?;
            (closing.closing && closing.name == tag.name && closing.range.end == range.end)
                .then_some(closing)
        });
        if let (HtmlBlockMark::Indent(level), Some(tag)) = (mark, &owner) {
            let previous = tag
                .attrs
                .iter()
                .find(|attr| attr.name == "class")
                .map(|attr| attr.value.as_str())
                .unwrap_or("");
            let mut classes = previous
                .split_ascii_whitespace()
                .filter(|class| !class.starts_with("ql-indent-"))
                .map(str::to_string)
                .collect::<Vec<_>>();
            if level > 0 {
                classes.push(format!("ql-indent-{level}"));
            }
            let (attribute_range, replacement) = attribute_patch(tag, "class", &classes.join(" "));
            self.patch(attribute_range, &replacement);
            return Ok(());
        }
        let (open, close) = match mark {
            HtmlBlockMark::Paragraph => ("<div>".to_string(), "</div>"),
            HtmlBlockMark::Header(level) => {
                if !(1..=6).contains(&level) {
                    return Err(HtmlEditError::InvalidHeader);
                }
                (
                    format!("<h{level}>"),
                    match level {
                        1 => "</h1>",
                        2 => "</h2>",
                        3 => "</h3>",
                        4 => "</h4>",
                        5 => "</h5>",
                        _ => "</h6>",
                    },
                )
            }
            HtmlBlockMark::List(HtmlListKind::Ordered) => ("<ol><li>".into(), "</li></ol>"),
            HtmlBlockMark::List(HtmlListKind::Bullet) => ("<ul><li>".into(), "</li></ul>"),
            HtmlBlockMark::Indent(level) => {
                (format!("<div class=\"ql-indent-{level}\">"), "</div>")
            }
            HtmlBlockMark::Blockquote => ("<blockquote>".into(), "</blockquote>"),
            HtmlBlockMark::CodeBlock => ("<pre>".into(), "</pre>"),
        };
        // A block heading/pre owns its text directly; replace only the tag
        // names, retaining capitalization/spacing/attributes outside those names.
        let new_tag = match &mark {
            HtmlBlockMark::Paragraph => Some("div".to_string()),
            HtmlBlockMark::Header(level) => Some(format!("h{level}")),
            HtmlBlockMark::CodeBlock => Some("pre".to_string()),
            _ => None,
        };
        let mut replacement =
            if let (Some(tag), Some(end_tag), Some(new_tag)) = (&owner, &closing, new_tag) {
                replace_owner_tag_names(&original, range.start, tag, end_tag, &new_tag)
            } else {
                let contents = if let (Some(tag), Some(end_tag)) = (&owner, &closing) {
                    if tag.name == "li" {
                        replace_owner_tag_names(&original, range.start, tag, end_tag, "div")
                    } else {
                        original.clone()
                    }
                } else {
                    original.clone()
                };
                format!("{open}{contents}{close}")
            };
        if owner.as_ref().is_some_and(|tag| tag.name == "li") && closing.is_some() {
            let ancestors = &self.block_ancestors[index];
            if let Some(list_index) = ancestors
                .iter()
                .rposition(|(tag, _)| matches!(tag.as_str(), "ol" | "ul"))
            {
                let list_ancestors = &ancestors[list_index..];
                let list_tag = &list_ancestors[0].0;
                if let HtmlBlockMark::List(kind) = mark {
                    let new_list_tag = match kind {
                        HtmlListKind::Ordered => "ol",
                        HtmlListKind::Bullet => "ul",
                    };
                    if list_tag == new_list_tag {
                        return Ok(());
                    }
                    let mut new_open = list_ancestors[0].1.clone();
                    new_open.replace_range(1..1 + list_tag.len(), new_list_tag);
                    // Keep the original li and its attrs instead of placing li
                    // directly inside li. Split/reopen the existing list around
                    // this item so unselected siblings keep their list kind.
                    replacement = format!("{new_open}{original}</{new_list_tag}>");
                }
                let close_ancestors = list_ancestors
                    .iter()
                    .rev()
                    .map(|(tag, _)| format!("</{tag}>"))
                    .collect::<String>();
                let reopen = list_ancestors
                    .iter()
                    .map(|(_, markup)| markup.as_str())
                    .collect::<String>();
                replacement = format!("{close_ancestors}{replacement}{reopen}");
            }
        }
        self.patch(range, &replacement);
        Ok(())
    }

    /// Button semantics for structural formats. Indent remains an explicit set
    /// operation. Cancellation isolates this block from matching ancestors so
    /// other lines keep their original structure, attributes and formatting.
    pub fn toggle_block(&mut self, index: usize, mark: HtmlBlockMark) -> Result<(), HtmlEditError> {
        self.check_block_scope(index)?;
        let block = self.blocks.get(index).ok_or(HtmlEditError::MissingBlock)?;
        let active = match mark {
            HtmlBlockMark::Header(level) => block.kind == HtmlBlockKind::Header(level),
            HtmlBlockMark::List(kind) => block.list == Some(kind),
            HtmlBlockMark::Blockquote => block.quote,
            HtmlBlockMark::CodeBlock => block.code,
            HtmlBlockMark::Paragraph | HtmlBlockMark::Indent(_) => false,
        };
        if active {
            self.cancel_block_format(index, mark)
        } else {
            self.format_block(index, mark)
        }
    }

    fn check_block_scope(&self, index: usize) -> Result<Range<usize>, HtmlEditError> {
        let block = self.blocks.get(index).ok_or(HtmlEditError::MissingBlock)?;
        let range = block.source_range.clone();
        // A direct-text container that owns several projected child blocks
        // cannot be rewritten as one selected line without affecting siblings.
        if self.blocks.iter().enumerate().any(|(other, block)| {
            other != index
                && block.source_range.start >= range.start
                && block.source_range.end <= range.end
                && !block.elements.is_empty()
        }) {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        Ok(range)
    }

    fn cancel_block_format(
        &mut self,
        index: usize,
        mark: HtmlBlockMark,
    ) -> Result<(), HtmlEditError> {
        let range = self.check_block_scope(index)?;
        let owner = source_tag(&self.source, range.start)
            .filter(|tag| !tag.closing && is_block(&tag.name))
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let owner_close = matching_close_tag(&self.source, owner.range.start)
            .filter(|tag| tag.range.end == range.end)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        let original = &self.source[range.clone()];
        let (owner_name, owner_open) =
            neutralize_block_markup(&self.source[owner.range.clone()], mark);
        let owner_close_markup = renamed_closing_markup(
            &self.source[owner_close.range.clone()],
            &owner_close.name,
            &owner_name,
        );
        let mut selected_block = original.to_string();
        selected_block.replace_range(
            owner_close.range.start - range.start..owner_close.range.end - range.start,
            &owner_close_markup,
        );
        selected_block.replace_range(0..owner.range.len(), &owner_open);
        let ancestors = &self.block_ancestors[index];
        let first = ancestors
            .iter()
            .position(|(_, markup)| block_markup_has_format(markup, mark));
        let replacement = if let Some(first) = first {
            let mut originals = Vec::new();
            let mut selected = Vec::new();
            for ((name, markup), start) in ancestors[first..]
                .iter()
                .zip(&self.block_ancestor_starts[index][first..])
            {
                let close = matching_close_tag(&self.source, *start)
                    .filter(|tag| tag.range.start >= range.end)
                    .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
                let closing = self.source[close.range.clone()].to_string();
                let (new_name, new_open) = neutralize_block_markup(markup, mark);
                let new_close = renamed_closing_markup(&closing, name, &new_name);
                originals.push((markup.clone(), closing));
                selected.push((new_open, new_close));
            }
            format!(
                "{}{}{}{}{}",
                originals
                    .iter()
                    .rev()
                    .map(|(_, close)| close.as_str())
                    .collect::<String>(),
                selected
                    .iter()
                    .map(|(open, _)| open.as_str())
                    .collect::<String>(),
                selected_block,
                selected
                    .iter()
                    .rev()
                    .map(|(_, close)| close.as_str())
                    .collect::<String>(),
                originals
                    .iter()
                    .map(|(open, _)| open.as_str())
                    .collect::<String>()
            )
        } else {
            selected_block
        };
        if replacement == original {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        self.patch(range, &replacement);
        Ok(())
    }

    /// Edits a selected decoded text range while retaining surrounding entities
    /// and every byte outside the touched source span. Selecting part of a
    /// multi-character entity expands only that entity and escapes its remainder.
    pub fn replace_text_range(
        &mut self,
        index: usize,
        range: Range<usize>,
        text: &str,
    ) -> Result<(), HtmlEditError> {
        let (source_range, prefix, _, suffix) = self.selection_source(index, range)?;
        self.patch(
            source_range,
            &format!(
                "{}{}{}",
                escape_html_text(&prefix),
                escape_html_text(text),
                escape_html_text(&suffix)
            ),
        );
        Ok(())
    }

    pub fn wrap_text(&mut self, index: usize, mark: HtmlMark) -> Result<(), HtmlEditError> {
        let length = self
            .runs
            .get(index)
            .ok_or(HtmlEditError::MissingRun)?
            .text
            .len();
        self.wrap_text_range(index, 0..length, mark)
    }

    pub fn wrap_text_range(
        &mut self,
        index: usize,
        range: Range<usize>,
        mark: HtmlMark,
    ) -> Result<(), HtmlEditError> {
        self.wrap_text_range_anchor(index, range, mark).map(|_| ())
    }

    /// Return the exact newly selected raw text span, including its entity
    /// spelling. Callers can resolve the run by source_range rather than text
    /// equality, which is ambiguous when several runs contain the same text.
    pub fn wrap_text_range_anchor(
        &mut self,
        index: usize,
        range: Range<usize>,
        mark: HtmlMark,
    ) -> Result<Range<usize>, HtmlEditError> {
        let (open, close) = mark_markup(mark)?;
        let (source_range, prefix, selected, suffix) = self.selection_source(index, range)?;
        let prefix_escaped = escape_html_text(&prefix);
        let start = source_range.start + prefix_escaped.len() + open.len();
        let anchor = start..start + selected.len();
        self.patch(
            source_range,
            &format!(
                "{}{}{}{}{}",
                prefix_escaped,
                open,
                selected,
                close,
                escape_html_text(&suffix)
            ),
        );
        self.empty_text_anchor = anchor.is_empty().then_some(anchor.start);
        Ok(anchor)
    }

    pub fn toggle_text(&mut self, index: usize, mark: HtmlMark) -> Result<(), HtmlEditError> {
        let length = self
            .runs
            .get(index)
            .ok_or(HtmlEditError::MissingRun)?
            .text
            .len();
        self.toggle_text_range(index, 0..length, mark)
    }

    /// A Quill-style toolbar toggle on a decoded UTF-8 selection. Every run has
    /// one effective style: clicking an active mark cancels only that mark;
    /// clicking a different color/link value applies that value. Empty selections
    /// are a no-op (the UI owns formatting for subsequently typed characters).
    /// Run indices and source anchors must be re-resolved after this mutation.
    pub fn toggle_text_range(
        &mut self,
        index: usize,
        range: Range<usize>,
        mark: HtmlMark,
    ) -> Result<(), HtmlEditError> {
        self.toggle_text_range_anchor(index, range, mark)
            .map(|_| ())
    }

    /// Exact counterpart to toggle_text_range. An empty decoded selection does
    /// not mutate the source and returns its existing empty source anchor. A
    /// caret inside a multi-character entity uses that indivisible token's start.
    pub fn toggle_text_range_anchor(
        &mut self,
        index: usize,
        range: Range<usize>,
        mark: HtmlMark,
    ) -> Result<Range<usize>, HtmlEditError> {
        let run = self.runs.get(index).ok_or(HtmlEditError::MissingRun)?;
        let active = mark_is_active(&run.style, &mark);
        let style = run.style.clone();
        let (source_range, prefix, selected, suffix) =
            self.selection_source(index, range.clone())?;
        // Validate requested values even when cancelling an existing value.
        mark_markup(mark.clone())?;
        if range.is_empty() {
            return Ok(source_range.start..source_range.start);
        }
        if !active {
            return self.wrap_text_range_anchor(index, range, mark);
        }
        let ancestors = &self.run_ancestors[index];
        let original_close = ancestors
            .iter()
            .rev()
            .map(|(tag, _)| format!("</{tag}>"))
            .collect::<String>();
        let original_open = ancestors
            .iter()
            .map(|(_, markup)| markup.as_str())
            .collect::<String>();
        let selected_ancestors = ancestors
            .iter()
            .map(|(_, markup)| neutralize_ancestor_markup(markup, &mark))
            .collect::<Vec<_>>();
        let selected_open = selected_ancestors
            .iter()
            .map(|(_, markup)| markup.as_str())
            .collect::<String>();
        let selected_close = selected_ancestors
            .iter()
            .rev()
            .map(|(tag, _)| format!("</{tag}>"))
            .collect::<String>();
        let reset = mark_reset_css(&style, &mark, ancestors);
        let reset_open = if reset.is_empty() {
            "<span>".to_string()
        } else {
            format!("<span style=\"{}\">", escape_html_attribute(&reset))
        };
        let prefix_escaped = escape_html_text(&prefix);
        let start = source_range.start
            + prefix_escaped.len()
            + original_close.len()
            + selected_open.len()
            + reset_open.len();
        let anchor = start..start + selected.len();
        self.patch(source_range, &format!("{prefix_escaped}{original_close}{selected_open}{reset_open}{selected}</span>{selected_close}{original_open}{}", escape_html_text(&suffix)));
        Ok(anchor)
    }

    /// Native empty-document input and "new paragraph" use the same text span
    /// anchoring as existing runs. No source outside the insertion is changed.
    /// The insertion precedes the real body/html closing tag, never one inside
    /// script/comment text. If the source ends in an unclosed hidden element,
    /// insert before it so new text cannot accidentally become script or CSS.
    pub fn append_paragraph(&mut self, text: &str) -> Range<usize> {
        if self.runs.iter().all(|run| {
            run.text
                .trim_matches([' ', '\t', '\r', '\n', '\u{c}'])
                .is_empty()
        }) && self.images.is_empty()
            && self.blocks.len() <= 1
            && !self.blocks.iter().any(|block| block.code)
        {
            if let Some(slot) = empty_paragraph_slot(&self.source) {
                let escaped = escape_html_text(text);
                let anchor = slot.start..slot.start + escaped.len();
                self.patch(slot, &escaped);
                self.empty_text_anchor = anchor.is_empty().then_some(anchor.start);
                return anchor;
            }
        }
        let at = paragraph_insertion_point(&self.source);
        let escaped = escape_html_text(text);
        let anchor = at + 3..at + 3 + escaped.len();
        self.patch(at..at, &format!("<p>{escaped}</p>"));
        self.empty_text_anchor = anchor.is_empty().then_some(anchor.start);
        anchor
    }

    pub fn clear_text_format(&mut self, index: usize) -> Result<(), HtmlEditError> {
        let length = self
            .runs
            .get(index)
            .ok_or(HtmlEditError::MissingRun)?
            .text
            .len();
        self.clear_text_format_range(index, 0..length)
    }

    /// Isolate the selection from inline ancestor tags instead of deleting a
    /// parent's markup (which would also clear formatting on adjacent runs).
    /// The block's structural formatting is retained. Unknown inline ancestors
    /// are closed/reopened around the isolated selection, preserving their bytes.
    pub fn clear_text_format_range(
        &mut self,
        index: usize,
        range: Range<usize>,
    ) -> Result<(), HtmlEditError> {
        self.clear_text_format_range_anchor(index, range)
            .map(|_| ())
    }

    /// Return the selected text's exact post-clear source span. Ancestor closing
    /// tags and the reset wrapper are accounted for before the selected bytes.
    pub fn clear_text_format_range_anchor(
        &mut self,
        index: usize,
        range: Range<usize>,
    ) -> Result<Range<usize>, HtmlEditError> {
        let (source_range, prefix, selected, suffix) = self.selection_source(index, range)?;
        let ancestors = &self.run_ancestors[index];
        let close = ancestors
            .iter()
            .rev()
            .map(|(tag, _)| format!("</{tag}>"))
            .collect::<String>();
        let open = ancestors
            .iter()
            .map(|(_, markup)| markup.as_str())
            .collect::<String>();
        let reset = "<span style=\"font-weight:normal;font-style:normal;text-decoration:none;color:initial;background-color:transparent;vertical-align:baseline;font-family:initial\">";
        let prefix_escaped = escape_html_text(&prefix);
        let start = source_range.start + prefix_escaped.len() + close.len() + reset.len();
        let anchor = start..start + selected.len();
        self.patch(
            source_range,
            &format!(
                "{}{close}{reset}{selected}</span>{open}{}",
                prefix_escaped,
                escape_html_text(&suffix)
            ),
        );
        self.empty_text_anchor = anchor.is_empty().then_some(anchor.start);
        Ok(anchor)
    }

    /// Quill removeFormat on a nonempty selection clears inline marks and all
    /// structural formats on the selected line. Work on a clone until every
    /// transformation succeeds; malformed source never produces a partial clear.
    /// Structural edits preserve text run order, so the selected run's ordinal
    /// updates the anchor without ambiguous matching of duplicated text.
    pub fn clear_selection_format_anchor(
        &mut self,
        index: usize,
        range: Range<usize>,
    ) -> Result<Range<usize>, HtmlEditError> {
        if range.is_empty() {
            let (source, _, _, _) = self.selection_source(index, range)?;
            return Ok(source.start..source.start);
        }
        let mut candidate = self.clone();
        let anchor = candidate.clear_text_format_range_anchor(index, range)?;
        let ordinal = candidate
            .runs
            .iter()
            .position(|run| run.source_range == anchor)
            .ok_or(HtmlEditError::MissingRun)?;
        for _ in 0..8 {
            let block_index = candidate
                .runs
                .get(ordinal)
                .ok_or(HtmlEditError::MissingRun)?
                .block_index;
            let block = &candidate.blocks[block_index];
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
            let Some(next) = next else {
                let result = candidate.runs[ordinal].source_range.clone();
                *self = candidate;
                return Ok(result);
            };
            candidate.cancel_block_format(block_index, next)?;
        }
        Err(HtmlEditError::UnsupportedBlockStructure)
    }

    /// Replace only src's value; alt, dimensions, vendor attributes, comments
    /// and the original tag spelling/order remain untouched.
    pub fn replace_image_reference(
        &mut self,
        index: usize,
        src: &str,
    ) -> Result<(), HtmlEditError> {
        validate_reference(src, true)?;
        let image = self.images.get(index).ok_or(HtmlEditError::MissingImage)?;
        let (range, replacement) = match &image.src_attribute {
            Some(attr) => match &attr.value_range {
                Some(range) if attr.quote.is_some() => (range.clone(), escape_html_attribute(src)),
                Some(range) => (range.clone(), format!("\"{}\"", escape_html_attribute(src))),
                None => (
                    attr.source_range.clone(),
                    format!(
                        "{}=\"{}\"",
                        &self.source[attr.source_range.clone()],
                        escape_html_attribute(src)
                    ),
                ),
            },
            None => (
                image.insert_at..image.insert_at,
                format!(" src=\"{}\"", escape_html_attribute(src)),
            ),
        };
        self.patch(range, &replacement);
        Ok(())
    }

    pub fn remove_image(&mut self, index: usize) -> Result<(), HtmlEditError> {
        let range = self
            .images
            .get(index)
            .ok_or(HtmlEditError::MissingImage)?
            .source_range
            .clone();
        self.patch(range, "");
        Ok(())
    }

    pub fn insert_image_after_run(
        &mut self,
        index: usize,
        src: &str,
        alt: &str,
    ) -> Result<(), HtmlEditError> {
        validate_reference(src, true)?;
        let end = self
            .runs
            .get(index)
            .ok_or(HtmlEditError::MissingRun)?
            .source_range
            .end;
        self.patch(
            end..end,
            &format!(
                "<img src=\"{}\" alt=\"{}\">",
                escape_html_attribute(src),
                escape_html_attribute(alt)
            ),
        );
        Ok(())
    }

    fn patch(&mut self, range: Range<usize>, replacement: &str) {
        self.source.replace_range(range, replacement);
        self.runs.clear();
        self.blocks.clear();
        self.images.clear();
        self.opaque.clear();
        self.segments.clear();
        self.run_ancestors.clear();
        self.block_ancestors.clear();
        self.block_ancestor_starts.clear();
        self.empty_text_anchor = None;
        self.break_sources.clear();
        self.project();
    }

    fn selection_source(
        &self,
        index: usize,
        range: Range<usize>,
    ) -> Result<(Range<usize>, String, String, String), HtmlEditError> {
        let run = self.runs.get(index).ok_or(HtmlEditError::MissingRun)?;
        if range.start > range.end
            || range.end > run.text.len()
            || !run.text.is_char_boundary(range.start)
            || !run.text.is_char_boundary(range.end)
        {
            return Err(HtmlEditError::InvalidTextRange);
        }
        let segments = &self.segments[index];
        // Exact source boundaries are retained, including an entity's original
        // spelling. Interior entity boundaries need a decoded prefix/suffix.
        let start_segment = segments.iter().find(|part| range.start < part.text.end);
        let end_segment = segments
            .iter()
            .rev()
            .find(|part| range.end > part.text.start);
        let start = start_segment.map_or(run.source_range.end, |part| {
            part.source.start
                + if part.direct {
                    range.start - part.text.start
                } else {
                    0
                }
        });
        let prefix = start_segment.map_or("", |part| {
            if part.direct {
                ""
            } else {
                &run.text[part.text.start..range.start]
            }
        });
        let (end, suffix) = if range.is_empty() {
            if let Some(part) =
                start_segment.filter(|part| !part.direct && range.start > part.text.start)
            {
                (part.source.end, &run.text[range.end..part.text.end])
            } else {
                (start, "")
            }
        } else {
            end_segment.map_or((start, ""), |part| {
                if part.direct {
                    (part.source.start + range.end - part.text.start, "")
                } else {
                    (part.source.end, &run.text[range.end..part.text.end])
                }
            })
        };
        let selected = if prefix.is_empty() && suffix.is_empty() {
            self.source[start..end].to_string()
        } else {
            escape_html_text(&run.text[range.clone()])
        };
        Ok((start..end, prefix.into(), selected, suffix.into()))
    }

    fn ensure_block(
        &mut self,
        stack: &mut [SourceFrame],
        active: &mut Option<usize>,
        range: &Range<usize>,
    ) -> usize {
        if let Some(index) = active {
            self.blocks[*index].source_range.end = range.end;
            return *index;
        }
        let owner = stack.iter().rposition(|frame| frame.block_owner);
        let format = stack
            .last()
            .map(|frame| frame.format.clone())
            .unwrap_or_default();
        let index = self.blocks.len();
        self.blocks.push(HtmlBlock {
            kind: format.kind,
            list: format.list,
            indent: format.indent,
            quote: format.quote,
            code: format.code,
            elements: Vec::new(),
            source_range: owner.map_or(range.start, |i| stack[i].source_start)..range.end,
        });
        let owner_index = owner.unwrap_or(stack.len());
        self.block_ancestor_starts.push(
            stack[..owner_index]
                .iter()
                .map(|frame| frame.source_start)
                .collect(),
        );
        self.block_ancestors.push(
            stack[..owner_index]
                .iter()
                .filter_map(|frame| {
                    source_tag(&self.source, frame.source_start)
                        .map(|tag| (frame.tag.clone(), self.source[tag.range].to_string()))
                })
                .collect(),
        );
        if let Some(owner) = owner {
            stack[owner].blocks.push(index);
        }
        *active = Some(index);
        index
    }

    fn add_text(
        &mut self,
        range: Range<usize>,
        stack: &mut [SourceFrame],
        active: &mut Option<usize>,
    ) {
        if range.is_empty() || stack.last().is_some_and(|frame| frame.suppressed) {
            return;
        }
        let (text, segments) = decode_source_text(&self.source[range.clone()], range.start);
        if text.is_empty() {
            return;
        }
        // Source indentation outside an explicit block does not create blank
        // visual paragraphs. Whitespace inside p/div/etc is editable payload:
        // the first typed space and replacing an entire run with a newline must
        // still produce a run. NBSP/ZWJ are meaningful even outside a block.
        if active.is_none()
            && text
                .chars()
                .all(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n' | '\u{c}'))
            && !stack.last().is_some_and(|f| f.format.code)
            && !stack.iter().any(|frame| frame.block_owner)
        {
            return;
        }
        let block_index = self.ensure_block(stack, active, &range);
        let index = self.runs.len();
        self.runs.push(HtmlRun {
            text,
            source_range: range,
            block_index,
            style: stack
                .last()
                .map(|frame| frame.style.clone())
                .unwrap_or_default(),
        });
        self.segments.push(segments);
        let first_inline = stack
            .iter()
            .rposition(|frame| frame.block_owner)
            .map_or(0, |owner| owner + 1);
        self.run_ancestors.push(
            stack[first_inline..]
                .iter()
                .filter_map(|frame| {
                    source_tag(&self.source, frame.source_start)
                        .map(|tag| (frame.tag.clone(), self.source[tag.range].to_string()))
                })
                .collect(),
        );
        self.blocks[block_index]
            .elements
            .push(HtmlElement::Text(index));
    }

    fn project(&mut self) {
        let mut stack: Vec<SourceFrame> = Vec::new();
        let mut active = None;
        // Keep the original envelope in source and keep every range absolute;
        // its protocol header is not visible or editable document text.
        let mut cursor = if cliphost_core::cf_html::looks_like_complete_format(&self.source) {
            self.source.find('<').unwrap_or(self.source.len()).min(
                cliphost_core::cf_html::fragment_range(&self.source)
                    .map(|(start, _)| start).unwrap_or(self.source.len()))
        } else { 0 };
        let mut text_start = cursor;
        while cursor < self.source.len() {
            if self.source.as_bytes()[cursor] != b'<' {
                cursor += 1;
                continue;
            }
            if self.source[cursor..].starts_with("<!--") {
                self.add_text(text_start..cursor, &mut stack, &mut active);
                cursor = self.source[cursor + 4..]
                    .find("-->")
                    .map_or(self.source.len(), |end| cursor + 4 + end + 3);
                text_start = cursor;
                continue;
            }
            if self.source[cursor..].starts_with("<!") || self.source[cursor..].starts_with("<?") {
                self.add_text(text_start..cursor, &mut stack, &mut active);
                cursor = tag_end(&self.source, cursor).unwrap_or(self.source.len());
                text_start = cursor;
                continue;
            }
            let Some(tag) = source_tag(&self.source, cursor) else {
                cursor += 1;
                continue;
            };
            self.add_text(text_start..cursor, &mut stack, &mut active);
            cursor = tag.range.end;
            text_start = cursor;
            if tag.closing {
                if let Some(index) = stack.iter().rposition(|frame| frame.tag == tag.name) {
                    for frame in stack.drain(index..) {
                        for block in frame.blocks {
                            self.blocks[block].source_range.end = cursor;
                        }
                        if frame.block_owner {
                            active = None;
                        }
                    }
                }
                continue;
            }
            let suppressed =
                stack.last().is_some_and(|frame| frame.suppressed) || suppress_tag(&tag.name);
            if !known_tag(&tag.name)
                || suppress_tag(&tag.name)
                || matches!(tag.name.as_str(), "table" | "td" | "th" | "style")
            {
                self.opaque.push(HtmlOpaque {
                    tag: tag.name.clone(),
                    source_range: tag.range.clone(),
                });
            }
            // Raw-text elements cannot be lexed as normal tags: a string such
            // as "<img src=x>" inside a script must never become a visible image.
            if matches!(
                tag.name.as_str(),
                "script" | "style" | "textarea" | "title" | "xmp" | "plaintext"
            ) {
                let end = raw_text_end(&self.source, cursor, &tag.name);
                if let Some(opaque) = self
                    .opaque
                    .last_mut()
                    .filter(|node| node.source_range.start == tag.range.start)
                {
                    opaque.source_range.end = end;
                } else {
                    self.opaque.push(HtmlOpaque {
                        tag: tag.name,
                        source_range: tag.range.start..end,
                    });
                }
                cursor = end;
                text_start = cursor;
                continue;
            }
            if suppressed {
                if !tag.self_closing && !void_tag(&tag.name) {
                    stack.push(SourceFrame {
                        tag: tag.name,
                        style: HtmlStyle::default(),
                        format: BlockFormat::default(),
                        suppressed: true,
                        block_owner: false,
                        source_start: tag.range.start,
                        blocks: Vec::new(),
                    });
                }
                continue;
            }
            if tag.name == "br" || tag.name == "hr" {
                if tag.name == "hr" {
                    active = None;
                }
                let block_index = self.ensure_block(&mut stack, &mut active, &tag.range);
                self.break_sources.push((
                    block_index,
                    self.blocks[block_index].elements.len(),
                    tag.range.clone(),
                ));
                if tag.name == "hr" {
                    self.blocks[block_index].kind = HtmlBlockKind::HorizontalRule;
                    self.blocks[block_index]
                        .elements
                        .push(HtmlElement::HorizontalRule);
                    active = None;
                } else {
                    self.blocks[block_index].elements.push(HtmlElement::Break);
                }
                continue;
            }
            if tag.name == "img" {
                let block_index = self.ensure_block(&mut stack, &mut active, &tag.range);
                let index = self.images.len();
                let attr = |name: &str| tag.attrs.iter().find(|attr| attr.name == name);
                self.images.push(HtmlImage {
                    src: attr("src")
                        .map(|attr| attr.value.clone())
                        .unwrap_or_default(),
                    alt: attr("alt")
                        .map(|attr| attr.value.clone())
                        .unwrap_or_default(),
                    title: attr("title").map(|attr| attr.value.clone()),
                    width: attr("width").and_then(|attr| attr.value.parse().ok()),
                    height: attr("height").and_then(|attr| attr.value.parse().ok()),
                    display_style: attr("style").map(|attr| attr.value.clone()),
                    source_range: tag.range,
                    block_index,
                    src_attribute: attr("src").cloned(),
                    insert_at: tag.insert_at,
                });
                self.blocks[block_index]
                    .elements
                    .push(HtmlElement::Image(index));
                continue;
            }
            let mut style = stack
                .last()
                .map(|frame| frame.style.clone())
                .unwrap_or_default();
            let mut format = stack
                .last()
                .map(|frame| frame.format.clone())
                .unwrap_or_default();
            apply_tag_style(&tag, &mut style, &mut format);
            let block_owner = is_block(&tag.name);
            if block_owner {
                active = None;
            }
            if !tag.self_closing && !void_tag(&tag.name) {
                stack.push(SourceFrame {
                    tag: tag.name,
                    style,
                    format,
                    suppressed: false,
                    block_owner,
                    source_start: tag.range.start,
                    blocks: Vec::new(),
                });
            }
        }
        self.add_text(text_start..self.source.len(), &mut stack, &mut active);
        for frame in stack {
            for block in frame.blocks {
                self.blocks[block].source_range.end = self.source.len();
            }
        }
    }
}

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn replace_owner_tag_names(
    original: &str,
    origin: usize,
    open: &SourceTag,
    close: &SourceTag,
    replacement: &str,
) -> String {
    let mut output = original.to_string();
    let close_start = close.range.start - origin + 2;
    output.replace_range(close_start..close_start + close.name.len(), replacement);
    let open_start = open.range.start - origin + 1;
    output.replace_range(open_start..open_start + open.name.len(), replacement);
    output
}

fn attribute_patch(tag: &SourceTag, name: &str, value: &str) -> (Range<usize>, String) {
    let escaped = escape_html_attribute(value);
    match tag.attrs.iter().find(|attr| attr.name == name) {
        Some(attr) => match &attr.value_range {
            Some(range) if attr.quote.is_some() => (range.clone(), escaped),
            Some(range) => (range.clone(), format!("\"{escaped}\"")),
            None => (attr.source_range.clone(), format!("{name}=\"{escaped}\"")),
        },
        None => (
            tag.insert_at..tag.insert_at,
            format!(" {name}=\"{escaped}\""),
        ),
    }
}

fn block_markup_has_format(markup: &str, mark: HtmlBlockMark) -> bool {
    let Some(tag) = source_tag(markup, 0) else {
        return false;
    };
    match mark {
        HtmlBlockMark::Header(_) => {
            matches!(tag.name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
        }
        HtmlBlockMark::List(_) => {
            matches!(tag.name.as_str(), "ol" | "ul" | "li")
                || tag.attrs.iter().any(|attr| {
                    attr.name == "data-list" && matches!(attr.value.as_str(), "ordered" | "bullet")
                })
        }
        HtmlBlockMark::Blockquote => tag.name == "blockquote",
        HtmlBlockMark::CodeBlock => {
            tag.name == "pre"
                || tag.attrs.iter().any(|attr| {
                    attr.name == "class"
                        && attr
                            .value
                            .split_ascii_whitespace()
                            .any(|class| class == "ql-code-block")
                })
        }
        HtmlBlockMark::Indent(_) => tag.attrs.iter().any(|attr| {
            attr.name == "class"
                && attr
                    .value
                    .split_ascii_whitespace()
                    .any(|class| class.starts_with("ql-indent-"))
        }),
        HtmlBlockMark::Paragraph => false,
    }
}

fn neutralize_block_markup(markup: &str, mark: HtmlBlockMark) -> (String, String) {
    let Some(tag) = source_tag(markup, 0) else {
        return ("div".into(), markup.into());
    };
    let rename = match mark {
        HtmlBlockMark::Header(_) => {
            matches!(tag.name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
        }
        HtmlBlockMark::List(_) => matches!(tag.name.as_str(), "ol" | "ul" | "li"),
        HtmlBlockMark::Blockquote => tag.name == "blockquote",
        HtmlBlockMark::CodeBlock => tag.name == "pre",
        _ => false,
    };
    let mut output = markup.to_string();
    if matches!(mark, HtmlBlockMark::List(_)) {
        let mut ranges = tag
            .attrs
            .iter()
            .filter(|attr| {
                attr.name == "data-list" && matches!(attr.value.as_str(), "ordered" | "bullet")
            })
            .map(|attr| attr.source_range.clone())
            .collect::<Vec<_>>();
        ranges.sort_by(|a, b| b.start.cmp(&a.start));
        for range in ranges {
            output.replace_range(range, "");
        }
    }
    if matches!(mark, HtmlBlockMark::CodeBlock | HtmlBlockMark::Indent(_)) {
        if let Some(class) = tag.attrs.iter().find(|attr| attr.name == "class") {
            let remaining = class
                .value
                .split_ascii_whitespace()
                .filter(|class| {
                    if matches!(mark, HtmlBlockMark::CodeBlock) {
                        *class != "ql-code-block"
                    } else {
                        !class.starts_with("ql-indent-")
                    }
                })
                .collect::<Vec<_>>();
            if remaining.len() != class.value.split_ascii_whitespace().count() {
                let (range, value) = attribute_patch(&tag, "class", &remaining.join(" "));
                output.replace_range(range, &value);
            }
        }
    }
    let name = if rename {
        output.replace_range(1..1 + tag.name.len(), "div");
        "div".to_string()
    } else {
        tag.name
    };
    (name, output)
}

fn renamed_closing_markup(markup: &str, old_name: &str, new_name: &str) -> String {
    if old_name == new_name {
        return markup.to_string();
    }
    let mut output = markup.to_string();
    output.replace_range(2..2 + old_name.len(), new_name);
    output
}

/// Strict lexical pairing is only used before structural source edits. Unknown
/// tag spelling/attrs survive, but crossed/unclosed blocks are rejected instead
/// of inventing DOM repairs that could rewrite unrelated clipboard payload.
fn matching_close_tag(source: &str, start: usize) -> Option<SourceTag> {
    let opening = source_tag(source, start)?;
    if opening.closing || opening.self_closing || void_tag(&opening.name) {
        return None;
    }
    let mut cursor = opening.range.end;
    let mut stack = vec![opening.name];
    while cursor < source.len() {
        let offset = source[cursor..].find('<')?;
        cursor += offset;
        if source[cursor..].starts_with("<!--") {
            cursor += 4 + source[cursor + 4..].find("-->")? + 3;
            continue;
        }
        if source[cursor..].starts_with("<!") || source[cursor..].starts_with("<?") {
            cursor = tag_end(source, cursor)?;
            continue;
        }
        let Some(tag) = source_tag(source, cursor) else {
            cursor += 1;
            continue;
        };
        cursor = tag.range.end;
        if tag.closing {
            if void_tag(&tag.name) {
                continue;
            }
            if stack.last().is_some_and(|name| *name == tag.name) {
                stack.pop();
                if stack.is_empty() {
                    return Some(tag);
                }
            } else if stack.iter().any(|name| *name == tag.name) {
                return None;
            }
        } else if matches!(
            tag.name.as_str(),
            "script" | "style" | "textarea" | "title" | "xmp" | "plaintext"
        ) {
            let end = raw_text_end(source, cursor, &tag.name);
            let closed = source[cursor..end]
                .rfind("</")
                .and_then(|offset| source_tag(source, cursor + offset))
                .is_some_and(|close| {
                    close.closing && close.name == tag.name && close.range.end == end
                });
            if !closed {
                return None;
            }
            cursor = end;
        } else if !tag.self_closing && !void_tag(&tag.name) {
            stack.push(tag.name);
        }
    }
    None
}

fn mark_is_active(style: &HtmlStyle, mark: &HtmlMark) -> bool {
    let color_equal = |current: &Option<String>, requested: &str| {
        let normalize = |value: &str| {
            value
                .chars()
                .filter(|ch| !ch.is_ascii_whitespace())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        current
            .as_ref()
            .is_some_and(|value| normalize(value) == normalize(requested))
    };
    match mark {
        HtmlMark::Bold => style.bold,
        HtmlMark::Italic => style.italic,
        HtmlMark::Underline => style.underline,
        HtmlMark::Strike => style.strike,
        HtmlMark::Color(value) => color_equal(&style.color, value),
        HtmlMark::Background(value) => color_equal(&style.background, value),
        HtmlMark::Subscript => style.subscript,
        HtmlMark::Superscript => style.superscript,
        HtmlMark::Link(value) => style.link.as_deref() == Some(value.as_str()),
        HtmlMark::Code => style.code,
    }
}

/// Work on cloned opening-tag source only, never on the original ancestor.
/// Unknown attrs and unrelated CSS declarations remain present, including on
/// semantic tags that need to become span to cancel their one formatting mark.
fn neutralize_ancestor_markup(markup: &str, mark: &HtmlMark) -> (String, String) {
    let Some(tag) = source_tag(markup, 0) else {
        return ("span".into(), markup.into());
    };
    let neutral_tag = match mark {
        HtmlMark::Bold => matches!(tag.name.as_str(), "b" | "strong"),
        HtmlMark::Italic => matches!(tag.name.as_str(), "i" | "em"),
        HtmlMark::Underline => tag.name == "u",
        HtmlMark::Strike => matches!(tag.name.as_str(), "s" | "strike" | "del"),
        HtmlMark::Subscript => tag.name == "sub",
        HtmlMark::Superscript => tag.name == "sup",
        HtmlMark::Code => tag.name == "code",
        _ => false,
    };
    let mut edits = Vec::new();
    for attr in &tag.attrs {
        if matches!(mark, HtmlMark::Link(_)) && attr.name == "href" {
            edits.push((attr.source_range.clone(), String::new()));
        } else if attr.name == "style" {
            let new_value = neutralize_css(&attr.value, mark);
            if new_value != attr.value {
                if let Some(range) = &attr.value_range {
                    let value = escape_html_attribute(&new_value);
                    edits.push((
                        range.clone(),
                        if attr.quote.is_some() {
                            value
                        } else {
                            format!("\"{value}\"")
                        },
                    ));
                }
            }
        } else if matches!(mark, HtmlMark::Color(_)) && tag.name == "font" && attr.name == "color" {
            edits.push((attr.source_range.clone(), String::new()));
        }
    }
    edits.sort_by(|(a, _), (b, _)| b.start.cmp(&a.start));
    let mut output = markup.to_string();
    for (range, value) in edits {
        output.replace_range(range, &value);
    }
    let name = if neutral_tag {
        output.replace_range(1..1 + tag.name.len(), "span");
        "span".to_string()
    } else {
        tag.name
    };
    (name, output)
}

fn image_css_dimension(value: &str, maximum: bool, height: bool) -> Option<HtmlImageDimension> {
    let lower = value.trim().to_ascii_lowercase();
    if (!maximum && lower == "auto") || (maximum && lower == "none") {
        return Some(HtmlImageDimension::Auto);
    }
    if let Some(number) = lower.strip_suffix("px") {
        return nonnegative_css_number(number).map(HtmlImageDimension::Pixels);
    }
    if let Some(number) = lower.strip_suffix('%') {
        return nonnegative_css_number(number).map(|number| {
            if height {
                HtmlImageDimension::Unresolved
            } else {
                HtmlImageDimension::Percent(number)
            }
        });
    }
    if let Some(number) = nonnegative_css_number(&lower) {
        return (number == 0.).then_some(HtmlImageDimension::Pixels(0.));
    }
    // Classify context-dependent lengths without converting or executing them.
    // Unknown identifiers/units and negative lengths are invalid declarations;
    // they must not overwrite an earlier valid value or the source attr fallback.
    for unit in [
        "em", "rem", "ex", "rex", "ch", "rch", "cap", "rcap", "ic", "ric", "lh", "rlh", "cm", "mm",
        "q", "in", "pt", "pc", "vw", "vh", "vi", "vb", "vmin", "vmax", "svw", "svh", "svi", "svb",
        "svmin", "svmax", "lvw", "lvh", "lvi", "lvb", "lvmin", "lvmax", "dvw", "dvh", "dvi", "dvb",
        "dvmin", "dvmax", "cqw", "cqh", "cqi", "cqb", "cqmin", "cqmax",
    ] {
        if lower
            .strip_suffix(unit)
            .and_then(nonnegative_css_number)
            .is_some()
        {
            return Some(HtmlImageDimension::Unresolved);
        }
    }
    if matches!(
        lower.as_str(),
        "inherit"
            | "initial"
            | "unset"
            | "revert"
            | "revert-layer"
            | "min-content"
            | "max-content"
            | "fit-content"
            | "stretch"
            | "contain"
    ) || ([
        "calc(",
        "var(",
        "env(",
        "min(",
        "max(",
        "clamp(",
        "fit-content(",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        && lower.ends_with(')'))
    {
        return Some(HtmlImageDimension::Unresolved);
    }
    None
}

fn nonnegative_css_number(value: &str) -> Option<f32> {
    let bytes = value.as_bytes();
    let mut cursor = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    let integer_digits = cursor > integer_start;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == fraction_start {
            return None;
        }
    } else if !integer_digits {
        return None;
    }
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        cursor += 1;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        let exponent_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == exponent_start {
            return None;
        }
    }
    if cursor != bytes.len() {
        return None;
    }
    value
        .parse::<f32>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.)
}

fn css_value_without_priority(value: &str) -> &str {
    let value = value.trim();
    if value
        .get(value.len().saturating_sub(10)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case("!important"))
    {
        value[..value.len() - 10].trim_end()
    } else {
        value
    }
}

fn neutralize_css(css: &str, mark: &HtmlMark) -> String {
    css.split(';')
        .map(|declaration| {
            let Some((name, value)) = declaration.split_once(':') else {
                return declaration.to_string();
            };
            let clean_value = css_value_without_priority(value);
            let lower = clean_value.to_ascii_lowercase();
            let property = name.trim().to_ascii_lowercase();
            let replacement = match mark {
                HtmlMark::Bold if property == "font-weight" => Some("normal".to_string()),
                HtmlMark::Italic if property == "font-style" => Some("normal".to_string()),
                HtmlMark::Color(_) if property == "color" => Some("initial".to_string()),
                HtmlMark::Background(_)
                    if property == "background-color"
                        || (property == "background" && inert_color(clean_value)) =>
                {
                    Some("transparent".to_string())
                }
                HtmlMark::Subscript if property == "vertical-align" && lower == "sub" => {
                    Some("baseline".to_string())
                }
                HtmlMark::Superscript if property == "vertical-align" && lower == "super" => {
                    Some("baseline".to_string())
                }
                HtmlMark::Underline | HtmlMark::Strike
                    if matches!(
                        property.as_str(),
                        "text-decoration" | "text-decoration-line"
                    ) =>
                {
                    let target = if matches!(mark, HtmlMark::Underline) {
                        "underline"
                    } else {
                        "line-through"
                    };
                    let remaining = clean_value
                        .split_ascii_whitespace()
                        .filter(|value| !value.eq_ignore_ascii_case(target))
                        .collect::<Vec<_>>();
                    if remaining.len() == clean_value.split_ascii_whitespace().count() {
                        None
                    } else {
                        Some(if remaining.is_empty() {
                            "none".to_string()
                        } else {
                            remaining.join(" ")
                        })
                    }
                }
                _ => None,
            };
            if let Some(replacement) = replacement {
                let priority = if clean_value != value.trim() {
                    " !important"
                } else {
                    ""
                };
                format!("{name}:{replacement}{priority}")
            } else {
                declaration.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn mark_reset_css(style: &HtmlStyle, mark: &HtmlMark, ancestors: &[(String, String)]) -> String {
    match mark {
        HtmlMark::Bold => "font-weight:normal".into(),
        HtmlMark::Italic => "font-style:normal".into(),
        HtmlMark::Color(_) => "color:initial".into(),
        HtmlMark::Background(_) => "background-color:transparent".into(),
        HtmlMark::Subscript | HtmlMark::Superscript => "vertical-align:baseline".into(),
        HtmlMark::Code => "font-family:initial".into(),
        HtmlMark::Link(_) => String::new(),
        HtmlMark::Underline | HtmlMark::Strike => {
            let mut lines = Vec::new();
            if !matches!(mark, HtmlMark::Underline) && style.underline {
                lines.push("underline");
            }
            if !matches!(mark, HtmlMark::Strike) && style.strike {
                lines.push("line-through");
            }
            // Keep the supported non-toolbar decoration if present; cancelling
            // underline must not silently remove an existing overline.
            if ancestors
                .iter()
                .filter_map(|(_, markup)| source_tag(markup, 0))
                .flat_map(|tag| tag.attrs)
                .filter(|attr| attr.name == "style")
                .any(|attr| {
                    attr.value
                        .split(';')
                        .filter_map(|s| s.split_once(':'))
                        .any(|(key, value)| {
                            matches!(
                                key.trim().to_ascii_lowercase().as_str(),
                                "text-decoration" | "text-decoration-line"
                            ) && value
                                .split_ascii_whitespace()
                                .any(|word| word.eq_ignore_ascii_case("overline"))
                        })
                })
            {
                lines.push("overline");
            }
            format!(
                "text-decoration:none;text-decoration-line:{}",
                if lines.is_empty() {
                    "none".to_string()
                } else {
                    lines.join(" ")
                }
            )
        }
    }
}

fn paragraph_insertion_point(source: &str) -> usize {
    let mut cursor = 0;
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut body_close = None;
    let mut html_close = None;
    while cursor < source.len() {
        let Some(relative) = source[cursor..].find('<') else {
            break;
        };
        cursor += relative;
        if source[cursor..].starts_with("<!--") {
            let Some(end) = source[cursor + 4..].find("-->") else {
                return stack
                    .iter()
                    .find(|(name, _)| suppress_tag(name))
                    .map_or(cursor, |(_, at)| *at);
            };
            cursor += 4 + end + 3;
            continue;
        }
        if source[cursor..].starts_with("<!") || source[cursor..].starts_with("<?") {
            cursor = tag_end(source, cursor).unwrap_or(source.len());
            continue;
        }
        let Some(tag) = source_tag(source, cursor) else {
            cursor += 1;
            continue;
        };
        cursor = tag.range.end;
        if tag.closing {
            if !stack.iter().any(|(name, _)| suppress_tag(name)) {
                if tag.name == "body" {
                    body_close = Some(tag.range.start);
                }
                if tag.name == "html" {
                    html_close = Some(tag.range.start);
                }
            }
            if let Some(index) = stack.iter().rposition(|(name, _)| *name == tag.name) {
                stack.truncate(index);
            }
        } else if matches!(
            tag.name.as_str(),
            "script" | "style" | "textarea" | "title" | "xmp" | "plaintext"
        ) {
            let end = raw_text_end(source, cursor, &tag.name);
            let has_close = source[cursor..end]
                .rfind("</")
                .and_then(|at| source_tag(source, cursor + at))
                .is_some_and(|close| {
                    close.closing && close.name == tag.name && close.range.end == end
                });
            if !has_close {
                return stack
                    .iter()
                    .find(|(name, _)| suppress_tag(name))
                    .map_or(tag.range.start, |(_, at)| *at);
            }
            cursor = end;
        } else if !tag.self_closing && !void_tag(&tag.name) {
            stack.push((tag.name, tag.range.start));
        }
    }
    stack
        .iter()
        .find(|(name, _)| suppress_tag(name))
        .map(|(_, at)| *at)
        .or(body_close)
        .or(html_close)
        .unwrap_or(source.len())
}

/// Only the one unambiguous Quill empty paragraph is a reusable input slot.
/// Comments, vendor br attributes, multiple empty paragraphs and hidden content
/// are retained and use ordinary append instead of guessing which to replace.
fn empty_paragraph_slot(source: &str) -> Option<Range<usize>> {
    let mut slots = empty_paragraph_source_ranges(source);
    if slots.len() == 1 {
        slots.pop().map(|(_, body)| body)
    } else {
        None
    }
}

fn inert_paragraph_gap(source: &str) -> bool {
    let mut remaining = source;
    loop {
        remaining = remaining.trim_start_matches([' ', '\t', '\r', '\n', '\u{c}']);
        if remaining.is_empty() {
            return true;
        }
        if !remaining.starts_with("<!--") {
            return false;
        }
        let Some(end) = remaining[4..].find("-->") else {
            return false;
        };
        remaining = &remaining[4 + end + 3..];
    }
}

fn paragraph_gap_whitespace(source: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = range.start;
    while cursor < range.end {
        let length = source[cursor..range.end].len()
            - source[cursor..range.end]
                .trim_start_matches([' ', '\t', '\r', '\n', '\u{c}'])
                .len();
        if length > 0 {
            ranges.push(cursor..cursor + length);
            cursor += length;
        }
        if cursor >= range.end {
            break;
        }
        if source[cursor..].starts_with("<!--") {
            if let Some(end) = source[cursor + 4..range.end].find("-->") {
                cursor += 4 + end + 3;
                continue;
            }
        }
        break; // Caller has already validated the whole inert seam.
    }
    ranges
}

fn canvas_parent_starts_at(source: &str, at: usize) -> Result<Vec<usize>, HtmlEditError> {
    let mut cursor = 0;
    let mut path: Vec<(String, usize)> = Vec::new();
    while cursor < at {
        let Some(offset) = source[cursor..at].find('<') else {
            break;
        };
        cursor += offset;
        if source[cursor..].starts_with("<!--") {
            cursor += 4
                + source[cursor + 4..]
                    .find("-->")
                    .ok_or(HtmlEditError::UnsupportedBlockStructure)?
                + 3;
            continue;
        }
        if source[cursor..].starts_with("<!") || source[cursor..].starts_with("<?") {
            cursor = tag_end(source, cursor).ok_or(HtmlEditError::UnsupportedBlockStructure)?;
            continue;
        }
        let Some(tag) = source_tag(source, cursor) else {
            cursor += 1;
            continue;
        };
        cursor = tag.range.end;
        if cursor > at {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        if tag.closing {
            if path.last().is_some_and(|(name, _)| *name == tag.name) {
                path.pop();
            } else if path.iter().any(|(name, _)| *name == tag.name) {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
        } else if matches!(
            tag.name.as_str(),
            "script" | "style" | "textarea" | "title" | "xmp" | "plaintext"
        ) {
            cursor = raw_text_end(source, cursor, &tag.name);
            if cursor > at {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
        } else if !tag.self_closing && !void_tag(&tag.name) {
            path.push((tag.name, tag.range.start));
        }
    }
    Ok(path.into_iter().map(|(_, start)| start).collect())
}

fn canvas_inline_path(
    source: &str,
    start: usize,
    at: usize,
) -> Result<Vec<(String, String)>, HtmlEditError> {
    let mut cursor = start;
    let mut path: Vec<(String, String)> = Vec::new();
    while cursor < at {
        let Some(offset) = source[cursor..at].find('<') else {
            break;
        };
        cursor += offset;
        if source[cursor..].starts_with("<!--") {
            let end = source[cursor + 4..]
                .find("-->")
                .map(|offset| cursor + 4 + offset + 3)
                .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
            if end > at {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
            cursor = end;
            continue;
        }
        let Some(tag) = source_tag(source, cursor) else {
            cursor += 1;
            continue;
        };
        if tag.range.end > at || is_block(&tag.name) || suppress_tag(&tag.name) {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        cursor = tag.range.end;
        if tag.closing {
            if path.last().is_some_and(|(name, _)| *name == tag.name) {
                path.pop();
            } else if path.iter().any(|(name, _)| *name == tag.name) {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
        } else if !tag.self_closing && !void_tag(&tag.name) {
            path.push((tag.name, source[tag.range].to_string()));
        }
    }
    Ok(path)
}

fn apply_canvas_patches(
    source: &mut String,
    mut patches: Vec<(Range<usize>, String)>,
) -> Result<(), HtmlEditError> {
    patches.sort_by(|(a, _), (b, _)| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    let mut previous: Option<&Range<usize>> = None;
    for (range, _) in &patches {
        if range.start > range.end
            || range.end > source.len()
            || !source.is_char_boundary(range.start)
            || !source.is_char_boundary(range.end)
        {
            return Err(HtmlEditError::InvalidSourceRange);
        }
        if previous.is_some_and(|previous| range.start < previous.end) {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        previous = Some(range);
    }
    for (range, replacement) in patches.into_iter().rev() {
        source.replace_range(range, &replacement);
    }
    Ok(())
}

fn empty_paragraph_source_ranges(source: &str) -> Vec<(Range<usize>, Range<usize>)> {
    empty_line_source_ranges(source, false)
}

fn empty_line_source_ranges(source: &str, styled: bool) -> Vec<(Range<usize>, Range<usize>)> {
    let mut cursor = 0;
    let mut stack: Vec<String> = Vec::new();
    let mut slots = Vec::new();
    while cursor < source.len() {
        let Some(relative) = source[cursor..].find('<') else {
            break;
        };
        cursor += relative;
        if source[cursor..].starts_with("<!--") {
            cursor = source[cursor + 4..]
                .find("-->")
                .map_or(source.len(), |at| cursor + 4 + at + 3);
            continue;
        }
        if source[cursor..].starts_with("<!") || source[cursor..].starts_with("<?") {
            cursor = tag_end(source, cursor).unwrap_or(source.len());
            continue;
        }
        let Some(tag) = source_tag(source, cursor) else {
            cursor += 1;
            continue;
        };
        cursor = tag.range.end;
        if tag.closing {
            if let Some(index) = stack.iter().rposition(|name| *name == tag.name) {
                stack.truncate(index);
            }
            continue;
        }
        if matches!(
            tag.name.as_str(),
            "script" | "style" | "textarea" | "title" | "xmp" | "plaintext"
        ) {
            cursor = raw_text_end(source, cursor, &tag.name);
            continue;
        }
        if (tag.name == "p"
            || (styled
                && matches!(
                    tag.name.as_str(),
                    "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                )))
            && !tag.self_closing
            && !stack.iter().any(|tag| suppress_tag(tag))
        {
            let contents_start = cursor;
            let next = cursor + source[cursor..].len()
                - source[cursor..]
                    .trim_start_matches([' ', '\t', '\r', '\n', '\u{c}'])
                    .len();
            let mut next_tag = source_tag(source, next);
            if let Some(br) = next_tag
                .as_ref()
                .filter(|tag| !tag.closing && tag.name == "br" && tag.attrs.is_empty())
            {
                let after_br = br.range.end;
                let close_at = after_br + source[after_br..].len()
                    - source[after_br..]
                        .trim_start_matches([' ', '\t', '\r', '\n', '\u{c}'])
                        .len();
                next_tag = source_tag(source, close_at);
            }
            if let Some(close) = next_tag.filter(|close| close.closing && close.name == tag.name) {
                slots.push((
                    tag.range.start..close.range.end,
                    contents_start..close.range.start,
                ));
            }
        }
        if !tag.self_closing && !void_tag(&tag.name) {
            stack.push(tag.name);
        }
    }
    slots
}
fn escape_html_attribute(text: &str) -> String {
    escape_html_text(text)
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn mark_markup(mark: HtmlMark) -> Result<(String, &'static str), HtmlEditError> {
    let is_color = matches!(mark, HtmlMark::Color(_));
    Ok(match mark {
        HtmlMark::Bold => ("<strong>".into(), "</strong>"),
        HtmlMark::Italic => ("<em>".into(), "</em>"),
        HtmlMark::Underline => ("<u>".into(), "</u>"),
        HtmlMark::Strike => ("<s>".into(), "</s>"),
        HtmlMark::Subscript => ("<sub>".into(), "</sub>"),
        HtmlMark::Superscript => ("<sup>".into(), "</sup>"),
        HtmlMark::Code => ("<code>".into(), "</code>"),
        HtmlMark::Color(value) | HtmlMark::Background(value) => {
            if !inert_color(&value) {
                return Err(HtmlEditError::InvalidColor);
            }
            let property = if is_color {
                "color"
            } else {
                "background-color"
            };
            (
                format!(
                    "<span style=\"{property}:{}\">",
                    escape_html_attribute(&value)
                ),
                "</span>",
            )
        }
        HtmlMark::Link(value) => {
            validate_reference(&value, false)?;
            (
                format!("<a href=\"{}\">", escape_html_attribute(&value)),
                "</a>",
            )
        }
    })
}

fn inert_color(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    if value.bytes().all(|b| b.is_ascii_alphabetic()) {
        return true;
    }
    ["rgb(", "rgba(", "hsl(", "hsla("]
        .iter()
        .any(|prefix| value.starts_with(prefix))
        && value.ends_with(')')
        && value[value.find('(').unwrap_or(0) + 1..value.len() - 1]
            .bytes()
            .all(|b| {
                b.is_ascii_digit() || matches!(b, b'.' | b',' | b'%' | b'-' | b'+' | b' ' | b'/')
            })
}

fn validate_reference(value: &str, image: bool) -> Result<(), HtmlEditError> {
    if value.chars().any(char::is_control) {
        return Err(HtmlEditError::UnsafeReference);
    }
    let lower = value.trim().to_ascii_lowercase();
    if let Some((scheme, _)) = lower.split_once(':') {
        // Native image references can include a Windows drive path. New HTML
        // links use explicit inert schemes; existing unusual links stay intact.
        let drive = image
            && scheme.len() == 1
            && scheme.bytes().all(|b| b.is_ascii_alphabetic())
            && value
                .trim()
                .as_bytes()
                .get(2)
                .is_some_and(|b| matches!(b, b'/' | b'\\'));
        if !drive
            && !matches!(scheme, "http" | "https" | "file" | "mailto" | "tel" | "cid")
            && !(!image && lower == "about:blank")
            && !(image && lower.starts_with("data:image/") && !lower.starts_with("data:image/svg"))
        {
            return Err(HtmlEditError::UnsafeReference);
        }
    }
    Ok(())
}

fn tag_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in source.as_bytes()[start..].iter().enumerate() {
        if quote == Some(*byte) {
            quote = None;
        } else if quote.is_none() {
            if matches!(byte, b'\'' | b'"') {
                quote = Some(*byte);
            } else if *byte == b'>' {
                return Some(start + offset + 1);
            }
        }
    }
    None
}

fn source_tag(source: &str, start: usize) -> Option<SourceTag> {
    let bytes = source.as_bytes();
    let mut cursor = start + 1;
    let closing = bytes.get(cursor) == Some(&b'/');
    if closing {
        cursor += 1;
    }
    if !bytes.get(cursor).is_some_and(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let end = tag_end(source, start)?;
    let name_start = cursor;
    while cursor < end
        && (bytes[cursor].is_ascii_alphanumeric() || matches!(bytes[cursor], b'-' | b':' | b'_'))
    {
        cursor += 1;
    }
    let name = source[name_start..cursor].to_ascii_lowercase();
    let mut self_closing = false;
    let mut insert_at = end - 1;
    let mut attrs = Vec::new();
    while cursor < end - 1 {
        while cursor < end - 1 && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < end - 1
            && bytes[cursor] == b'/'
            && source[cursor + 1..end - 1].trim().is_empty()
        {
            self_closing = true;
            insert_at = cursor;
            break;
        }
        let attr_start = cursor;
        while cursor < end - 1
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'=' | b'/' | b'>')
        {
            cursor += 1;
        }
        if cursor == attr_start {
            cursor += 1;
            continue;
        }
        let attr_name = source[attr_start..cursor].to_ascii_lowercase();
        while cursor < end - 1 && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let mut value_range = None;
        let mut quote = None;
        if cursor < end - 1 && bytes[cursor] == b'=' {
            cursor += 1;
            while cursor < end - 1 && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < end - 1 && matches!(bytes[cursor], b'\'' | b'"') {
                quote = Some(bytes[cursor]);
                cursor += 1;
                let value_start = cursor;
                while cursor < end - 1 && Some(bytes[cursor]) != quote {
                    cursor += 1;
                }
                value_range = Some(value_start..cursor);
                if cursor < end - 1 {
                    cursor += 1;
                }
            } else {
                let value_start = cursor;
                while cursor < end - 1 && !bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                value_range = Some(value_start..cursor);
            }
        }
        let value = value_range
            .as_ref()
            .map(|range| decode_attribute_entities(&source[range.clone()]))
            .unwrap_or_default();
        attrs.push(SourceAttribute {
            name: attr_name,
            value,
            value_range,
            quote,
            source_range: attr_start..cursor,
        });
    }
    Some(SourceTag {
        name,
        attrs,
        range: start..end,
        closing,
        self_closing,
        insert_at,
    })
}

fn raw_text_end(source: &str, start: usize, tag: &str) -> usize {
    if tag == "plaintext" {
        return source.len();
    }
    let pattern = format!("</{tag}");
    let mut cursor = start;
    while let Some(offset) = source[cursor..].find('<') {
        cursor += offset;
        if source[cursor..]
            .get(..pattern.len())
            .is_some_and(|text| text.eq_ignore_ascii_case(&pattern))
            && source
                .as_bytes()
                .get(cursor + pattern.len())
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
        {
            return tag_end(source, cursor).unwrap_or(source.len());
        }
        cursor += 1;
    }
    source.len()
}

fn void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}
fn suppress_tag(tag: &str) -> bool {
    matches!(
        tag,
        "head"
            | "script"
            | "style"
            | "template"
            | "noscript"
            | "svg"
            | "math"
            | "iframe"
            | "object"
            | "canvas"
            | "video"
            | "audio"
            | "embed"
            | "textarea"
            | "title"
            | "xmp"
            | "plaintext"
    )
}
fn known_tag(tag: &str) -> bool {
    is_block(tag)
        || void_tag(tag)
        || matches!(
            tag,
            "html"
                | "head"
                | "body"
                | "span"
                | "b"
                | "strong"
                | "i"
                | "em"
                | "u"
                | "s"
                | "strike"
                | "del"
                | "font"
                | "sub"
                | "sup"
                | "code"
                | "a"
                | "small"
                | "big"
        )
}

fn apply_tag_style(tag: &SourceTag, style: &mut HtmlStyle, format: &mut BlockFormat) {
    let attr = |name: &str| {
        tag.attrs
            .iter()
            .find(|attr| attr.name == name)
            .map(|attr| attr.value.as_str())
    };
    match tag.name.as_str() {
        "b" | "strong" => style.bold = true,
        "i" | "em" => style.italic = true,
        "u" => style.underline = true,
        "s" | "strike" | "del" => style.strike = true,
        "sub" => {
            style.subscript = true;
            style.superscript = false;
        }
        "sup" => {
            style.superscript = true;
            style.subscript = false;
        }
        "code" => style.code = true,
        "pre" => {
            style.code = true;
            format.code = true;
            format.kind = HtmlBlockKind::Code;
        }
        "a" => style.link = attr("href").map(str::to_string),
        "font" => {
            style.color = attr("color")
                .filter(|color| inert_color(color))
                .map(str::to_string)
        }
        "ol" => format.list = Some(HtmlListKind::Ordered),
        "ul" => format.list = Some(HtmlListKind::Bullet),
        "li" => format.kind = HtmlBlockKind::ListItem,
        "blockquote" => format.quote = true,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            format.kind = HtmlBlockKind::Header(tag.name.as_bytes()[1] - b'0');
            style.bold = true;
        }
        _ => {}
    }
    if let Some(classes) = attr("class") {
        for class in classes.split_ascii_whitespace() {
            if let Some(indent) = class
                .strip_prefix("ql-indent-")
                .and_then(|n| n.parse::<u8>().ok())
            {
                format.indent = indent;
            }
            if class == "ql-code-block" {
                style.code = true;
                format.code = true;
                format.kind = HtmlBlockKind::Code;
            }
        }
    }
    // Quill 2 uses data-list on li; existing Quill 1 ol/ul stays supported.
    if let Some(list) = attr("data-list") {
        match list {
            "ordered" => format.list = Some(HtmlListKind::Ordered),
            "bullet" => format.list = Some(HtmlListKind::Bullet),
            _ => {}
        }
    }
    if let Some(css) = attr("style") {
        // Apply declarations in stable priority groups rather than stripping
        // !important and letting a later ordinary value override it. Keep the
        // existing per-property checks: an invalid later value cannot discard a
        // valid earlier declaration. This projects metadata only, never CSS IO.
        let declarations = css.split(';').filter_map(|part| part.split_once(':'));
        let ordinary = declarations
            .clone()
            .filter(|(_, value)| css_value_without_priority(value) == value.trim());
        let important =
            declarations.filter(|(_, value)| css_value_without_priority(value) != value.trim());
        for (name, value) in ordinary.chain(important) {
            let value = css_value_without_priority(value);
            let lower = value.to_ascii_lowercase();
            match name.trim().to_ascii_lowercase().as_str() {
                "font-weight" => {
                    if lower == "bold"
                        || lower == "bolder"
                        || lower.parse::<u16>().is_ok_and(|n| n >= 600)
                    {
                        style.bold = true;
                    } else if lower == "normal" || lower.parse::<u16>().is_ok_and(|n| n < 600) {
                        style.bold = false;
                    }
                }
                "font-style" => {
                    if matches!(lower.as_str(), "italic" | "oblique") {
                        style.italic = true;
                    } else if lower == "normal" {
                        style.italic = false;
                    }
                }
                "text-decoration" | "text-decoration-line" => {
                    if lower == "none" {
                        style.underline = false;
                        style.strike = false;
                    } else {
                        style.underline |= lower.split_ascii_whitespace().any(|s| s == "underline");
                        style.strike |= lower.split_ascii_whitespace().any(|s| s == "line-through");
                    }
                }
                "color" if matches!(lower.as_str(), "initial" | "unset") => style.color = None,
                "color" if inert_color(value) => style.color = Some(value.into()),
                "background" | "background-color"
                    if matches!(lower.as_str(), "initial" | "unset" | "transparent") =>
                {
                    style.background = None
                }
                "background" | "background-color" if inert_color(value) => {
                    style.background = Some(value.into())
                }
                "vertical-align" if lower == "sub" => {
                    style.subscript = true;
                    style.superscript = false;
                }
                "vertical-align" if lower == "super" => {
                    style.superscript = true;
                    style.subscript = false;
                }
                "vertical-align" if lower == "baseline" => {
                    style.superscript = false;
                    style.subscript = false;
                }
                "font-family" if lower == "initial" => style.code = false,
                _ => {}
            }
        }
    }
}

/// Decodes character references with the same HTML5 parser as plain-text
/// projection, without normalizing spaces or treating decoded '<' as markup.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.replace("\r\n", "\n").replace('\r', "\n");
    }
    let escaped_angles = raw.replace('<', "&lt;");
    let document = parse_document(
        Dom {
            document: Rc::new(Node::default()),
        },
        Default::default(),
    )
    .one(format!("<body><span>{escaped_angles}</span></body>"));
    fn collect(node: &Rc<Node>, output: &mut String) {
        output.push_str(&node.text.borrow());
        for child in node.children.borrow().iter() {
            collect(child, output);
        }
    }
    let mut text = String::new();
    collect(&document, &mut text);
    text
}

fn decode_attribute_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let quoted = raw.replace('"', "&quot;");
    let document = parse_document(
        Dom {
            document: Rc::new(Node::default()),
        },
        Default::default(),
    )
    .one(format!(
        "<body><span data-value=\"{quoted}\"></span></body>"
    ));
    fn find_value(node: &Rc<Node>) -> Option<String> {
        if let Some(attr) = node
            .attrs
            .borrow()
            .iter()
            .find(|attr| attr.name.local.as_ref() == "data-value")
        {
            return Some(attr.value.to_string());
        }
        for child in node.children.borrow().iter() {
            if let Some(value) = find_value(child) {
                return Some(value);
            }
        }
        None
    }
    find_value(&document).unwrap_or_else(|| raw.to_string())
}

fn decode_source_text(raw: &str, source_start: usize) -> (String, Vec<TextSegment>) {
    let mut text = String::new();
    let mut segments = Vec::new();
    let mut cursor = 0;
    while cursor < raw.len() {
        let start = cursor;
        let byte = raw.as_bytes()[cursor];
        let text_start = text.len();
        let direct;
        if byte == b'&' {
            cursor += 1;
            while cursor < raw.len()
                && (raw.as_bytes()[cursor].is_ascii_alphanumeric()
                    || raw.as_bytes()[cursor] == b'#')
            {
                cursor += 1;
            }
            if raw.as_bytes().get(cursor) == Some(&b';') {
                cursor += 1;
            }
            let value = decode_entities(&raw[start..cursor]);
            direct = value == raw[start..cursor];
            text.push_str(&value);
        } else if byte == b'\r' {
            cursor += 1;
            if raw.as_bytes().get(cursor) == Some(&b'\n') {
                cursor += 1;
            }
            text.push('\n');
            direct = false;
        } else {
            cursor += raw[cursor..]
                .find(['&', '\r'])
                .unwrap_or(raw.len() - cursor);
            text.push_str(&raw[start..cursor]);
            direct = true;
        }
        segments.push(TextSegment {
            source: source_start + start..source_start + cursor,
            text: text_start..text.len(),
            direct,
        });
    }
    (text, segments)
}

#[derive(Default)]
struct Node {
    name: Option<QualName>,
    attrs: RefCell<Vec<Attribute>>,
    is_text: bool,
    text: RefCell<String>,
    children: RefCell<Vec<Rc<Node>>>,
    parent: RefCell<Weak<Node>>,
    template: RefCell<Option<Rc<Node>>>,
}

struct Dom {
    document: Rc<Node>,
}

impl Dom {
    fn detach(node: &Rc<Node>) {
        if let Some(parent) = node.parent.borrow().upgrade() {
            parent
                .children
                .borrow_mut()
                .retain(|child| !Rc::ptr_eq(child, node));
        }
        *node.parent.borrow_mut() = Weak::new();
    }
    fn attach(parent: &Rc<Node>, node: NodeOrText<Rc<Node>>, before: Option<&Rc<Node>>) {
        let child = match node {
            NodeOrText::AppendNode(child) => {
                Self::detach(&child);
                child
            }
            NodeOrText::AppendText(text) => {
                let children = parent.children.borrow();
                let index = before
                    .and_then(|sibling| {
                        children.iter().position(|child| Rc::ptr_eq(child, sibling))
                    })
                    .unwrap_or(children.len());
                // HTML5 emits character tokens separately for entities and
                // whitespace. A browser DOM merges consecutive character
                // tokens into one text node before htmlToPlainText normalizes
                // that node. Trimming each token would lose spaces around &.
                if let Some(previous) = index
                    .checked_sub(1)
                    .and_then(|index| children.get(index))
                    .filter(|node| node.is_text)
                {
                    previous.text.borrow_mut().push_str(&text);
                    return;
                }
                Rc::new(Node {
                    is_text: true,
                    text: RefCell::new(text.to_string()),
                    ..Default::default()
                })
            }
        };
        *child.parent.borrow_mut() = Rc::downgrade(parent);
        let mut children = parent.children.borrow_mut();
        let index = before
            .and_then(|sibling| children.iter().position(|child| Rc::ptr_eq(child, sibling)))
            .unwrap_or(children.len());
        children.insert(index, child);
    }
}

impl TreeSink for Dom {
    type Handle = Rc<Node>;
    type Output = Rc<Node>;
    type ElemName<'a> = ExpandedName<'a>;
    fn finish(self) -> Self::Output {
        self.document
    }
    fn get_document(&self) -> Self::Handle {
        self.document.clone()
    }
    fn elem_name<'a>(&'a self, node: &'a Self::Handle) -> ExpandedName<'a> {
        node.name.as_ref().expect("HTML element").expanded()
    }
    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> Self::Handle {
        Rc::new(Node {
            name: Some(name),
            attrs: RefCell::new(attrs),
            template: RefCell::new(flags.template.then(|| Rc::new(Node::default()))),
            ..Default::default()
        })
    }
    fn create_comment(&self, _: StrTendril) -> Self::Handle {
        Rc::new(Node::default())
    }
    fn create_pi(&self, _: StrTendril, _: StrTendril) -> Self::Handle {
        Rc::new(Node::default())
    }
    fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
        Self::attach(parent, child, None);
    }
    fn append_before_sibling(&self, sibling: &Self::Handle, node: NodeOrText<Self::Handle>) {
        let parent = sibling.parent.borrow().upgrade();
        if let Some(parent) = parent {
            Self::attach(&parent, node, Some(sibling));
        }
    }
    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        previous: &Self::Handle,
        node: NodeOrText<Self::Handle>,
    ) {
        if element.parent.borrow().upgrade().is_some() {
            self.append_before_sibling(element, node);
        } else {
            self.append(previous, node);
        }
    }
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}
    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<Attribute>) {
        let mut current = target.attrs.borrow_mut();
        for attr in attrs {
            if !current.iter().any(|item| item.name == attr.name) {
                current.push(attr);
            }
        }
    }
    fn remove_from_parent(&self, node: &Self::Handle) {
        Self::detach(node);
    }
    fn reparent_children(&self, node: &Self::Handle, parent: &Self::Handle) {
        let children = std::mem::take(&mut *node.children.borrow_mut());
        for child in children {
            Self::attach(parent, NodeOrText::AppendNode(child), None);
        }
    }
    fn get_template_contents(&self, node: &Self::Handle) -> Self::Handle {
        node.template
            .borrow()
            .as_ref()
            .expect("template contents")
            .clone()
    }
    fn same_node(&self, one: &Self::Handle, two: &Self::Handle) -> bool {
        Rc::ptr_eq(one, two)
    }
    fn parse_error(&self, _: Cow<'static, str>) {}
    fn set_quirks_mode(&self, _: QuirksMode) {}
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "dd"
            | "details"
            | "dialog"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "li"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "tr"
            | "ul"
    )
}
fn block_end(output: &mut String) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn normalized_text(text: &str, pre: bool) -> String {
    let text = text.replace('\u{a0}', " ");
    if pre {
        return text;
    }
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| {
            let leading = line.len() - line.trim_start_matches([' ', '\t']).len();
            let mut normalized = line[..leading].to_string();
            let mut horizontal = false;
            for ch in line[leading..].chars() {
                if matches!(ch, ' ' | '\t' | '\u{c}' | '\u{b}') {
                    if !horizontal {
                        normalized.push(' ');
                    }
                    horizontal = true;
                } else {
                    normalized.push(ch);
                    horizontal = false;
                }
            }
            normalized.truncate(normalized.trim_end().len());
            normalized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn walk(node: &Rc<Node>, output: &mut String, pre: bool) {
    let Some(name) = &node.name else {
        output.push_str(&normalized_text(&node.text.borrow(), pre));
        for child in node.children.borrow().iter() {
            walk(child, output, pre);
        }
        return;
    };
    let tag = name.local.as_ref();
    if matches!(
        tag,
        "script"
            | "style"
            | "template"
            | "noscript"
            | "svg"
            | "iframe"
            | "object"
            | "canvas"
            | "video"
            | "audio"
    ) {
        return;
    }
    match tag {
        "br" => {
            output.push('\n');
            return;
        }
        "wbr" => return,
        "hr" => {
            block_end(output);
            output.push_str("---\n");
            return;
        }
        "img" | "area" => {
            if let Some(alt) = node
                .attrs
                .borrow()
                .iter()
                .find(|attr| attr.name.local.as_ref() == "alt")
                .map(|attr| attr.value.trim())
                .filter(|alt| !alt.is_empty())
            {
                output.push_str(&normalized_text(&format!("[{alt}]"), false));
            }
            return;
        }
        "td" | "th" => {
            if let Some(parent) = node.parent.borrow().upgrade() {
                let children = parent.children.borrow();
                let previous = children
                    .iter()
                    .take_while(|child| !Rc::ptr_eq(child, node))
                    .filter(|child| child.name.is_some())
                    .last();
                if previous
                    .and_then(|previous| previous.name.as_ref())
                    .is_some_and(|name| matches!(name.local.as_ref(), "td" | "th"))
                {
                    output.push('\t');
                }
            }
        }
        _ => {}
    }
    for child in node.children.borrow().iter() {
        walk(child, output, pre || tag == "pre");
    }
    if is_block(tag) {
        block_end(output);
    }
}

fn find_body(node: &Rc<Node>) -> Option<Rc<Node>> {
    if node
        .name
        .as_ref()
        .is_some_and(|name| name.local.as_ref() == "body")
    {
        return Some(node.clone());
    }
    for child in node.children.borrow().iter() {
        if let Some(body) = find_body(child) {
            return Some(body);
        }
    }
    None
}

pub fn html_to_plain_text(html: &str) -> String {
    let fragment = cliphost_core::cf_html::try_extract_fragment_and_source_url(html)
        .map(|(fragment, _)| fragment);
    let html = fragment.as_deref().unwrap_or(html);
    if html.trim().is_empty() {
        return String::new();
    }
    let document = parse_document(
        Dom {
            document: Rc::new(Node::default()),
        },
        Default::default(),
    )
    .one(html.trim());
    let body = find_body(&document).unwrap_or(document);
    let mut raw = String::new();
    for child in body.children.borrow().iter() {
        walk(child, &mut raw, false);
    }
    let raw = raw.replace("\r\n", "\n").replace('\r', "\n");
    let raw = raw
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n");
    let mut output = String::new();
    let mut newline_count = 0;
    for ch in raw.chars() {
        if ch == '\n' {
            newline_count += 1;
            if newline_count <= 2 {
                output.push(ch);
            }
        } else {
            output.push(ch);
            newline_count = 0;
        }
    }
    output.truncate(output.trim_end().len());
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_envelope_remains_in_source_and_out_of_native_canvas_text() {
        let raw = cliphost_core::cf_html::build_clipboard_html(
            "<html xmlns:o='urn:schemas-microsoft-com:office:office'><head><style>.cell{color:red}</style></head><body><!--StartFragment--><table class=cell><tr><td>中文 🦀</td></tr></table><!--EndFragment--></body></html>",
            Some("https://wps.test/"));
        let document = HtmlDocument::parse(raw.clone());
        assert_eq!(document.source(), raw);
        assert_eq!(html_to_plain_text(document.source()), "中文 🦀");
        assert!(!document.canvas_projection().text.contains("Version:"));
        assert!(!document.canvas_projection().text.contains("StartHTML:"));
        for run in document.runs() {
            assert!(run.source_range.start >= raw.find("<html").unwrap());
            assert!(raw.is_char_boundary(run.source_range.start));
        }
    }

    #[test]
    fn cf_html_without_document_context_keeps_text_before_the_first_tag() {
        for fragment in ["纯文本😀", "Hello<b>world😀</b>"] {
            let header = "Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
            let raw = header.replace("StartFragment:0000000000", &format!("StartFragment:{:010}", header.len()))
                .replace("EndFragment:0000000000", &format!("EndFragment:{:010}", header.len() + fragment.len())) + fragment;
            let document = HtmlDocument::parse(raw.clone());
            assert_eq!(document.source(), raw);
            assert_eq!(document.canvas_projection().text,
                HtmlDocument::parse(fragment).canvas_projection().text);
            assert!(document.runs().iter().all(|run| run.source_range.start >= header.len()));
        }
    }
    #[test]
    fn image_display_layout_keeps_original_attribute_fields_eq_and_independent_axes() {
        use HtmlImageDimension::{Auto, Pixels};
        for (source, width, height) in [
            (
                "<img src='qa.png' width='32' height='20' vendor='keep'>",
                Pixels(32.),
                Pixels(20.),
            ),
            ("<img src='qa.png' width='32'>", Pixels(32.), Auto),
            ("<img src='qa.png' height='20'>", Auto, Pixels(20.)),
            (
                "<img src='qa.png' width='0' height='20'>",
                Pixels(0.),
                Pixels(20.),
            ),
            ("<img src='qa.png'>", Auto, Auto),
        ] {
            let document = HtmlDocument::parse(source);
            assert_eq!(
                document.images()[0].display_layout(),
                HtmlImageLayout {
                    width,
                    height,
                    max_width: Auto,
                    max_height: Auto,
                }
            );
            assert_eq!(document.images()[0], document.images()[0].clone());
            assert_eq!(document.source(), source);
        }
        let source = "<img width='32' height='20' style='width:auto;height:12.5px' vendor='keep'>";
        let document = HtmlDocument::parse(source);
        let image = &document.images()[0];
        assert_eq!(image.width, Some(32));
        assert_eq!(image.height, Some(20));
        assert_eq!(image.display_layout().width, Auto);
        assert_eq!(image.display_layout().height, Pixels(12.5));
        assert_eq!(document.source(), source);
    }

    #[test]
    fn image_display_layout_priorities_and_last_valid_values_do_not_rewrite_style() {
        use HtmlImageDimension::{Auto, Pixels};
        for (css, expected) in [
            ("width:12px!important;width:32px", Pixels(12.)),
            ("width:32px;width:12px !IMPORTANT", Pixels(12.)),
            ("width:12px!important;width:32px!important", Pixels(32.)),
            ("width:12px!important;width:-3px!important", Pixels(12.)),
            ("width:12px;width:garbage!important", Pixels(12.)),
            ("width:auto!important;width:32px", Auto),
            ("width:12px;WIDTH:+.5PX", Pixels(0.5)),
        ] {
            let source = format!("<!--keep--><img width='80' height='20' style='{css};letter-spacing:2px' vendor='keep'>");
            let document = HtmlDocument::parse(&source);
            let layout = document.images()[0].display_layout();
            assert_eq!(layout.width, expected, "{css}");
            assert_eq!(layout.height, Pixels(20.));
            assert_eq!(document.images()[0].width, Some(80));
            assert_eq!(document.source(), source);
        }
        let document = HtmlDocument::parse("<img style='max-width:64px!important;max-width:32px;max-height:20px!important;max-height:10px;width:32px;height:20px'>");
        let layout = document.images()[0].display_layout();
        assert_eq!(layout.max_width, Pixels(64.));
        assert_eq!(layout.max_height, Pixels(20.));
        let document = HtmlDocument::parse(
            "<img style='max-width:32px;max-width:none;max-height:20px;max-height:auto'>",
        );
        assert_eq!(document.images()[0].display_layout().max_width, Auto);
        assert_eq!(
            document.images()[0].display_layout().max_height,
            Pixels(20.)
        );
    }

    #[test]
    fn image_display_layout_percentages_and_context_dependent_css_remain_explicit() {
        use HtmlImageDimension::{Percent, Unresolved};
        let source = "<img width='32' height='20' style='width:50%;height:50%;max-width:80%;max-height:80%' vendor='keep'>";
        let document = HtmlDocument::parse(source);
        let layout = document.images()[0].display_layout();
        assert_eq!(layout.width, Percent(50.));
        assert_eq!(layout.height, Unresolved);
        assert_eq!(layout.max_width, Percent(80.));
        assert_eq!(layout.max_height, Unresolved);
        assert_eq!(document.source(), source);
        for value in [
            "2em",
            "1.5rem",
            "20vw",
            "calc(100% - 4px)",
            "var(--image-width)",
            "min-content",
        ] {
            let source =
                format!("<img width='32' style='width:{value};vendor-size:keep' vendor='keep'>");
            let document = HtmlDocument::parse(&source);
            assert_eq!(
                document.images()[0].display_layout().width,
                Unresolved,
                "{value}"
            );
            assert_eq!(document.source(), source);
        }
    }

    #[test]
    fn image_display_layout_rejects_invalid_or_nonfinite_dimensions_and_accepts_css_zero() {
        use HtmlImageDimension::Pixels;
        for invalid in [
            "-1px",
            "NaNpx",
            "infinitypx",
            "1e999px",
            "32",
            "2potato",
            "32 px",
            "1.px",
            ".px",
            "-20%",
            "-1em",
            "20%%",
        ] {
            let source = format!("<img width='32' height='20' style='width:10px;width:{invalid}!important;height:{invalid};max-width:100px;max-width:{invalid}'>");
            let document = HtmlDocument::parse(&source);
            let layout = document.images()[0].display_layout();
            assert_eq!(layout.width, Pixels(10.), "{invalid}");
            assert_eq!(layout.height, Pixels(20.), "{invalid}");
            assert_eq!(layout.max_width, Pixels(100.), "{invalid}");
            assert_eq!(document.source(), source);
        }
        for (value, expected) in [
            ("0", 0.),
            ("-0", 0.),
            ("+.0px", 0.),
            (".5px", 0.5),
            ("1e2px", 100.),
        ] {
            let document = HtmlDocument::parse(format!("<img width='32' style='width:{value}'>"));
            assert_eq!(
                document.images()[0].display_layout().width,
                Pixels(expected)
            );
        }
    }

    #[test]
    fn image_display_layout_survives_source_preserving_image_reference_edits() {
        let source = "<p><!--keep--><img src='old' width='32' height='20' style='width:auto;max-width:50% !important;unknown-css:keep' vendor='keep'></p>";
        let mut document = HtmlDocument::parse(source);
        let before = document.images()[0].display_layout();
        document.replace_image_reference(0, "new").unwrap();
        assert_eq!(document.images()[0].display_layout(), before);
        assert_eq!(document.source(), source.replace("src='old'", "src='new'"));
    }

    fn canvas_line_formats(document: &HtmlDocument) -> Vec<HtmlBlock> {
        document
            .canvas_projection()
            .spans
            .iter()
            .filter_map(|span| match &span.kind {
                HtmlCanvasKind::Boundary { left, .. } => document.canvas_block_format(left),
                _ => None,
            })
            .collect()
    }

    fn canvas_line_owner(document: &HtmlDocument, index: usize) -> HtmlCanvasBlock {
        document
            .canvas_projection()
            .spans
            .into_iter()
            .filter_map(|span| match span.kind {
                HtmlCanvasKind::Boundary { left, .. } => Some(left),
                _ => None,
            })
            .nth(index)
            .unwrap()
    }

    #[test]
    fn canvas_block_toggle_formats_virtual_empty_paragraph_and_heading_without_changing_text() {
        for original in [
            "<p vendor='keep'></p><p data-sibling='keep'><b>B</b></p>",
            "<h2 vendor='keep'></h2><p data-sibling='keep'><b>B</b></p>",
        ] {
            let mut document = HtmlDocument::parse(original);
            let first = canvas_line_owner(&document, 0);
            assert_eq!(first.index, None);
            document
                .toggle_canvas_block(&first, HtmlBlockMark::Header(2))
                .unwrap();
            assert_eq!(document.canvas_projection().text, "\nB\n");
            let expected = if original.starts_with("<p") {
                HtmlBlockKind::Header(2)
            } else {
                HtmlBlockKind::Paragraph
            };
            assert_eq!(canvas_line_formats(&document)[0].kind, expected);
            assert!(document.source().contains("vendor='keep'"));
            assert!(document
                .source()
                .ends_with("<p data-sibling='keep'><b>B</b></p>"));
            let first = canvas_line_owner(&document, 0);
            document
                .toggle_canvas_block(&first, HtmlBlockMark::Header(2))
                .unwrap();
            assert_eq!(document.canvas_projection().text, "\nB\n");
            assert_ne!(canvas_line_formats(&document)[0].kind, expected);
        }
    }

    #[test]
    fn canvas_block_toggle_empty_list_keeps_sibling_list_and_unknown_attrs() {
        let original = "<ol data-list-owner='keep'><li data-list='ordered' class='vendor ql-indent-1' data-item='keep'></li><li data-sibling='keep'>B</li></ol>";
        let mut document = HtmlDocument::parse(original);
        let first = canvas_line_owner(&document, 0);
        assert_eq!(first.index, None);
        document
            .toggle_canvas_block(&first, HtmlBlockMark::List(HtmlListKind::Ordered))
            .unwrap();
        assert_eq!(document.canvas_projection().text, "\nB\n");
        let formats = canvas_line_formats(&document);
        assert_eq!(formats[0].list, None);
        assert_eq!(formats[0].indent, 1);
        assert_eq!(formats[1].list, Some(HtmlListKind::Ordered));
        assert!(document.source().contains("data-item='keep'"));
        assert!(document.source().contains("vendor"));
        assert!(document.source().contains("<li data-sibling='keep'>B</li>"));
        let first = canvas_line_owner(&document, 0);
        document
            .toggle_canvas_block(&first, HtmlBlockMark::List(HtmlListKind::Bullet))
            .unwrap();
        let formats = canvas_line_formats(&document);
        assert_eq!(document.canvas_projection().text, "\nB\n");
        assert_eq!(formats[0].list, Some(HtmlListKind::Bullet));
        assert_eq!(formats[1].list, Some(HtmlListKind::Ordered));
    }

    #[test]
    fn canvas_block_toggle_initializes_raw_empty_owner_and_preserves_unrelated_source() {
        for original in ["", "<!--keep-->", "<html><body><!--keep--></body></html>"] {
            for mark in [
                HtmlBlockMark::Header(3),
                HtmlBlockMark::List(HtmlListKind::Bullet),
                HtmlBlockMark::Blockquote,
                HtmlBlockMark::CodeBlock,
                HtmlBlockMark::Indent(2),
            ] {
                let mut document = HtmlDocument::parse(original);
                let first = canvas_line_owner(&document, 0);
                assert_eq!(first.source_range, 0..0);
                document
                    .toggle_canvas_block(&first, mark)
                    .unwrap_or_else(|error| panic!("first {original:?} {mark:?}: {error:?}"));
                assert_eq!(document.canvas_projection().text, "\n");
                let format = &canvas_line_formats(&document)[0];
                match mark {
                    HtmlBlockMark::Header(level) => {
                        assert_eq!(format.kind, HtmlBlockKind::Header(level))
                    }
                    HtmlBlockMark::List(kind) => assert_eq!(format.list, Some(kind)),
                    HtmlBlockMark::Blockquote => assert!(format.quote),
                    HtmlBlockMark::CodeBlock => assert!(format.code),
                    HtmlBlockMark::Indent(level) => assert_eq!(format.indent, level),
                    HtmlBlockMark::Paragraph => unreachable!(),
                }
                if !original.is_empty() {
                    assert!(document.source().contains("<!--keep-->"));
                }
                let first = canvas_line_owner(&document, 0);
                document
                    .toggle_canvas_block(&first, mark)
                    .unwrap_or_else(|error| panic!("second {original:?} {mark:?}: {error:?}"));
                assert_eq!(document.canvas_projection().text, "\n");
            }
        }
    }

    #[test]
    fn canvas_block_toggle_populated_owner_and_rejections_are_atomic() {
        let original = "<p data-p='keep'><img src='opaque' data-img='keep'><b>A</b></p><p>B</p>";
        let mut document = HtmlDocument::parse(original);
        let first = canvas_line_owner(&document, 0);
        document
            .toggle_canvas_block(&first, HtmlBlockMark::Blockquote)
            .unwrap();
        assert_eq!(document.canvas_projection().text, "\u{fffc}A\nB\n");
        assert!(canvas_line_formats(&document)[0].quote);
        assert!(!canvas_line_formats(&document)[1].quote);
        assert!(document
            .source()
            .contains("<img src='opaque' data-img='keep'><b>A</b>"));
        let changed = document.source().to_string();
        assert_eq!(
            document.toggle_canvas_block(&first, HtmlBlockMark::Header(1)),
            Err(HtmlEditError::InvalidSourceRange)
        );
        assert_eq!(document.source(), changed);
        let original = "<div vendor='keep'>A<p>B</p>C</div>";
        let mut document = HtmlDocument::parse(original);
        let first = canvas_line_owner(&document, 0);
        assert_eq!(
            document.toggle_canvas_block(&first, HtmlBlockMark::Header(1)),
            Err(HtmlEditError::UnsupportedBlockStructure)
        );
        assert_eq!(document.source(), original);
        let mut document = HtmlDocument::parse("");
        let first = canvas_line_owner(&document, 0);
        assert_eq!(
            document.toggle_canvas_block(&first, HtmlBlockMark::Header(0)),
            Err(HtmlEditError::InvalidHeader)
        );
        assert_eq!(document.source(), "");
    }

    #[test]
    fn collapsed_canvas_typing_keeps_left_text_style_across_consecutive_insertions() {
        let mut document = HtmlDocument::parse("<p>AB</p>");
        assert_eq!(document.replace_canvas_range(1..1, "X").unwrap(), 2);
        document
            .wrap_text_range_anchor(0, 1..2, HtmlMark::Bold)
            .unwrap();
        assert_eq!(document.source(), "<p>A<strong>X</strong>B</p>");
        assert_eq!(document.replace_canvas_range(2..2, "Y").unwrap(), 3);
        assert_eq!(document.replace_canvas_range(3..3, "😀").unwrap(), 7);
        assert_eq!(document.source(), "<p>A<strong>XY😀</strong>B</p>");
        assert_eq!(document.canvas_projection().text, "AXY😀B\n");
        assert!(document.runs()[1].style.bold);

        let mut document = HtmlDocument::parse("<p><b>A</b></p><p>B</p>");
        document.replace_canvas_range(2..2, "X").unwrap();
        assert_eq!(document.source(), "<p><b>A</b></p><p>XB</p>");
        assert!(!document.runs()[1].style.bold);
    }

    #[test]
    fn collapsed_canvas_typing_respects_left_reset_and_nonempty_selection_stays_right_affine() {
        let original = "<p><strong>A</strong><span style='font-weight:normal' vendor='keep'>X</span><strong>B</strong></p>";
        let mut document = HtmlDocument::parse(original);
        assert_eq!(document.replace_canvas_range(2..2, "Y").unwrap(), 3);
        assert_eq!(document.replace_canvas_range(3..3, "Z").unwrap(), 4);
        assert_eq!(document.source(), "<p><strong>A</strong><span style='font-weight:normal' vendor='keep'>XYZ</span><strong>B</strong></p>");
        assert!(!document.runs()[1].style.bold);
        assert!(document.runs()[2].style.bold);

        let mut document = HtmlDocument::parse("<p><strong>A</strong>B</p>");
        document.replace_canvas_range(1..2, "Q").unwrap();
        assert_eq!(document.source(), "<p><strong>A</strong>Q</p>");
        assert!(!document.runs()[1].style.bold);
    }

    #[test]
    fn canvas_image_insertion_uses_exact_midrun_caret_and_escapes_reference_attributes() {
        let mut document = HtmlDocument::parse("<p vendor='keep'>AB<!--keep--></p>");
        let src = "https://example.invalid/a?x=1&title=\"quoted\"";
        let alt = "<alt> & \"quoted\" 'value'";
        assert_eq!(document.insert_canvas_image(1..1, src, alt).unwrap(), 4);
        assert_eq!(document.canvas_projection().text, "A\u{fffc}B\n");
        assert_eq!(document.images()[0].src, src);
        assert_eq!(document.images()[0].alt, alt);
        assert_eq!(document.source(), "<p vendor='keep'>A<img src=\"https://example.invalid/a?x=1&amp;title=&quot;quoted&quot;\" alt=\"&lt;alt&gt; &amp; &quot;quoted&quot; &#39;value&#39;\">B<!--keep--></p>");
    }

    #[test]
    fn canvas_image_insertion_keeps_style_and_untouched_entity_spelling_and_splits_partial_entity()
    {
        let mut document = HtmlDocument::parse("<p vendor='keep'><b>A&amp;😀&#x42;</b></p>");
        assert_eq!(
            document.insert_canvas_image(2..2, "cid:pic", "a").unwrap(),
            5
        );
        assert_eq!(
            document.source(),
            "<p vendor='keep'><b>A&amp;<img src=\"cid:pic\" alt=\"a\">😀&#x42;</b></p>"
        );
        assert_eq!(document.canvas_projection().text, "A&\u{fffc}😀B\n");
        assert!(document.runs().iter().all(|run| run.style.bold));

        let mut document = HtmlDocument::parse("<p><i>A&NotEqualTilde;B&amp;</i></p>");
        assert_eq!(document.insert_canvas_image(4..4, "pic", "").unwrap(), 7);
        assert_eq!(
            document.source(),
            "<p><i>A≂<img src=\"pic\" alt=\"\">\u{338}B&amp;</i></p>"
        );
        assert_eq!(document.canvas_projection().text, "A≂\u{fffc}\u{338}B&\n");
        assert!(document.runs().iter().all(|run| run.style.italic));
    }

    #[test]
    fn canvas_image_insertion_initializes_empty_source_or_row_and_protects_terminal_newline() {
        for original in [
            "",
            "<!DOCTYPE html><html><body><!--keep--></body></html>",
            "<p vendor='keep'></p>",
            "<p vendor='keep'><br></p>",
            "<ol><li vendor='keep'></li></ol>",
            "<h2 vendor='keep'></h2>",
            "<p><!--keep--><![CDATA[opaque]]></p>",
        ] {
            let mut document = HtmlDocument::parse(original);
            assert_eq!(document.canvas_projection().text, "\n");
            // Selecting the protected final newline still inserts before it.
            assert_eq!(document.insert_canvas_image(0..1, "pic", "a").unwrap(), 3);
            assert_eq!(document.canvas_projection().text, "\u{fffc}\n");
            assert_eq!(document.images().len(), 1);
            if original.contains("vendor='keep'") {
                assert!(document.source().contains("vendor='keep'"));
            }
            if original.contains("<!--keep-->") {
                assert!(document.source().contains("<!--keep-->"));
            }
            if original.contains("<![CDATA[opaque]]>") {
                assert!(document.source().contains("<![CDATA[opaque]]>"));
            }
        }
        let mut document = HtmlDocument::parse("");
        assert_eq!(document.insert_canvas_image(1..1, "pic", "").unwrap(), 3);
        assert_eq!(document.source(), "<p><img src=\"pic\" alt=\"\"></p>");
    }

    #[test]
    fn canvas_image_insertion_before_after_and_over_selected_objects_retains_unselected_source() {
        let mut document =
            HtmlDocument::parse("<p vendor='keep'><img src='old' data-old='keep'></p>");
        assert_eq!(document.insert_canvas_image(0..0, "before", "").unwrap(), 3);
        assert_eq!(document.insert_canvas_image(6..6, "after", "").unwrap(), 9);
        assert_eq!(
            document.canvas_projection().text,
            "\u{fffc}\u{fffc}\u{fffc}\n"
        );
        assert_eq!(document.images()[1].src, "old");
        assert!(document
            .source()
            .contains("<img src='old' data-old='keep'>"));
        assert_eq!(
            document.insert_canvas_image(3..6, "replace", "a").unwrap(),
            6
        );
        assert_eq!(document.images().len(), 3);
        assert_eq!(document.images()[0].src, "before");
        assert_eq!(document.images()[1].src, "replace");
        assert_eq!(document.images()[2].src, "after");
        assert!(!document.source().contains("data-old='keep'"));
        assert!(document.source().starts_with("<p vendor='keep'>"));
    }

    #[test]
    fn canvas_image_insertion_replaces_cross_paragraph_object_and_text_selection_locally() {
        let mut document = HtmlDocument::parse("<p data-left='keep'>A<img src='old' data-old='selected'><b data-b='keep'>BC</b></p><!--keep--><p data-right='keep'><i data-i='keep'>DE</i>F</p>");
        assert_eq!(document.canvas_projection().text, "A\u{fffc}BC\nDEF\n");
        assert_eq!(document.insert_canvas_image(1..9, "new", "").unwrap(), 4);
        assert_eq!(document.canvas_projection().text, "A\u{fffc}F\n");
        assert_eq!(document.images().len(), 1);
        assert_eq!(document.images()[0].src, "new");
        for payload in [
            "data-left='keep'",
            "data-right='keep'",
            "<b data-b='keep'></b>",
            "<i data-i='keep'></i>",
            "<!--keep-->",
        ] {
            assert!(document.source().contains(payload));
        }
        assert!(!document.source().contains("data-old='selected'"));
    }

    #[test]
    fn canvas_image_insertion_rejects_bad_refs_ranges_and_unsafe_source_without_mutation() {
        for src in [
            "",
            "  ",
            "javascript:alert(1)",
            "data:text/html,<img>",
            "data:image/svg+xml,<svg></svg>",
            "https://example.invalid/\nimage",
        ] {
            let original = "<p vendor='keep'>AB</p>";
            let mut document = HtmlDocument::parse(original);
            assert_eq!(
                document.insert_canvas_image(1..1, src, "a"),
                Err(HtmlEditError::UnsafeReference)
            );
            assert_eq!(document.source(), original);
        }
        let original = "<p>😀<img src='keep'></p>";
        for range in [1..1, 4..5, 0..9, 7..6] {
            let mut document = HtmlDocument::parse(original);
            assert_eq!(
                document.insert_canvas_image(range, "pic", "a"),
                Err(HtmlEditError::InvalidTextRange)
            );
            assert_eq!(document.source(), original);
        }
        for (original, range) in [
            ("<p>A", 1..1),
            ("<p><b>A</p>", 1..1),
            ("<p>", 0..0),
            ("<p>A</p><object vendor='keep'></object><p>B</p>", 0..3),
        ] {
            let mut document = HtmlDocument::parse(original);
            assert_eq!(
                document.insert_canvas_image(range, "pic", "a"),
                Err(HtmlEditError::UnsupportedBlockStructure)
            );
            assert_eq!(document.source(), original);
        }
    }

    #[test]
    fn virtual_empty_canvas_lines_keep_owner_and_real_parent_formatting() {
        let original = "<blockquote class='ql-indent-2'><ol><li data-v='keep'></li><li>txt</li></ol></blockquote><h3 data-h='keep'></h3><p class='ql-code-block'></p>";
        let document = HtmlDocument::parse(original);
        assert_eq!(document.canvas_projection().text, "\ntxt\n\n\n");
        let formats = canvas_line_formats(&document);
        assert_eq!(formats.len(), 4);
        assert_eq!(formats[0].kind, HtmlBlockKind::ListItem);
        assert_eq!(formats[0].list, Some(HtmlListKind::Ordered));
        assert_eq!(formats[0].indent, 2);
        assert!(formats[0].quote);
        assert_eq!(formats[2].kind, HtmlBlockKind::Header(3));
        assert!(formats[3].code);
        assert_eq!(formats[3].kind, HtmlBlockKind::Code);
        assert_eq!(document.source(), original);
    }

    #[test]
    fn keyboard_backspace_outdents_before_cancelling_a_list_without_deleting_text_or_newline() {
        let mut document = HtmlDocument::parse("<ul data-v='list'><li class='vendor ql-indent-2' data-item='keep'>A</li><li>B</li></ul>");
        assert_eq!(
            document
                .canvas_keyboard_edit(0, HtmlCanvasKey::Backspace)
                .unwrap(),
            Some(0)
        );
        assert_eq!(canvas_line_formats(&document)[0].indent, 1);
        assert_eq!(
            canvas_line_formats(&document)[0].list,
            Some(HtmlListKind::Bullet)
        );
        document
            .canvas_keyboard_edit(0, HtmlCanvasKey::Backspace)
            .unwrap();
        assert_eq!(canvas_line_formats(&document)[0].indent, 0);
        assert_eq!(
            canvas_line_formats(&document)[0].list,
            Some(HtmlListKind::Bullet)
        );
        document
            .canvas_keyboard_edit(0, HtmlCanvasKey::Backspace)
            .unwrap();
        let formats = canvas_line_formats(&document);
        assert_eq!(formats[0].list, None);
        assert_eq!(formats[1].list, Some(HtmlListKind::Bullet));
        assert_eq!(document.canvas_projection().text, "A\nB\n");
        assert!(
            document.source().contains("data-item='keep'") && document.source().contains("vendor")
        );
        assert_eq!(
            document
                .canvas_keyboard_edit(0, HtmlCanvasKey::Backspace)
                .unwrap(),
            None
        );
    }

    #[test]
    fn keyboard_empty_list_exit_preserves_empty_line_sibling_formats_and_attrs() {
        for (key, class) in [
            (HtmlCanvasKey::Backspace, "vendor"),
            (HtmlCanvasKey::Enter, "vendor ql-indent-2"),
        ] {
            let mut document = HtmlDocument::parse(format!("<ol data-v='list'><li data-list='ordered' class='{class}' data-item='keep'></li><li>B</li></ol>"));
            assert_eq!(document.canvas_keyboard_edit(0, key).unwrap(), Some(0));
            assert_eq!(document.canvas_projection().text, "\nB\n");
            let formats = canvas_line_formats(&document);
            assert_eq!(formats[0].list, None);
            assert_eq!(formats[0].indent, 0);
            assert_eq!(formats[1].list, Some(HtmlListKind::Ordered));
            assert!(
                document.source().contains("data-item='keep'")
                    && document.source().contains("vendor")
            );
        }
        let mut document = HtmlDocument::parse("<ul><li data-item='keep'><Br></li><li>B</li></ul>");
        document
            .canvas_keyboard_edit(0, HtmlCanvasKey::Enter)
            .unwrap();
        assert_eq!(document.canvas_projection().text, "\nB\n");
    }

    #[test]
    fn keyboard_enter_at_heading_end_exits_new_line_only_and_tracks_unicode_caret() {
        let mut document = HtmlDocument::parse("<h2 data-h='keep'><b>A😀</b></h2><p>B</p>");
        assert_eq!(
            document
                .canvas_keyboard_edit(5, HtmlCanvasKey::Enter)
                .unwrap(),
            Some(6)
        );
        assert_eq!(document.canvas_projection().text, "A😀\n\nB\n");
        let formats = canvas_line_formats(&document);
        assert_eq!(formats[0].kind, HtmlBlockKind::Header(2));
        assert_eq!(formats[1].kind, HtmlBlockKind::Paragraph);
        assert_eq!(formats[2].kind, HtmlBlockKind::Paragraph);
        assert!(document
            .source()
            .starts_with("<h2 data-h='keep'><b>A😀</b></h2>"));
        assert!(document.source().contains("<div data-h='keep'><br></div>"));
        let mut document = HtmlDocument::parse("<h1 vendor='keep'></h1><p>B</p>");
        assert_eq!(
            document
                .canvas_keyboard_edit(0, HtmlCanvasKey::Enter)
                .unwrap(),
            Some(1)
        );
        assert_eq!(document.canvas_projection().text, "\n\nB\n");
        assert_eq!(
            canvas_line_formats(&document)[0].kind,
            HtmlBlockKind::Header(1)
        );
        assert_eq!(
            canvas_line_formats(&document)[1].kind,
            HtmlBlockKind::Paragraph
        );
        assert!(document.source().contains("vendor='keep'"));
    }

    #[test]
    fn keyboard_normal_or_midline_keys_delegate_and_invalid_structure_is_atomic() {
        for (original, caret, key) in [
            ("<h3>AB</h3>", 1, HtmlCanvasKey::Enter),
            ("<ul><li>A</li></ul>", 1, HtmlCanvasKey::Enter),
            ("<p>AB</p>", 0, HtmlCanvasKey::Backspace),
            ("<ul><li>AB</li></ul>", 1, HtmlCanvasKey::Backspace),
        ] {
            let mut document = HtmlDocument::parse(original);
            assert_eq!(document.canvas_keyboard_edit(caret, key).unwrap(), None);
            assert_eq!(document.source(), original);
        }
        let original = "<h2>A";
        let mut document = HtmlDocument::parse(original);
        assert_eq!(
            document.canvas_keyboard_edit(1, HtmlCanvasKey::Enter),
            Err(HtmlEditError::UnsupportedBlockStructure)
        );
        assert_eq!(document.source(), original);
        let original = "<ul><li class='ql-indent-2'>😀</li></ul>";
        let mut document = HtmlDocument::parse(original);
        assert_eq!(
            document.canvas_keyboard_edit(1, HtmlCanvasKey::Backspace),
            Err(HtmlEditError::InvalidTextRange)
        );
        assert_eq!(document.source(), original);
    }

    #[test]
    fn keyboard_outdent_from_inherited_indent_changes_only_selected_line() {
        let mut document = HtmlDocument::parse(
            "<div class='ql-indent-1 vendor' data-v='keep'><p>A</p><p>B</p></div>",
        );
        assert_eq!(
            document
                .canvas_keyboard_edit(0, HtmlCanvasKey::Backspace)
                .unwrap(),
            Some(0)
        );
        let formats = canvas_line_formats(&document);
        assert_eq!(formats[0].indent, 0);
        assert_eq!(formats[1].indent, 1);
        assert_eq!(document.canvas_projection().text, "A\nB\n");
        assert!(
            document.source().contains("vendor") && document.source().contains("data-v='keep'")
        );
    }
    #[test]
    fn canvas_projects_contiguous_runs_images_breaks_and_empty_paragraphs_with_final_newline() {
        let document = HtmlDocument::parse("<p><b>A</b><img src='x' data-v='keep'><i>B</i></p><p data-empty='keep'></p><p>C<br>D</p>");
        let projection = document.canvas_projection();
        assert_eq!(projection.text, "A\u{fffc}B\n\nC\nD\n");
        assert_eq!(
            projection
                .spans
                .iter()
                .filter(|span| matches!(span.kind, HtmlCanvasKind::Image { .. }))
                .count(),
            1
        );
        let image = projection
            .spans
            .iter()
            .find(|span| matches!(span.kind, HtmlCanvasKind::Image { .. }))
            .unwrap();
        assert_eq!(image.range, 1..4);
        assert_eq!(
            projection.text[image.range.clone()].encode_utf16().count(),
            1
        );
        assert_eq!(
            projection
                .spans
                .iter()
                .filter(|span| matches!(span.kind, HtmlCanvasKind::Boundary { .. }))
                .count(),
            3
        );
        assert!(matches!(
            projection.spans.last().unwrap().kind,
            HtmlCanvasKind::Boundary { terminal: true, .. }
        ));
        assert_eq!(HtmlDocument::parse("").canvas_projection().text, "\n");
        assert_eq!(
            HtmlDocument::parse("<p><br></p>").canvas_projection().text,
            "\n"
        );
    }

    #[test]
    fn canvas_replace_deletes_selected_object_and_run_fragments_while_retaining_unknown_markup() {
        let original = "<p>A<img SRC='keep' data-image='selected'><unknown data-u='preserve'>B</unknown><!--comment--><b data-b='keep'>C</b>D</p>";
        let mut document = HtmlDocument::parse(original);
        let caret = document.replace_canvas_range(1..6, "X").unwrap();
        assert_eq!(caret, 2);
        assert_eq!(
            document.source(),
            "<p>AX<unknown data-u='preserve'></unknown><!--comment--><b data-b='keep'></b>D</p>"
        );
        assert_eq!(document.canvas_projection().text, "AXD\n");
        assert!(document.images().is_empty());
        let mut document =
            HtmlDocument::parse("<p><b data-v='keep'>A&#x1F600;</b><i>&amp;B</i></p>");
        assert_eq!(document.replace_canvas_range(1..6, "中").unwrap(), 4);
        assert_eq!(document.source(), "<p><b data-v='keep'>A中</b><i>B</i></p>");
        assert_eq!(document.canvas_projection().text, "A中B\n");
    }

    #[test]
    fn canvas_boundary_delete_merges_paragraphs_and_preserves_both_attribute_sets() {
        let mut document = HtmlDocument::parse(
            "<p data-left='a'>A</p><!--keep--><p style='color:red' data-right='b'>B</p>",
        );
        assert_eq!(document.replace_canvas_range(1..2, "").unwrap(), 1);
        assert_eq!(
            document.source(),
            "<p data-left='a'>A<!--keep--><span style='color:red' data-right='b'>B</span></p>"
        );
        assert_eq!(document.canvas_projection().text, "AB\n");
        assert_eq!(document.runs()[1].style.color.as_deref(), Some("red"));
        let mut document = HtmlDocument::parse("<p a='1'>A</p><p b='2'>B</p><p c='3'>C</p>");
        assert_eq!(document.replace_canvas_range(1..5, "").unwrap(), 1);
        assert_eq!(
            document.source(),
            "<p a='1'>A<span b='2'></span><span c='3'></span></p>"
        );
        assert_eq!(document.canvas_projection().text, "A\n");
        let len = document.canvas_projection().text.len();
        assert_eq!(document.replace_canvas_range(0..len, "").unwrap(), 0);
        assert_eq!(document.canvas_projection().text, "\n");
        assert!(
            document.source().contains("a='1'")
                && document.source().contains("b='2'")
                && document.source().contains("c='3'")
        );
    }

    #[test]
    fn canvas_newline_splits_only_local_source_and_preserves_formatted_ancestors() {
        let mut document = HtmlDocument::parse("<p data-row='keep'><b vendor='x'>x😀y</b></p>");
        assert_eq!(document.replace_canvas_range(5..5, "\r\n").unwrap(), 6);
        assert_eq!(document.source(), "<p data-row='keep'><b vendor='x'>x😀</b></p><p data-row='keep'><b vendor='x'>y</b></p>");
        assert_eq!(document.canvas_projection().text, "x😀\ny\n");
        assert!(document.runs().iter().all(|run| run.style.bold));
        let end = document.canvas_projection().text.len() - 1;
        assert_eq!(
            document.replace_canvas_range(end..end, "\n").unwrap(),
            end + 1
        );
        assert_eq!(document.canvas_projection().text, "x😀\ny\n\n");
    }

    #[test]
    fn canvas_empty_input_break_delete_and_terminal_guard_keep_meaningful_whitespace() {
        let mut document = HtmlDocument::parse("<p data-v='keep'><br></p>");
        assert_eq!(document.replace_canvas_range(0..0, " ").unwrap(), 1);
        assert_eq!(document.source(), "<p data-v='keep'> </p>");
        assert_eq!(document.canvas_projection().text, " \n");
        let end = document.canvas_projection().text.len() - 1;
        assert_eq!(
            document.replace_canvas_range(end..end + 1, "").unwrap(),
            end
        );
        assert_eq!(document.canvas_projection().text, " \n");
        let mut document = HtmlDocument::parse("<p>A<br>B</p>");
        document.replace_canvas_range(1..2, "").unwrap();
        assert_eq!(document.source(), "<p>AB</p>");
        let mut document = HtmlDocument::parse("");
        assert_eq!(document.replace_canvas_range(0..0, "a\nb").unwrap(), 3);
        assert_eq!(document.source(), "<p>a</p><p>b</p>");
        assert_eq!(document.canvas_projection().text, "a\nb\n");
    }

    #[test]
    fn canvas_unsafe_structural_crossings_and_partial_object_or_utf8_ranges_are_atomic_errors() {
        for original in [
            "<p>A</p><script>keep()</script><p>B</p>",
            "<div>A<p>B</p>C</div>",
            "<div><p>A</p></div><div><p>B</p></div>",
        ] {
            let mut document = HtmlDocument::parse(original);
            assert_eq!(
                document.replace_canvas_range(0..3, "X"),
                Err(HtmlEditError::UnsupportedBlockStructure)
            );
            assert_eq!(document.source(), original);
        }
        let original = "<p>A<img src='keep'>😀</p>";
        let mut document = HtmlDocument::parse(original);
        assert_eq!(
            document.replace_canvas_range(2..3, "x"),
            Err(HtmlEditError::InvalidTextRange)
        );
        assert_eq!(
            document.replace_canvas_range(5..6, "x"),
            Err(HtmlEditError::InvalidTextRange)
        );
        assert_eq!(document.source(), original);
    }

    #[test]
    fn canvas_safe_formatted_siblings_join_keep_first_line_format_and_drop_seam_indentation() {
        for original in ["<h2 left='keep'>A</h2>\r\n <!--keep--> <p right='keep'>B</p>", "<blockquote data-v='keep'><p left='keep'>A</p>\n<p right='keep'>B</p></blockquote>", "<ol data-v='keep'><li class='ql-indent-2' left='keep'>A</li>\n<li right='keep'>B</li></ol>"] {
            let mut document = HtmlDocument::parse(original);
            let before = document.blocks()[0].clone();
            document.replace_canvas_range(1..2, "").unwrap();
            assert_eq!(document.canvas_projection().text, "AB\n");
            assert!(document.source().contains("left='keep'") && document.source().contains("right='keep'"));
            assert_eq!(document.blocks()[0].kind, before.kind);
            assert_eq!(document.blocks()[0].list, before.list);
            assert_eq!(document.blocks()[0].indent, before.indent);
            assert_eq!(document.blocks()[0].quote, before.quote);
            if original.contains("<!--keep-->") { assert!(document.source().contains("<!--keep-->")); }
        }
        let mut document = HtmlDocument::parse("<ol><li data-v='keep'>AB</li></ol>");
        document.replace_canvas_range(1..1, "\n").unwrap();
        assert_eq!(
            document.source(),
            "<ol><li data-v='keep'>A</li><li data-v='keep'>B</li></ol>"
        );
        assert_eq!(document.canvas_projection().text, "A\nB\n");
        assert!(document
            .blocks()
            .iter()
            .all(|block| block.list == Some(HtmlListKind::Ordered)));
    }
    #[test]
    fn quill_data_list_is_cancelled_without_dropping_unknown_attributes_or_sibling_list() {
        let original =
            "<ol data-vendor='list'><li data-list='ordered' vendor='x'>A</li><li>B</li></ol>";
        let mut document = HtmlDocument::parse(original);
        document
            .toggle_block(0, HtmlBlockMark::List(HtmlListKind::Ordered))
            .unwrap();
        assert_eq!(document.blocks()[0].list, None);
        assert_eq!(document.blocks()[1].list, Some(HtmlListKind::Ordered));
        assert!(document.source().contains("vendor='x'"));
        document
            .toggle_block(0, HtmlBlockMark::List(HtmlListKind::Ordered))
            .unwrap();
        assert_eq!(document.blocks()[0].list, Some(HtmlListKind::Ordered));
        document
            .toggle_block(0, HtmlBlockMark::List(HtmlListKind::Ordered))
            .unwrap();
        assert_eq!(document.blocks()[0].list, None);
        let mut document = HtmlDocument::parse(original);
        let anchor = document.clear_selection_format_anchor(0, 0..1).unwrap();
        let selected = document
            .runs()
            .iter()
            .find(|run| run.source_range == anchor)
            .unwrap();
        assert_eq!(document.blocks()[selected.block_index].list, None);
        assert_eq!(document.blocks()[1].list, Some(HtmlListKind::Ordered));
        assert!(document.source().contains("vendor='x'"));
        let mut document =
            HtmlDocument::parse("<div data-list='bullet' vendor='outer'><p>A</p><p>B</p></div>");
        document
            .toggle_block(0, HtmlBlockMark::List(HtmlListKind::Bullet))
            .unwrap();
        assert_eq!(document.blocks()[0].list, None);
        assert_eq!(document.blocks()[1].list, Some(HtmlListKind::Bullet));
        assert!(document.source().contains("vendor='outer'"));
    }

    #[test]
    fn inactive_structural_buttons_refuse_a_shared_container_without_changing_siblings() {
        let original = "<div vendor='x'>A<p>B</p>C</div>";
        for mark in [
            HtmlBlockMark::Header(1),
            HtmlBlockMark::List(HtmlListKind::Bullet),
            HtmlBlockMark::Blockquote,
            HtmlBlockMark::CodeBlock,
            HtmlBlockMark::Indent(1),
        ] {
            let mut document = HtmlDocument::parse(original);
            let before_runs = document.runs().to_vec();
            let before_blocks = document.blocks().to_vec();
            assert_eq!(
                document.toggle_block(0, mark),
                Err(HtmlEditError::UnsupportedBlockStructure)
            );
            assert_eq!(document.source(), original);
            assert_eq!(document.runs(), before_runs);
            assert_eq!(document.blocks(), before_blocks);
        }
    }
    #[test]
    fn structural_buttons_repeat_without_nesting_active_formats_and_indent_stays_set() {
        let mut document =
            HtmlDocument::parse("<p data-vendor='keep' style='color:red'><i>A</i></p><p>B</p>");
        for mark in [
            HtmlBlockMark::Header(2),
            HtmlBlockMark::CodeBlock,
            HtmlBlockMark::Blockquote,
            HtmlBlockMark::List(HtmlListKind::Bullet),
        ] {
            for _ in 0..2 {
                document.toggle_block(0, mark).unwrap();
                let active = &document.blocks()[0];
                match mark {
                    HtmlBlockMark::Header(level) => {
                        assert_eq!(active.kind, HtmlBlockKind::Header(level))
                    }
                    HtmlBlockMark::CodeBlock => assert!(active.code),
                    HtmlBlockMark::Blockquote => assert!(active.quote),
                    HtmlBlockMark::List(kind) => assert_eq!(active.list, Some(kind)),
                    _ => unreachable!(),
                }
                document.toggle_block(0, mark).unwrap();
                let inactive = &document.blocks()[0];
                assert_eq!(inactive.kind, HtmlBlockKind::Paragraph);
                assert!(!inactive.quote && !inactive.code && inactive.list.is_none());
                assert!(document.runs()[0].style.italic);
                assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
                assert!(document.source().contains("data-vendor='keep'"));
            }
        }
        document.toggle_block(0, HtmlBlockMark::Indent(2)).unwrap();
        document.toggle_block(0, HtmlBlockMark::Indent(2)).unwrap();
        assert_eq!(document.blocks()[0].indent, 2);
        assert_eq!(html_to_plain_text(document.source()), "A\nB");
    }

    #[test]
    fn quote_toggle_isolates_selected_sibling_and_retains_other_block_styles() {
        let mut document = HtmlDocument::parse("<blockquote data-quote='keep'><p class='ql-indent-2 vendor' style='color:red'><b>A</b></p><p>B</p></blockquote>");
        document.toggle_block(0, HtmlBlockMark::Blockquote).unwrap();
        assert!(!document.blocks()[0].quote);
        assert!(document.blocks()[1].quote);
        assert_eq!(document.blocks()[0].indent, 2);
        assert!(document.runs()[0].style.bold);
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
        assert!(document.source().contains("<div data-quote='keep'>"));
        document.toggle_block(0, HtmlBlockMark::Blockquote).unwrap();
        assert!(document.blocks()[0].quote && document.blocks()[1].quote);
        document.toggle_block(0, HtmlBlockMark::Blockquote).unwrap();
        assert!(!document.blocks()[0].quote && document.blocks()[1].quote);
        let mut document = HtmlDocument::parse("<blockquote class='vendor ql-indent-3' data-quote='keep' style='font-style:italic'>A</blockquote>");
        document.toggle_block(0, HtmlBlockMark::Blockquote).unwrap();
        assert_eq!(
            document.source(),
            "<div class='vendor ql-indent-3' data-quote='keep' style='font-style:italic'>A</div>"
        );
        assert!(!document.blocks()[0].quote && document.runs()[0].style.italic);
        assert_eq!(document.blocks()[0].indent, 3);
    }

    #[test]
    fn code_toggle_removes_only_quill_code_class_and_isolates_pre_ancestors() {
        let mut document = HtmlDocument::parse("<pre class='vendor ql-code-block ql-indent-3' data-code='keep' style='color:blue'>A</pre><p>B</p>");
        document.toggle_block(0, HtmlBlockMark::CodeBlock).unwrap();
        assert_eq!(
            document.source(),
            "<div class='vendor ql-indent-3' data-code='keep' style='color:blue'>A</div><p>B</p>"
        );
        assert!(!document.blocks()[0].code);
        assert_eq!(document.blocks()[0].indent, 3);
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("blue"));
        document.toggle_block(0, HtmlBlockMark::CodeBlock).unwrap();
        assert!(document.blocks()[0].code);
        document.toggle_block(0, HtmlBlockMark::CodeBlock).unwrap();
        assert!(!document.blocks()[0].code);
        let mut document = HtmlDocument::parse("<blockquote><ol><li class='ql-indent-2'><pre data-code='keep'><div class='ql-code-block vendor' data-item='A'>A</div><div>B</div></pre></li></ol></blockquote>");
        document.toggle_block(0, HtmlBlockMark::CodeBlock).unwrap();
        assert!(!document.blocks()[0].code && document.blocks()[1].code);
        assert!(document.blocks()[0].quote && document.blocks()[1].quote);
        assert_eq!(document.blocks()[0].list, Some(HtmlListKind::Ordered));
        assert_eq!(document.blocks()[0].indent, 2);
        assert!(document.source().contains("class='vendor' data-item='A'"));
        assert!(document.source().contains("data-code='keep'"));
    }

    #[test]
    fn list_toggle_preserves_siblings_unknown_attributes_indent_and_nested_header() {
        let mut document = HtmlDocument::parse("<ol data-list='keep'><li class='ql-indent-2 vendor' data-item='A'><h2 style='color:red'>A</h2></li><li>B</li></ol>");
        document
            .toggle_block(0, HtmlBlockMark::List(HtmlListKind::Ordered))
            .unwrap();
        assert_eq!(document.blocks()[0].list, None);
        assert_eq!(document.blocks()[1].list, Some(HtmlListKind::Ordered));
        assert_eq!(document.blocks()[0].kind, HtmlBlockKind::Header(2));
        assert_eq!(document.blocks()[0].indent, 2);
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
        assert!(document
            .source()
            .contains("class='ql-indent-2 vendor' data-item='A'"));
        assert!(document.source().contains("data-list='keep'"));
        document.toggle_block(0, HtmlBlockMark::Header(2)).unwrap();
        assert_eq!(document.blocks()[0].kind, HtmlBlockKind::Paragraph);
        assert_eq!(document.blocks()[1].list, Some(HtmlListKind::Ordered));
        assert_eq!(document.blocks()[0].indent, 2);
        assert_eq!(html_to_plain_text(document.source()), "A\nB");
    }

    #[test]
    fn clean_nonempty_selection_clears_its_line_formats_atomically_and_tracks_duplicate_run_ordinal(
    ) {
        let original = "<blockquote data-quote='keep'><ol data-list='keep'><li class='ql-indent-3 vendor' data-item='A'><h2 data-header='keep'><pre class='ql-code-block custom' data-code='keep'><b><i>aa</i></b></pre></h2></li><li class='ql-indent-1'><h3><pre><b>B</b></pre></h3></li></ol></blockquote>";
        let mut document = HtmlDocument::parse(original);
        let sibling_style = document.runs()[1].style.clone();
        let sibling = document.blocks()[1].clone();
        let anchor = document.clear_selection_format_anchor(0, 1..2).unwrap();
        let selected = document
            .runs()
            .iter()
            .find(|run| run.source_range == anchor)
            .unwrap();
        assert_eq!(selected.text, "a");
        assert!(!selected.style.bold && !selected.style.italic && !selected.style.code);
        let block = &document.blocks()[selected.block_index];
        assert_eq!(block.kind, HtmlBlockKind::Paragraph);
        assert_eq!(block.list, None);
        assert!(!block.quote && !block.code);
        assert_eq!(block.indent, 0);
        assert!(document.runs()[0].style.bold && document.runs()[0].style.italic);
        let other = document.runs().iter().find(|run| run.text == "B").unwrap();
        assert_eq!(other.style, sibling_style);
        let other_block = &document.blocks()[other.block_index];
        assert_eq!(other_block.kind, sibling.kind);
        assert_eq!(other_block.list, sibling.list);
        assert_eq!(other_block.indent, sibling.indent);
        assert_eq!(other_block.quote, sibling.quote);
        assert_eq!(other_block.code, sibling.code);
        for attribute in [
            "data-quote='keep'",
            "data-list='keep'",
            "data-item='A'",
            "data-header='keep'",
            "data-code='keep'",
        ] {
            assert!(document.source().contains(attribute));
        }
        document.replace_source_text_span(anchor, "X").unwrap();
        assert_eq!(html_to_plain_text(document.source()), "aX\nB");
    }

    #[test]
    fn malformed_or_shared_container_formatting_refuses_without_partial_mutation() {
        for original in [
            "<blockquote data-q='keep'><p><b>aa</b></p>",
            "<blockquote><p><b>aa</p></b></blockquote>",
            "<blockquote>aa<p>B</p>C</blockquote>",
        ] {
            let mut document = HtmlDocument::parse(original);
            assert_eq!(
                document.toggle_block(0, HtmlBlockMark::Blockquote),
                Err(HtmlEditError::UnsupportedBlockStructure)
            );
            assert_eq!(document.source(), original);
            assert_eq!(
                document.clear_selection_format_anchor(0, 1..2),
                Err(HtmlEditError::UnsupportedBlockStructure)
            );
            assert_eq!(document.source(), original);
        }
        let mut document = HtmlDocument::parse("<h2><b>aa</b></h2>");
        let anchor = document.clear_selection_format_anchor(0, 1..1).unwrap();
        assert_eq!(anchor, 8..8);
        assert_eq!(document.source(), "<h2><b>aa</b></h2>");
    }
    #[test]
    fn explicit_block_first_whitespace_and_replacement_keep_editable_source_runs() {
        for value in [
            " ",
            "\t",
            "\n",
            "\r\n",
            "\u{a0}",
            "\u{200d}",
            "👩\u{200d}💻",
        ] {
            let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
            let mut document = HtmlDocument::parse("<p data-vendor='keep'></p>");
            let anchor = document.append_paragraph(value);
            assert_eq!(document.runs().len(), 1, "first input {value:?}");
            assert_eq!(document.runs()[0].source_range, anchor);
            assert_eq!(document.runs()[0].text, normalized);
            let anchor = document
                .wrap_text_range_anchor(0, 0..normalized.len(), HtmlMark::Bold)
                .unwrap();
            assert!(document.runs()[0].style.bold);
            assert_eq!(document.runs()[0].source_range, anchor);
            assert_eq!(document.runs()[0].text, normalized);
            let mut document = HtmlDocument::parse("<p data-vendor='keep'>abc</p>");
            let anchor = document
                .replace_source_text_span(document.runs()[0].source_range.clone(), value)
                .unwrap();
            assert_eq!(document.runs().len(), 1, "replacement {value:?}");
            assert_eq!(document.runs()[0].source_range, anchor);
            assert_eq!(document.runs()[0].text, normalized);
            assert_eq!(
                document.source(),
                format!("<p data-vendor='keep'>{value}</p>")
            );
        }
    }

    #[test]
    fn document_indentation_stays_out_of_preview_but_nbsp_and_inline_block_spaces_remain() {
        let document = HtmlDocument::parse(
            "<!DOCTYPE html>\r\n<html>\n <body>\n  <p>A</p>\r\n  <p>B</p>\n </body>\n</html>\n",
        );
        assert_eq!(
            document
                .runs()
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert_eq!(document.blocks().len(), 2);
        let document = HtmlDocument::parse("<p><b> </b></p><div>\r\n</div><span>&nbsp;</span>");
        assert_eq!(
            document
                .runs()
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            [" ", "\n", "\u{a0}"]
        );
        assert!(document.runs()[0].style.bold);
        assert_eq!(
            document.source(),
            "<p><b> </b></p><div>\r\n</div><span>&nbsp;</span>"
        );
    }
    #[test]
    fn format_anchor_resolves_second_duplicate_text_without_a_text_match() {
        for toggle in [false, true] {
            let mut document = HtmlDocument::parse("<p>aa</p>");
            let anchor = if toggle {
                document
                    .toggle_text_range_anchor(0, 1..2, HtmlMark::Bold)
                    .unwrap()
            } else {
                document
                    .wrap_text_range_anchor(0, 1..2, HtmlMark::Bold)
                    .unwrap()
            };
            assert_eq!(document.source(), "<p>a<strong>a</strong></p>");
            assert_eq!(anchor, 12..13);
            let selected = document
                .runs()
                .iter()
                .find(|run| run.source_range == anchor)
                .unwrap();
            assert!(selected.style.bold);
            assert_eq!(selected.text, "a");
            assert!(!document.runs()[0].style.bold);
            document.replace_source_text_span(anchor, "b").unwrap();
            assert_eq!(document.source(), "<p>a<strong>b</strong></p>");
        }
    }

    #[test]
    fn cancelled_mark_anchor_accounts_for_neutralized_ancestor_and_reset_markup() {
        let mut document = HtmlDocument::parse("<p><b data-v='same' style='color:red'>aa</b></p>");
        let before = "<p><b data-v='same' style='color:red'>a</b><span data-v='same' style='color:red'><span style=\"font-weight:normal\">";
        let after = "</span></span><b data-v='same' style='color:red'></b></p>";
        let anchor = document
            .toggle_text_range_anchor(0, 1..2, HtmlMark::Bold)
            .unwrap();
        assert_eq!(document.source(), format!("{before}a{after}"));
        assert_eq!(anchor, before.len()..before.len() + 1);
        let selected = document
            .runs()
            .iter()
            .find(|run| run.source_range == anchor)
            .unwrap();
        assert!(!selected.style.bold);
        assert_eq!(selected.style.color.as_deref(), Some("red"));
        assert!(document.runs()[0].style.bold);
        document.replace_source_text_span(anchor, "b").unwrap();
        assert_eq!(document.source(), format!("{before}b{after}"));
        assert_eq!(html_to_plain_text(document.source()), "ab");
    }

    #[test]
    fn cleared_format_anchor_resolves_second_duplicate_and_retains_entity_source_bytes() {
        let mut document = HtmlDocument::parse("<p><b><i>a&#x61;a</i></b></p>");
        let before = "<p><b><i>a</i></b><span style=\"font-weight:normal;font-style:normal;text-decoration:none;color:initial;background-color:transparent;vertical-align:baseline;font-family:initial\">";
        let anchor = document.clear_text_format_range_anchor(0, 1..2).unwrap();
        assert_eq!(anchor, before.len()..before.len() + "&#x61;".len());
        assert_eq!(&document.source()[anchor.clone()], "&#x61;");
        let selected = document
            .runs()
            .iter()
            .find(|run| run.source_range == anchor)
            .unwrap();
        assert!(!selected.style.bold && !selected.style.italic);
        assert_eq!(selected.text, "a");
        assert!(document.runs()[0].style.bold && document.runs()[2].style.bold);
        document.replace_source_text_span(anchor, "X").unwrap();
        assert_eq!(html_to_plain_text(document.source()), "aXa");
    }

    #[test]
    fn returned_format_anchors_cover_only_selected_multi_character_entity_portion() {
        let mut document = HtmlDocument::parse("<p><b>&NotEqualTilde;</b></p>");
        let anchor = document
            .toggle_text_range_anchor(0, "≂".len().."≂\u{338}".len(), HtmlMark::Bold)
            .unwrap();
        assert_eq!(&document.source()[anchor.clone()], "\u{338}");
        let selected = document
            .runs()
            .iter()
            .find(|run| run.source_range == anchor)
            .unwrap();
        assert!(!selected.style.bold);
        assert!(document.runs()[0].style.bold);
        document.replace_source_text_span(anchor, "X").unwrap();
        assert_eq!(html_to_plain_text(document.source()), "≂X");
        let mut document = HtmlDocument::parse("<p>&NotEqualTilde;</p>");
        let anchor = document
            .wrap_text_range_anchor(0, 0.."≂".len(), HtmlMark::Italic)
            .unwrap();
        assert_eq!(&document.source()[anchor.clone()], "≂");
        assert!(
            document
                .runs()
                .iter()
                .find(|run| run.source_range == anchor)
                .unwrap()
                .style
                .italic
        );
        assert!(!document.runs()[1].style.italic);
    }

    #[test]
    fn empty_toggle_anchor_is_existing_source_caret_and_does_not_create_markup() {
        let mut document = HtmlDocument::parse("<p>aa</p>");
        let anchor = document
            .toggle_text_range_anchor(0, 1..1, HtmlMark::Bold)
            .unwrap();
        assert_eq!(anchor, 4..4);
        assert_eq!(document.source(), "<p>aa</p>");
        let mut document = HtmlDocument::parse("<p>&NotEqualTilde;</p>");
        let anchor = document
            .toggle_text_range_anchor(0, "≂".len().."≂".len(), HtmlMark::Italic)
            .unwrap();
        assert_eq!(anchor, 3..3);
        assert_eq!(document.source(), "<p>&NotEqualTilde;</p>");
    }
    #[test]
    fn toolbar_boolean_toggles_cancel_only_the_selected_property_and_keep_other_marks() {
        let original = "<p data-block='keep'><strong data-vendor='one'><em><u><s><span style='color:#ff0000;background-color:#00ff00'><sub><code data-code='yes'><a href='https://example.test' title='keep'>前中&#x1F600;后</a></code></sub></span></s></u></em></strong></p><!--tail-->";
        for mark in [
            HtmlMark::Bold,
            HtmlMark::Italic,
            HtmlMark::Underline,
            HtmlMark::Strike,
            HtmlMark::Subscript,
            HtmlMark::Code,
        ] {
            let mut document = HtmlDocument::parse(original);
            let mut expected = document.runs()[0].style.clone();
            match mark {
                HtmlMark::Bold => expected.bold = false,
                HtmlMark::Italic => expected.italic = false,
                HtmlMark::Underline => expected.underline = false,
                HtmlMark::Strike => expected.strike = false,
                HtmlMark::Subscript => expected.subscript = false,
                HtmlMark::Code => expected.code = false,
                _ => unreachable!(),
            }
            let selected = "前".len().."前中😀".len();
            let (changed_range, _, _, _) = document.selection_source(0, selected.clone()).unwrap();
            document.toggle_text_range(0, selected, mark).unwrap();
            let selected = document
                .runs()
                .iter()
                .find(|run| run.text == "中😀")
                .unwrap();
            assert_eq!(selected.style, expected);
            assert!(document
                .source()
                .starts_with(&original[..changed_range.start]));
            assert!(document.source().ends_with(&original[changed_range.end..]));
            assert!(document.source().contains("data-vendor='one'"));
            assert!(document.source().contains("data-code='yes'"));
            assert!(document.source().contains("title='keep'"));
            for side in ["前", "后"] {
                let run = document.runs().iter().find(|run| run.text == side).unwrap();
                assert_eq!(run.style, HtmlDocument::parse(original).runs()[0].style);
            }
            assert_eq!(html_to_plain_text(document.source()), "前中😀后");
        }
        let mut document =
            HtmlDocument::parse("<p><b><sup style='color:red' data-vendor='keep'>2</sup></b></p>");
        document.toggle_text(0, HtmlMark::Superscript).unwrap();
        assert!(!document.runs()[0].style.superscript);
        assert!(document.runs()[0].style.bold);
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
        assert!(document.source().contains("data-vendor='keep'"));
    }

    #[test]
    fn underline_and_strike_toggles_keep_each_other_css_and_unknown_attributes() {
        let original = "<p><span data-vendor='keep' style=\"font-weight:700 !important;color:red;background:url('keep.png');text-decoration:underline line-through overline dotted red !important;letter-spacing:2px\">selected</span></p>";
        let mut document = HtmlDocument::parse(original);
        assert!(
            document.runs()[0].style.bold
                && document.runs()[0].style.underline
                && document.runs()[0].style.strike
        );
        document.toggle_text(0, HtmlMark::Underline).unwrap();
        assert!(!document.runs()[0].style.underline);
        assert!(document.runs()[0].style.strike && document.runs()[0].style.bold);
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
        assert!(document
            .source()
            .contains("background:url(&#39;keep.png&#39;)"));
        assert!(document.source().contains("letter-spacing:2px"));
        assert!(document.source().contains("overline dotted red !important"));
        document.toggle_text(0, HtmlMark::Strike).unwrap();
        assert!(!document.runs()[0].style.strike && !document.runs()[0].style.underline);
        assert!(document.runs()[0].style.bold);
        assert!(document.source().contains("data-vendor='keep'"));
    }

    #[test]
    fn color_background_and_link_toggles_cancel_only_matching_values() {
        let original = "<p><b><font color='#AABBCC' face='vendor'><span style='background-color:#ddeeff;letter-spacing:3px'><a href='https://example.test/a?a=1&amp;b=2' title='keep' target='_blank' data-vendor='yes'>text</a></span></font></b></p>";
        for mark in [
            HtmlMark::Color("#aabbcc".into()),
            HtmlMark::Background("#DDEEFF".into()),
            HtmlMark::Link("https://example.test/a?a=1&b=2".into()),
        ] {
            let mut document = HtmlDocument::parse(original);
            let mut expected = document.runs()[0].style.clone();
            match &mark {
                HtmlMark::Color(_) => expected.color = None,
                HtmlMark::Background(_) => expected.background = None,
                HtmlMark::Link(_) => expected.link = None,
                _ => unreachable!(),
            }
            document.toggle_text(0, mark).unwrap();
            assert_eq!(document.runs()[0].style, expected);
            assert!(document.source().contains("face='vendor'"));
            assert!(document.source().contains("letter-spacing:3px"));
            assert!(document
                .source()
                .contains("title='keep' target='_blank' data-vendor='yes'"));
        }
        let mut document = HtmlDocument::parse(original);
        document
            .toggle_text(0, HtmlMark::Color("blue".into()))
            .unwrap();
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("blue"));
        assert!(document.runs()[0].style.bold && document.runs()[0].style.link.is_some());
        document
            .toggle_text(0, HtmlMark::Color("BLUE".into()))
            .unwrap();
        assert_eq!(document.runs()[0].style.color, None);
        assert!(document.runs()[0].style.bold && document.runs()[0].style.link.is_some());
    }

    #[test]
    fn toolbar_toggle_applies_inactive_mark_and_entity_partial_selection_remains_unicode_safe() {
        let mut document =
            HtmlDocument::parse("<p><i><b data-keep='yes'>&NotEqualTilde;</b></i></p>");
        document
            .toggle_text_range(0, "≂".len().."≂\u{338}".len(), HtmlMark::Bold)
            .unwrap();
        let selected = document
            .runs()
            .iter()
            .find(|run| run.text == "\u{338}")
            .unwrap();
        assert!(!selected.style.bold && selected.style.italic);
        assert!(document.runs()[0].style.bold);
        let index = document
            .runs()
            .iter()
            .position(|run| run.text == "\u{338}")
            .unwrap();
        document.toggle_text(index, HtmlMark::Underline).unwrap();
        let selected = document
            .runs()
            .iter()
            .find(|run| run.text == "\u{338}")
            .unwrap();
        assert!(selected.style.underline && selected.style.italic && !selected.style.bold);
        assert_eq!(html_to_plain_text(document.source()), "≂\u{338}");
    }

    #[test]
    fn toolbar_empty_selection_is_noop_and_bad_ranges_or_values_keep_source() {
        let mut document = HtmlDocument::parse("<p><b>中😀</b></p>");
        let original = document.source().to_string();
        document.toggle_text_range(0, 3..3, HtmlMark::Bold).unwrap();
        assert_eq!(document.source(), original);
        assert_eq!(
            document.toggle_text_range(0, 1..3, HtmlMark::Bold),
            Err(HtmlEditError::InvalidTextRange)
        );
        assert_eq!(
            document.toggle_text(0, HtmlMark::Link("javascript:x".into())),
            Err(HtmlEditError::UnsafeReference)
        );
        assert_eq!(
            document.toggle_text(0, HtmlMark::Color("red;bad:x".into())),
            Err(HtmlEditError::InvalidColor)
        );
        assert_eq!(document.source(), original);
    }

    #[test]
    fn script_toolbar_uses_one_exclusive_attribute_and_retains_unrelated_markup() {
        // Quill's Script blot is one `script` attribute with sub/super values.
        // Applying the other value replaces the effective value; cancelling
        // the active value returns to baseline without restoring an older one.
        for original in [
            "<p><sup data-vendor='keep'><b style='color:red !important'>x</b></sup></p>",
            "<p><sub data-vendor='keep'><b style='color:red !important'>x</b></sub></p>",
        ] {
            let mut document = HtmlDocument::parse(original);
            document.toggle_text(0, HtmlMark::Subscript).unwrap();
            let after_first = document.runs()[0].style.clone();
            if original.contains("<sup") {
                assert!(after_first.subscript && !after_first.superscript);
            } else {
                assert!(!after_first.subscript && !after_first.superscript);
            }
            document.toggle_text(0, HtmlMark::Superscript).unwrap();
            assert!(document.runs()[0].style.superscript && !document.runs()[0].style.subscript);
            document.toggle_text(0, HtmlMark::Superscript).unwrap();
            assert!(!document.runs()[0].style.superscript && !document.runs()[0].style.subscript);
            assert!(document.runs()[0].style.bold);
            assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
            assert!(document.source().contains("data-vendor='keep'"));
            assert!(document.source().contains("color:red !important"));
        }
    }

    #[test]
    fn append_paragraph_preserves_source_and_empty_input_anchor_becomes_a_visible_run() {
        for original in [
            "",
            "<html><head><title>title</title></head><body></body></html>",
            "<html><body><script>const s='</body>';</script></body></html>",
            "<html><body><script>unclosed",
            "<head><title>unclosed",
            "<template>unclosed",
            "<!--unclosed",
        ] {
            let mut document = HtmlDocument::parse(original);
            let at = paragraph_insertion_point(original);
            let anchor = document.append_paragraph("");
            assert_eq!(
                document.source(),
                format!("{}<p></p>{}", &original[..at], &original[at..])
            );
            assert!(anchor.is_empty());
            let anchor = document.replace_source_text_span(anchor, "新<&😀").unwrap();
            assert_eq!(&document.source()[anchor], "新&lt;&amp;😀");
            assert!(document.runs().iter().any(|run| run.text == "新<&😀"));
        }
        let mut document = HtmlDocument::parse("<html><body><p>old</p><!--keep--></body></html>");
        let anchor = document.append_paragraph("new");
        assert_eq!(
            document.source(),
            "<html><body><p>old</p><!--keep--><p>new</p></body></html>"
        );
        assert_eq!(&document.source()[anchor], "new");
        assert_eq!(document.runs().len(), 2);
    }

    #[test]
    fn empty_quill_paragraph_is_reused_with_attrs_and_complex_empty_markup_is_retained() {
        let mut document = HtmlDocument::parse("<html><body><P data-vendor='keep' style='color:red'> \r\n<BR/> \n</P><!--tail--></body></html>");
        let anchor = document.append_paragraph("中<&");
        assert_eq!(document.source(), "<html><body><P data-vendor='keep' style='color:red'>中&lt;&amp;</P><!--tail--></body></html>");
        assert_eq!(&document.source()[anchor], "中&lt;&amp;");
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
        let mut document = HtmlDocument::parse("<p data-x='keep'></p>");
        let anchor = document.append_paragraph("");
        assert_eq!(document.source(), "<p data-x='keep'></p>");
        document.replace_source_text_span(anchor, "new").unwrap();
        assert_eq!(document.source(), "<p data-x='keep'>new</p>");
        for original in [
            "<p><!--keep--></p>",
            "<p><br data-vendor='keep'></p>",
            "<p></p><p></p>",
        ] {
            let mut document = HtmlDocument::parse(original);
            document.append_paragraph("new");
            assert_eq!(document.source(), format!("{original}<p>new</p>"));
        }
    }
    #[test]
    fn rich_document_preserves_unknown_markup_comments_attributes_and_entity_spelling() {
        let original = "<!DOCTYPE HTML><HTML><head><title>head</title></head><body><P data-x='1>0'><unknown a='b'>原&#x1F600;文</unknown><!-- vendor --><IMG SRC='file:///C:/图.png' ALT='A&amp;B' data-qq='123'></P></body></HTML>";
        let mut document = HtmlDocument::parse(original);
        assert_eq!(document.source(), original);
        assert_eq!(document.runs()[0].text, "原😀文");
        assert_eq!(document.images()[0].alt, "A&B");
        let range = document.runs()[0].source_range.clone();
        document
            .replace_text_range(0, 0.."原".len(), "新<&")
            .unwrap();
        assert_eq!(
            document.source(),
            format!(
                "{}新&lt;&amp;{}",
                &original[..range.start],
                &original[range.start + "原".len()..]
            )
        );
        assert!(document.opaque().iter().any(|node| node.tag == "unknown"));
    }

    #[test]
    fn rich_projection_covers_quill_formats_and_css_resets() {
        let document = HtmlDocument::parse("<blockquote><ol><li class='ql-indent-2'><h2><strong><em><u><s><span style='color:#123456;background-color:rgb(1, 2, 3)'><sub><a href='https://example.test'>文</a></sub></span></s></u></em></strong></h2></li></ol></blockquote><pre class='ql-syntax'>code</pre><p><b><span style='font-weight:normal;font-style:normal;text-decoration:none;vertical-align:baseline'>normal</span></b><sup>2</sup></p>");
        let run = &document.runs()[0];
        assert!(run.style.bold && run.style.italic && run.style.underline && run.style.strike);
        assert!(run.style.subscript && !run.style.superscript);
        assert_eq!(run.style.color.as_deref(), Some("#123456"));
        assert_eq!(run.style.background.as_deref(), Some("rgb(1, 2, 3)"));
        assert_eq!(run.style.link.as_deref(), Some("https://example.test"));
        let block = &document.blocks()[run.block_index];
        assert_eq!(block.kind, HtmlBlockKind::Header(2));
        assert_eq!(block.list, Some(HtmlListKind::Ordered));
        assert_eq!(block.indent, 2);
        assert!(block.quote);
        assert!(document.runs()[1].style.code);
        assert_eq!(
            document.blocks()[document.runs()[1].block_index].kind,
            HtmlBlockKind::Code
        );
        assert!(!document.runs()[2].style.bold);
        assert!(document.runs()[3].style.superscript);
    }

    #[test]
    fn css_important_projection_prioritizes_color_without_discarding_earlier_valid_values() {
        for (css, expected) in [
            ("color:red!important;color:blue", Some("red")),
            ("color:blue;color:red!important", Some("red")),
            ("color:red !IMPORTANT;color:blue !important", Some("blue")),
            ("color:red!important;color:??? !important", Some("red")),
            ("color:red;color:??? !important", Some("red")),
            ("color:red!important;color:initial", Some("red")),
            ("color:red;color:initial!important", None),
        ] {
            let source = format!(
                "<!--keep--><p vendor='keep'><span style='{css}' data-u='keep'>A</span>B</p>"
            );
            let document = HtmlDocument::parse(&source);
            assert_eq!(document.runs()[0].style.color.as_deref(), expected, "{css}");
            assert_eq!(document.source(), source);
            assert_eq!(document.canvas_projection().text, "AB\n");
        }
    }

    #[test]
    fn css_important_projection_keeps_priority_across_supported_style_families() {
        let source = "<pre vendor='keep'><span style='font-weight:700 !important;font-weight:normal;font-style:italic !important;font-style:normal;text-decoration:underline line-through !important;text-decoration:none;vertical-align:super !important;vertical-align:baseline;color:#123 !important;color:#456;background-color:red !important;background:blue;font-family:initial !important;font-family:other' data-u='keep'>A</span></pre>";
        let document = HtmlDocument::parse(source);
        let style = &document.runs()[0].style;
        assert!(style.bold && style.italic && style.underline && style.strike);
        assert!(style.superscript && !style.subscript);
        assert_eq!(style.color.as_deref(), Some("#123"));
        assert_eq!(style.background.as_deref(), Some("red"));
        assert!(!style.code);
        assert!(document.blocks()[0].code);
        assert_eq!(document.source(), source);

        let source = "<b><i><u><s><sup style='font-weight:normal !important;font-weight:700;font-style:normal !important;font-style:italic;text-decoration:none !important;text-decoration:underline line-through;vertical-align:baseline !important;vertical-align:sub;background:transparent !important;background-color:red'>A</sup></s></u></i></b>";
        let document = HtmlDocument::parse(source);
        let style = &document.runs()[0].style;
        assert!(!style.bold && !style.italic && !style.underline && !style.strike);
        assert!(!style.superscript && !style.subscript);
        assert_eq!(style.background, None);
        assert_eq!(document.source(), source);
    }

    #[test]
    fn css_important_projection_retains_group_order_and_per_property_validation() {
        let source = "<p><span style='font-weight:normal !important;font-weight:700 !important;font-weight:??? !important;font-style:italic !important;font-style:??? !important;text-decoration:underline !important;text-decoration:??? !important;vertical-align:sub !important;vertical-align:??? !important;background:red !important;background-color:??? !important'>A</span></p>";
        let document = HtmlDocument::parse(source);
        let style = &document.runs()[0].style;
        assert!(style.bold && style.italic && style.underline);
        assert!(style.subscript && !style.superscript);
        assert_eq!(style.background.as_deref(), Some("red"));
        assert_eq!(document.source(), source);
    }

    #[test]
    fn css_important_projection_does_not_override_a_descendants_explicit_value() {
        let source = "<p style='color:red !important;font-weight:700 !important' vendor='keep'>A<span style='color:blue;font-weight:normal' data-u='keep'>B</span><span style='color:??? !important'>C</span></p>";
        let mut document = HtmlDocument::parse(source);
        assert_eq!(document.runs()[0].style.color.as_deref(), Some("red"));
        assert!(document.runs()[0].style.bold);
        assert_eq!(document.runs()[1].style.color.as_deref(), Some("blue"));
        assert!(!document.runs()[1].style.bold);
        assert_eq!(document.runs()[2].style.color.as_deref(), Some("red"));
        assert!(document.runs()[2].style.bold);
        assert_eq!(document.source(), source);
        document.replace_text(1, "D").unwrap();
        assert_eq!(document.source(), source.replace(">B</span>", ">D</span>"));
    }

    #[test]
    fn active_markup_and_raw_text_cannot_become_visible_text_or_image_requests() {
        let original = "<head><title>skip</title></head><script>var img = '<img src=network>'; '</scriptish>';</script><style>p{background:url(x)}</style><template><p>hidden<img src='remote'></p></template><svg><foreignObject><p>hidden</p></foreignObject></svg><iframe>hidden</iframe><p>visible<img src='https://example.test/a'></p>";
        let document = HtmlDocument::parse(original);
        assert_eq!(document.source(), original);
        assert_eq!(
            document
                .runs()
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            "visible"
        );
        assert_eq!(document.images().len(), 1);
        assert_eq!(document.images()[0].reference(), HtmlImageReference::Remote);
        assert!(document.opaque().iter().any(|node| node.tag == "script"));
        assert!(document.opaque().iter().any(|node| node.tag == "template"));
    }

    #[test]
    fn source_span_survives_empty_input_and_escapes_new_markup() {
        let mut document = HtmlDocument::parse("<p data-original='1'>中😀</p><!--keep-->");
        let anchor = document.runs()[0].source_range.clone();
        let anchor = document.replace_source_text_span(anchor, "").unwrap();
        assert!(document.runs().is_empty());
        assert_eq!(document.source(), "<p data-original='1'></p><!--keep-->");
        let anchor = document.replace_source_text_span(anchor, "新<&😀").unwrap();
        assert_eq!(&document.source()[anchor], "新&lt;&amp;😀");
        assert_eq!(document.runs()[0].text, "新<&😀");
        assert_eq!(
            document.source(),
            "<p data-original='1'>新&lt;&amp;😀</p><!--keep-->"
        );
        let source = document.source().to_string();
        assert_eq!(
            document.replace_source_text_span(1..1, "bad"),
            Err(HtmlEditError::InvalidSourceRange)
        );
        assert_eq!(document.source(), source);
    }

    #[test]
    fn wrapping_a_selection_keeps_selected_entity_bytes_and_unselected_content() {
        let mut document = HtmlDocument::parse("<p data-x='y'>A&#x1F600;B &amp; C</p>");
        document.wrap_text_range(0, 1..5, HtmlMark::Bold).unwrap();
        assert_eq!(
            document.source(),
            "<p data-x='y'>A<strong>&#x1F600;</strong>B &amp; C</p>"
        );
        assert!(document.runs()[1].style.bold);
        let range = document.runs()[2].text.find('&').unwrap();
        document
            .replace_text_range(2, range..range + 1, "<new>")
            .unwrap();
        assert_eq!(
            document.source(),
            "<p data-x='y'>A<strong>&#x1F600;</strong>B &lt;new&gt; C</p>"
        );
        let before = document.source().to_string();
        assert_eq!(
            document.replace_text_range(1, 1..2, "bad"),
            Err(HtmlEditError::InvalidTextRange)
        );
        assert_eq!(document.source(), before);
    }

    #[test]
    fn partial_multi_character_entity_selection_keeps_decoded_remainder() {
        let mut document = HtmlDocument::parse("<p>&NotEqualTilde; &#128512;</p>");
        assert_eq!(document.runs()[0].text, "≂\u{338} 😀");
        document.replace_text_range(0, 0.."≂".len(), "X").unwrap();
        assert_eq!(document.source(), "<p>X\u{338} &#128512;</p>");
        let mut document = HtmlDocument::parse("<p>&NotEqualTilde;</p>");
        document
            .wrap_text_range(0, "≂".len().."≂\u{338}".len(), HtmlMark::Underline)
            .unwrap();
        assert_eq!(document.source(), "<p>≂<u>\u{338}</u></p>");
    }

    #[test]
    fn large_literal_run_uses_one_source_segment_and_utf8_selection_stays_local() {
        let literal = "中😀abc".repeat(10_000);
        let mut document = HtmlDocument::parse(format!("<p>{literal}</p>"));
        assert_eq!(document.segments[0].len(), 1);
        document.replace_text_range(0, 3..7, "x").unwrap();
        assert_eq!(document.source(), format!("<p>中x{}</p>", &literal[7..]));
        document.wrap_text_range(0, 3..4, HtmlMark::Italic).unwrap();
        assert_eq!(
            document.source(),
            format!("<p>中<em>x</em>{}</p>", &literal[7..])
        );
    }

    #[test]
    fn clear_isolates_only_selected_text_from_inline_ancestors() {
        let mut document = HtmlDocument::parse("<p data-keep='yes'><b>before<a href='https://example.test'><i>selected</i></a>after</b></p>");
        document.clear_text_format(1).unwrap();
        let selected = document
            .runs()
            .iter()
            .find(|run| run.text == "selected")
            .unwrap();
        assert!(!selected.style.bold && !selected.style.italic && selected.style.link.is_none());
        let before = document
            .runs()
            .iter()
            .find(|run| run.text == "before")
            .unwrap();
        let after = document
            .runs()
            .iter()
            .find(|run| run.text == "after")
            .unwrap();
        assert!(before.style.bold && after.style.bold);
        assert!(document.source().contains("<p data-keep='yes'>"));
        assert!(document
            .source()
            .contains("<a href='https://example.test'><i>"));
    }

    #[test]
    fn block_format_retains_original_block_attributes_and_children() {
        let mut document = HtmlDocument::parse(
            "<P data-vendor='keep' style='color:red'>A<img src='x'>B</P><!--tail-->",
        );
        document.format_block(0, HtmlBlockMark::Header(3)).unwrap();
        assert_eq!(
            document.source(),
            "<h3 data-vendor='keep' style='color:red'>A<img src='x'>B</h3><!--tail-->"
        );
        assert_eq!(document.blocks()[0].kind, HtmlBlockKind::Header(3));
        document.format_block(0, HtmlBlockMark::Indent(4)).unwrap();
        assert_eq!(document.blocks()[0].indent, 4);
        document
            .format_block(0, HtmlBlockMark::List(HtmlListKind::Bullet))
            .unwrap();
        assert_eq!(document.blocks()[0].list, Some(HtmlListKind::Bullet));
        assert_eq!(document.images()[0].src, "x");
        let source = document.source().to_string();
        assert_eq!(
            document.format_block(0, HtmlBlockMark::Header(0)),
            Err(HtmlEditError::InvalidHeader)
        );
        assert_eq!(document.source(), source);
    }

    #[test]
    fn editing_one_list_item_splits_list_without_changing_sibling_kind_or_vendor_attrs() {
        let mut document = HtmlDocument::parse("<ul data-vendor='keep'><li data-item='A' class='extra ql-indent-2'>A</li><li>B</li></ul><!--tail-->");
        document.format_block(0, HtmlBlockMark::Indent(3)).unwrap();
        assert_eq!(document.source(), "<ul data-vendor='keep'><li data-item='A' class='extra ql-indent-3'>A</li><li>B</li></ul><!--tail-->");
        document
            .format_block(0, HtmlBlockMark::List(HtmlListKind::Ordered))
            .unwrap();
        assert_eq!(document.source(), "<ul data-vendor='keep'></ul><ol data-vendor='keep'><li data-item='A' class='extra ql-indent-3'>A</li></ol><ul data-vendor='keep'><li>B</li></ul><!--tail-->");
        assert_eq!(document.blocks()[0].list, Some(HtmlListKind::Ordered));
        assert_eq!(document.blocks()[1].list, Some(HtmlListKind::Bullet));
        assert_eq!(html_to_plain_text(document.source()), "A\nB");
        document.format_block(0, HtmlBlockMark::Header(2)).unwrap();
        let a = document.runs().iter().find(|run| run.text == "A").unwrap();
        let b = document.runs().iter().find(|run| run.text == "B").unwrap();
        assert_eq!(
            document.blocks()[a.block_index].kind,
            HtmlBlockKind::Header(2)
        );
        assert_eq!(document.blocks()[a.block_index].list, None);
        assert_eq!(
            document.blocks()[b.block_index].list,
            Some(HtmlListKind::Bullet)
        );
        assert!(document
            .source()
            .contains("<h2 data-item='A' class='extra ql-indent-3'>A</h2>"));
        assert_eq!(html_to_plain_text(document.source()), "A\nB");
    }

    #[test]
    fn image_mutation_preserves_other_attributes_and_quotes() {
        let mut document = HtmlDocument::parse(
            "<p>A<IMG SRC='old?a=1&amp;b=2' ALT='keep' WIDTH=80 data-vendor='>'>B</p>",
        );
        assert_eq!(document.images()[0].src, "old?a=1&b=2");
        assert_eq!(document.images()[0].width, Some(80));
        document
            .replace_image_reference(0, "file:///C:/图'&.png")
            .unwrap();
        assert_eq!(
            document.source(),
            "<p>A<IMG SRC='file:///C:/图&#39;&amp;.png' ALT='keep' WIDTH=80 data-vendor='>'>B</p>"
        );
        assert_eq!(document.images()[0].reference(), HtmlImageReference::Local);
        let mut document =
            HtmlDocument::parse("<p>A<img src=old alt=keep/>B<img alt='next' /></p>");
        assert_eq!(document.images()[0].alt, "keep/");
        document.replace_image_reference(0, "new path").unwrap();
        document.replace_image_reference(1, "next").unwrap();
        assert_eq!(
            document.source(),
            "<p>A<img src=\"new path\" alt=keep/>B<img alt='next'  src=\"next\"/></p>"
        );
        document.remove_image(1).unwrap();
        assert_eq!(document.images().len(), 1);
        document
            .insert_image_after_run(0, "relative.png", "new <alt>")
            .unwrap();
        assert_eq!(document.images()[0].alt, "new <alt>");
    }

    #[test]
    fn inert_color_and_reference_validation_rejects_injected_markup_without_mutation() {
        let mut document = HtmlDocument::parse("<p>A<img src='keep'></p>");
        let source = document.source().to_string();
        assert_eq!(
            document.wrap_text(0, HtmlMark::Color("red;position:absolute".into())),
            Err(HtmlEditError::InvalidColor)
        );
        assert_eq!(
            document.wrap_text(0, HtmlMark::Link("javascript:alert(1)".into())),
            Err(HtmlEditError::UnsafeReference)
        );
        assert_eq!(
            document.replace_image_reference(0, "data:image/svg+xml,<svg/>"),
            Err(HtmlEditError::UnsafeReference)
        );
        assert_eq!(
            document.replace_image_reference(0, "java\nscript:alert(1)"),
            Err(HtmlEditError::UnsafeReference)
        );
        assert_eq!(document.source(), source);
        document
            .wrap_text(0, HtmlMark::Background("#aabbcc".into()))
            .unwrap();
        assert_eq!(
            document.runs()[0].style.background.as_deref(),
            Some("#aabbcc")
        );
    }

    #[test]
    fn attribute_character_references_use_html5_attribute_context() {
        let document = HtmlDocument::parse(
            "<p title='unknown'><img src='x?x=&notit;&amp;y=1' alt='中&#x1F600;'></p>",
        );
        assert_eq!(document.images()[0].src, "x?x=&notit;&y=1");
        assert_eq!(document.images()[0].alt, "中😀");
        let document = HtmlDocument::parse("<p>&notit;</p>");
        assert_eq!(document.runs()[0].text, "¬it;");
    }

    #[test]
    fn block_element_sequence_keeps_images_breaks_and_rules_in_order() {
        let document = HtmlDocument::parse("<p>A<img src='x'><br>B</p><hr><p>C</p>");
        assert_eq!(
            document.blocks()[0].elements,
            vec![
                HtmlElement::Text(0),
                HtmlElement::Image(0),
                HtmlElement::Break,
                HtmlElement::Text(1)
            ]
        );
        assert_eq!(
            document.blocks()[1].elements,
            vec![HtmlElement::HorizontalRule]
        );
        assert_eq!(document.blocks()[2].elements, vec![HtmlElement::Text(2)]);
    }
    #[test]
    fn text_projection_matches_web_blocks_cells_pre_alt_and_entities() {
        assert_eq!(html_to_plain_text("<div>中 &amp; 😀</div><table><tr><td>A</td><td>B</td></tr></table><pre>  x\n    y</pre><img alt=' picture '><script>ignored()</script>"), "中 & 😀\nA\tB\n  x\n    y\n[picture]");
        assert_eq!(
            html_to_plain_text("<p>one<br>two</p><!--keep source--><div>  leading   middle </div>"),
            "one\ntwo\n  leading middle"
        );
    }
    #[test]
    fn html5_parser_handles_quoted_angles_and_unsafe_elements() {
        assert_eq!(html_to_plain_text("<div title='a > b'>x</div><svg><text>skip</text></svg><style>skip</style><hr><p>&#x4E2D;&#128512;</p>"), "x\n---\n中😀");
    }
    #[test]
    fn table_reparenting_and_adjacent_inline_nodes_preserve_text() {
        assert_eq!(
            html_to_plain_text("<p>A<b>B</b>C</p><div><span>D</span><span>E</span></div>"),
            "ABC\nDE"
        );
    }

    #[test]
    fn entity_tokens_merge_but_real_inline_node_boundaries_keep_web_whitespace_rules() {
        assert_eq!(
            html_to_plain_text("<p>甲 &amp; 乙 &#x1F600; 丙</p>"),
            "甲 & 乙 😀 丙"
        );
        // frontend normalizes and trimEnds each DOM text node individually.
        // A space before a real inline element is consequently removed; the
        // leading space after that element remains. Entity token boundaries
        // must not introduce extra text nodes or remove their internal spaces.
        assert_eq!(
            html_to_plain_text("<p>A <b>B</b> C &amp; D</p>"),
            "AB C & D"
        );
        assert_eq!(
            html_to_plain_text("<pre>  &amp;  &#x4E2D;\n a</pre>"),
            "  &  中\n a"
        );
        // Foster-parented character tokens use append_before_sibling, so
        // they need the same adjacent-text merging as ordinary append.
        assert_eq!(
            html_to_plain_text("<table>中 &amp; 😀<tr><td>cell</td></tr></table>"),
            "中 & 😀cell"
        );
    }
}
