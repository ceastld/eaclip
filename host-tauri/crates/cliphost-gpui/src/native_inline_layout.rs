//! Pure continuous inline flow for compact QQ/IM list previews.
//!
//! The caller supplies CSS-normal whitespace and a shaped-text measurement
//! callback. This module does not read images, initialize GPUI or change payloads.
//! A source segment is metadata, never a forced line/shaping boundary. Paint
//! touching text fragments with the same style as one shaped run, especially
//! when a grapheme or a ligature crosses two source segments.

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, PartialEq)]
pub enum InlineAtom {
    Text {
        source: usize,
        text: String,
    },
    /// Width includes the object's horizontal margins; height is its outer box.
    Object {
        source: usize,
        width: f32,
        height: f32,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InlineLayout {
    /// Requested content width. An oversized atomic object can overflow it.
    pub width: f32,
    pub height: f32,
    pub lines: Vec<InlineLine>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InlineLine {
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub fragments: Vec<PlacedInline>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlacedInline {
    Text(PlacedText),
    Object(PlacedObject),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlacedText {
    pub source: usize,
    /// UTF-8 byte range within this source Text atom, not the joined virtual flow.
    pub range: Range<usize>,
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlacedObject {
    pub source: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug)]
struct SourcePiece {
    source: usize,
    range: Range<usize>,
}

#[derive(Default, Debug)]
struct JoinedText {
    text: String,
    pieces: Vec<SourcePiece>,
}

impl JoinedText {
    fn push(&mut self, source: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        self.pieces.push(SourcePiece {
            source,
            range: start..self.text.len(),
        });
    }

    /// UAX word boundaries span original segments. CJK and emoji offer
    /// grapheme-sized opportunities; nonbreaking spaces and closing/opening
    /// punctuation keep their neighboring word together.
    fn tokens(&self) -> Vec<Range<usize>> {
        let mut tokens: Vec<Range<usize>> = Vec::new();
        for (start, part) in self.text.split_word_bound_indices() {
            let end = start + part.len();
            let join_previous = tokens.last().is_some_and(|previous| {
                let before = &self.text[previous.clone()];
                !ascii_space(before)
                    && !ascii_space(part)
                    && (before.ends_with('\u{00a0}')
                        || part.starts_with('\u{00a0}')
                        || before.ends_with('\u{202f}')
                        || part.starts_with('\u{202f}')
                        || before.chars().last().is_some_and(is_opening_punctuation)
                        || part.chars().next().is_some_and(is_closing_punctuation))
            });
            if join_previous {
                tokens.last_mut().unwrap().end = end;
            } else {
                tokens.push(start..end);
            }
        }
        tokens
    }
}

fn is_opening_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '(' | '[' | '{' | '“' | '‘' | '（' | '【' | '《' | '〈' | '「' | '『'
    )
}

fn is_closing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        ')' | ']'
            | '}'
            | ','
            | '.'
            | '!'
            | '?'
            | ';'
            | ':'
            | '”'
            | '’'
            | '，'
            | '。'
            | '！'
            | '？'
            | '；'
            | '：'
            | '）'
            | '】'
            | '》'
            | '〉'
            | '」'
            | '』'
    )
}

fn ascii_space(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|ch| matches!(ch, b' ' | b'\t' | b'\n' | b'\r' | 0x0c))
}

fn nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn positive(value: f32) -> f32 {
    if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    }
}

struct Flow<'a, M> {
    width: f32,
    text_height: f32,
    measure: &'a M,
    lines: Vec<InlineLine>,
    current: InlineLine,
    /// Text since the previous object/line. Measure it together, preserving
    /// kerning across words and source Text atoms instead of summing char widths.
    continuous_text: String,
    continuous_width: f32,
    preserve_spaces: bool,
}

impl<M: Fn(&str) -> f32> Flow<'_, M> {
    fn measured(&self, text: &str) -> f32 {
        nonnegative((self.measure)(text))
    }
    fn remaining(&self) -> f32 {
        (self.width - self.current.width).max(0.0)
    }
    fn occupied(&self) -> bool {
        !self.current.fragments.is_empty()
    }

    fn advance_for(&self, text: &str) -> f32 {
        let combined = format!("{}{}", self.continuous_text, text);
        (self.measured(&combined) - self.continuous_width).max(0.0)
    }

    fn append_text(&mut self, joined: &JoinedText, range: Range<usize>) {
        for piece in &joined.pieces {
            let start = range.start.max(piece.range.start);
            let end = range.end.min(piece.range.end);
            if start >= end {
                continue;
            }
            let text = &joined.text[start..end];
            self.continuous_text.push_str(text);
            let new_width = self.measured(&self.continuous_text);
            let advance = (new_width - self.continuous_width).max(0.0);
            self.continuous_width = new_width;
            let local = start - piece.range.start..end - piece.range.start;
            if let Some(PlacedInline::Text(last)) = self.current.fragments.last_mut() {
                if last.source == piece.source && last.range.end == local.start {
                    last.range.end = local.end;
                    last.text.push_str(text);
                    last.width += advance;
                    self.current.width += advance;
                    continue;
                }
            }
            self.current.fragments.push(PlacedInline::Text(PlacedText {
                source: piece.source,
                range: local,
                text: text.to_owned(),
                x: self.current.width,
                y: 0.0,
                width: advance,
                height: self.text_height,
            }));
            self.current.width += advance;
        }
    }

    fn finish_line(&mut self) {
        if !self.occupied() {
            return;
        }
        if !self.preserve_spaces {
            self.trim_line_end_space();
        }
        if !self.occupied() {
            return;
        }
        let mut line = std::mem::take(&mut self.current);
        line.y = self
            .lines
            .last()
            .map(|line| line.y + line.height)
            .unwrap_or(0.0);
        line.height = self.text_height;
        for fragment in &line.fragments {
            let height = match fragment {
                PlacedInline::Text(text) => text.height,
                PlacedInline::Object(object) => object.height,
            };
            line.height = line.height.max(height);
        }
        for fragment in &mut line.fragments {
            match fragment {
                PlacedInline::Text(text) => text.y = line.y + (line.height - text.height) * 0.5,
                PlacedInline::Object(object) => {
                    object.y = line.y + (line.height - object.height) * 0.5
                }
            }
        }
        self.lines.push(line);
        self.continuous_text.clear();
        self.continuous_width = 0.0;
    }

    fn trim_line_end_space(&mut self) {
        loop {
            let Some(PlacedInline::Text(last)) = self.current.fragments.last() else {
                break;
            };
            let trimmed = last
                .text
                .trim_end_matches(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{000c}'))
                .len();
            if trimmed == last.text.len() {
                break;
            }
            let removed = last.text.len() - trimmed;
            self.continuous_text
                .truncate(self.continuous_text.len().saturating_sub(removed));
            let new_width = self.measured(&self.continuous_text);
            let shrink = (self.continuous_width - new_width).max(0.0);
            self.continuous_width = new_width;
            self.current.width = (self.current.width - shrink).max(0.0);
            let Some(PlacedInline::Text(last)) = self.current.fragments.last_mut() else {
                unreachable!()
            };
            last.text.truncate(trimmed);
            last.range.end -= removed;
            last.width = (last.width - shrink).max(0.0);
            if last.text.is_empty() {
                self.current.fragments.pop();
            } else {
                break;
            }
        }
    }

    fn object(&mut self, source: usize, width: f32, height: f32) {
        let width = nonnegative(width);
        if self.occupied() && width > self.remaining() + 0.001 {
            self.finish_line();
        }
        self.current
            .fragments
            .push(PlacedInline::Object(PlacedObject {
                source,
                x: self.current.width,
                y: 0.0,
                width,
                height: nonnegative(height),
            }));
        self.current.width += width;
        self.continuous_text.clear();
        self.continuous_width = 0.0;
    }

    fn joined_text(&mut self, joined: &JoinedText) {
        let mut pending_space: Option<Range<usize>> = None;
        for token in joined.tokens() {
            let text = &joined.text[token.clone()];
            if !self.preserve_spaces && ascii_space(text) {
                if self.occupied() {
                    pending_space = Some(token);
                }
                continue;
            }
            let pending = pending_space
                .as_ref()
                .map(|range| &joined.text[range.clone()])
                .unwrap_or("");
            let candidate = format!("{pending}{text}");
            if self.advance_for(&candidate) <= self.remaining() + 0.001 {
                if let Some(space) = pending_space.take() {
                    self.append_text(joined, space);
                }
                self.append_text(joined, token);
                continue;
            }
            if self.measured(text) <= self.width + 0.001 {
                // A normal word fits a full line, so it moves as a whole. This
                // also keeps a word spanning multiple source atoms together.
                self.finish_line();
                pending_space = None;
                self.append_text(joined, token);
                continue;
            }
            // An overlong word can break at grapheme boundaries, filling the
            // current line's remainder rather than becoming a full-width flex box.
            if let Some(space) = pending_space.take() {
                if self.advance_for(&joined.text[space.clone()]) < self.remaining() {
                    self.append_text(joined, space);
                }
            }
            let mut start = token.start;
            let mut ends = joined.text[token.clone()]
                .grapheme_indices(true)
                .map(|(start, grapheme)| token.start + start + grapheme.len())
                .peekable();
            while start < token.end {
                let mut end = start;
                while let Some(&candidate) = ends.peek() {
                    if self.advance_for(&joined.text[start..candidate]) > self.remaining() + 0.001 {
                        break;
                    }
                    end = candidate;
                    ends.next();
                }
                if end == start {
                    if self.occupied() {
                        self.finish_line();
                        continue;
                    }
                    // A single grapheme wider than the viewport stays intact;
                    // allowing overflow guarantees progress without corrupting emoji.
                    end = ends.next().unwrap();
                }
                self.append_text(joined, start..end);
                start = end;
                if start < token.end {
                    self.finish_line();
                }
            }
        }
        // Inline-block boundaries permit a break, but a real input space still
        // separates the text and the following object when it fits on the line.
        if let Some(space) = pending_space {
            if self.advance_for(&joined.text[space.clone()]) <= self.remaining() + 0.001 {
                self.append_text(joined, space);
            }
        }
    }
}

/// Lay out continuous source text and atomic objects in reading order.
/// `measure` returns the shaped advance of its entire string at the caller's
/// current text style; it must not insert whitespace or mutate external state.
/// x/y are relative to the content's top-left. Center alignment is an explicit
/// approximation of CSS `vertical-align: middle`; a baseline/x-height adapter
/// can adjust painting without changing the flow's line-break decisions.
pub fn layout_inline(
    atoms: &[InlineAtom],
    available_width: f32,
    text_line_height: f32,
    measure: impl Fn(&str) -> f32,
) -> InlineLayout {
    layout_inline_with_spaces(atoms, available_width, text_line_height, false, measure)
}

/// The ordinary list's pre-wrap text retains spaces. Hard line separators are
/// handled by the consumer before invoking this single-paragraph flow.
pub fn layout_inline_with_spaces(
    atoms: &[InlineAtom],
    available_width: f32,
    text_line_height: f32,
    preserve_spaces: bool,
    measure: impl Fn(&str) -> f32,
) -> InlineLayout {
    let width = positive(available_width);
    let mut flow = Flow {
        width,
        text_height: positive(text_line_height),
        measure: &measure,
        lines: Vec::new(),
        current: InlineLine::default(),
        continuous_text: String::new(),
        continuous_width: 0.0,
        preserve_spaces,
    };
    let mut joined = JoinedText::default();
    for atom in atoms {
        match atom {
            InlineAtom::Text { source, text } => joined.push(*source, text),
            InlineAtom::Object {
                source,
                width,
                height,
            } => {
                flow.joined_text(&joined);
                joined = JoinedText::default();
                flow.object(*source, *width, *height);
            }
        }
    }
    flow.joined_text(&joined);
    flow.finish_line();
    let height = flow
        .lines
        .last()
        .map(|line| line.y + line.height)
        .unwrap_or(0.0);
    InlineLayout {
        width,
        height,
        lines: flow.lines,
    }
}

/// CSS `word-break: break-all` for the label inside an atomic inline-flex box.
/// Ranges retain the original UTF-8 string, excluding hard line separators.
/// A grapheme wider than the label stays intact and is clipped by its consumer.
pub fn wrap_grapheme_lines(
    text: &str,
    available_width: f32,
    measure: impl Fn(&str) -> f32,
) -> Vec<Range<usize>> {
    let width = positive(available_width);
    let mut paragraphs = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < text.len() {
        let ch = text[cursor..].chars().next().unwrap();
        if matches!(ch, '\r' | '\n' | '\u{2028}' | '\u{2029}') {
            paragraphs.push(start..cursor);
            cursor += ch.len_utf8();
            if ch == '\r' && text[cursor..].starts_with('\n') {
                cursor += 1;
            }
            start = cursor;
        } else {
            cursor += ch.len_utf8();
        }
    }
    paragraphs.push(start..text.len());
    let mut lines = Vec::new();
    for paragraph in paragraphs {
        let mut start = paragraph.start;
        for (offset, grapheme) in text[paragraph.clone()].grapheme_indices(true) {
            let next = paragraph.start + offset;
            let end = next + grapheme.len();
            if next > start && nonnegative(measure(&text[start..end])) > width + 0.001 {
                lines.push(start..next);
                start = next;
            }
        }
        lines.push(start..paragraph.end);
    }
    lines
}

/// Longest grapheme-safe prefix fitting the shaped advance. This is display
/// only; callers keep their original source ranges and payload intact.
pub fn fitting_prefix_bytes(
    text: &str,
    available_width: f32,
    measure: impl Fn(&str) -> f32,
) -> usize {
    let width = nonnegative(available_width);
    let mut retained = 0;
    for (start, grapheme) in text.grapheme_indices(true) {
        let end = start + grapheme.len();
        if nonnegative(measure(&text[..end])) > width + 0.001 {
            break;
        }
        retained = end;
    }
    retained
}

/// Always append a display-only ellipsis, including when hidden content starts
/// on a later line. Never split a surrogate/ZWJ/combining cluster for the suffix.
pub fn ellipsized_text(text: &str, available_width: f32, measure: impl Fn(&str) -> f32) -> String {
    let retained = fitting_prefix_bytes(
        text,
        (nonnegative(available_width) - nonnegative(measure("…"))).max(0.0),
        &measure,
    );
    format!("{}…", &text[..retained])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn measure(text: &str) -> f32 {
        text.graphemes(true).count() as f32 * 10.0
    }
    fn text(source: usize, value: &str) -> InlineAtom {
        InlineAtom::Text {
            source,
            text: value.into(),
        }
    }
    fn object(source: usize, width: f32, height: f32) -> InlineAtom {
        InlineAtom::Object {
            source,
            width,
            height,
        }
    }
    fn line_text(line: &InlineLine) -> String {
        line.fragments
            .iter()
            .filter_map(|fragment| match fragment {
                PlacedInline::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }
    fn sources(layout: &InlineLayout) -> Vec<usize> {
        layout
            .lines
            .iter()
            .flat_map(|line| &line.fragments)
            .map(|fragment| match fragment {
                PlacedInline::Text(text) => text.source,
                PlacedInline::Object(object) => object.source,
            })
            .collect()
    }

    #[test]
    fn qq_text_image_file_placeholder_text_fixture_stays_in_order() {
        // The same five DTO segments used by ImRichMixedPreview. Image width
        // includes 4px margins; file/placeholder widths come from their chip boxes.
        let layout = layout_inline(
            &[
                text(0, "prefix "),
                object(1, 64.0, 56.0),
                object(2, 52.0, 20.0),
                object(3, 20.0, 20.0),
                text(4, " suffix"),
            ],
            200.0,
            18.0,
            measure,
        );
        assert_eq!(sources(&layout), [0, 1, 2, 3, 4]);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].width, 186.0);
        assert_eq!(layout.height, 76.0);
        let PlacedInline::Object(image) = &layout.lines[0].fragments[1] else {
            panic!()
        };
        assert_eq!(
            (image.x, image.y, image.width, image.height),
            (70.0, 0.0, 64.0, 56.0)
        );
        let PlacedInline::Text(tail) = &layout.lines[1].fragments[1] else {
            panic!()
        };
        assert_eq!(
            (tail.x, tail.y, tail.text.as_str()),
            (20.0, 57.0, " suffix")
        );
    }

    #[test]
    fn long_text_tail_accepts_image_on_its_last_visual_line() {
        let layout = layout_inline(
            &[
                text(0, "甲乙丙丁戊己庚辛壬癸甲乙丙丁戊"),
                object(1, 40.0, 56.0),
                text(2, "后"),
            ],
            100.0,
            18.0,
            measure,
        );
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(line_text(&layout.lines[0]), "甲乙丙丁戊己庚辛壬癸");
        assert_eq!(line_text(&layout.lines[1]), "甲乙丙丁戊后");
        assert_eq!(layout.height, 74.0);
        let PlacedInline::Object(image) = &layout.lines[1].fragments[1] else {
            panic!()
        };
        assert_eq!((image.x, image.y), (50.0, 18.0));
    }

    #[test]
    fn objects_allow_breaks_without_artificial_spaces() {
        let layout = layout_inline(
            &[text(0, "甲乙"), object(1, 40.0, 30.0), text(2, "丙")],
            50.0,
            18.0,
            measure,
        );
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(line_text(&layout.lines[0]), "甲乙");
        assert_eq!(line_text(&layout.lines[1]), "丙");
        let PlacedInline::Text(tail) = &layout.lines[1].fragments[1] else {
            panic!()
        };
        assert_eq!(tail.x, 40.0);
    }

    #[test]
    fn word_across_adjacent_text_sources_moves_as_one_word() {
        let layout = layout_inline(
            &[text(0, "xx "), text(1, "hel"), text(2, "lo")],
            70.0,
            18.0,
            measure,
        );
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(line_text(&layout.lines[0]), "xx");
        assert_eq!(line_text(&layout.lines[1]), "hello");
        let PlacedInline::Text(hel) = &layout.lines[1].fragments[0] else {
            panic!()
        };
        let PlacedInline::Text(lo) = &layout.lines[1].fragments[1] else {
            panic!()
        };
        assert_eq!(
            (hel.range.clone(), lo.range.clone(), lo.x),
            (0..3, 0..2, 30.0)
        );
    }

    #[test]
    fn zwj_emoji_is_never_split_by_a_line_boundary_even_across_sources() {
        let family = "👨‍👩‍👧‍👦";
        let layout = layout_inline(&[text(0, "甲👨‍"), text(1, "👩‍👧‍👦乙")], 10.0, 18.0, measure);
        assert_eq!(
            layout.lines.iter().map(line_text).collect::<Vec<_>>(),
            ["甲", family, "乙"]
        );
        for line in &layout.lines {
            assert!(line.width <= 10.0);
        }
    }

    #[test]
    fn shaped_measurement_keeps_kerning_between_source_fragments() {
        let measure = |text: &str| {
            text.chars().count() as f32 * 10.0 - if text.contains("AV") { 4.0 } else { 0.0 }
        };
        let layout = layout_inline(&[text(0, "A"), text(1, "V")], 17.0, 18.0, measure);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.lines[0].width, 16.0);
        let PlacedInline::Text(v) = &layout.lines[0].fragments[1] else {
            panic!()
        };
        assert_eq!((v.x, v.width), (10.0, 6.0));
    }

    #[test]
    fn nonbreaking_space_and_closing_punctuation_stay_with_their_word() {
        let layout = layout_inline(&[text(0, "x A\u{a0}B")], 40.0, 18.0, measure);
        assert_eq!(
            layout.lines.iter().map(line_text).collect::<Vec<_>>(),
            ["x", "A\u{a0}B"]
        );
        let punct = layout_inline(&[text(0, "甲乙，丙")], 30.0, 18.0, measure);
        assert_eq!(
            punct.lines.iter().map(line_text).collect::<Vec<_>>(),
            ["甲乙，", "丙"]
        );
    }

    #[test]
    fn oversized_cluster_and_invalid_geometry_make_progress() {
        let layout = layout_inline(
            &[text(0, "😀"), object(1, 100.0, 30.0), text(2, "x")],
            5.0,
            18.0,
            measure,
        );
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(sources(&layout), [0, 1, 2]);
        let empty = layout_inline(&[], f32::NAN, f32::INFINITY, measure);
        assert_eq!((empty.width, empty.height), (1.0, 0.0));
    }

    #[test]
    fn normal_flow_trims_wrap_space_and_pre_wrap_keeps_original_spaces() {
        let normal = layout_inline(
            &[text(0, "aa "), object(1, 40.0, 20.0)],
            50.0,
            18.0,
            measure,
        );
        assert_eq!(line_text(&normal.lines[0]), "aa");
        assert_eq!(normal.lines[0].width, 20.0);
        let pre = layout_inline_with_spaces(&[text(0, "  x  ")], 100.0, 18.0, true, measure);
        assert_eq!(line_text(&pre.lines[0]), "  x  ");
        assert_eq!(pre.lines[0].width, 50.0);
    }

    #[test]
    fn inline_flex_color_value_breaks_all_inside_its_bounded_width() {
        let text = "rgb(garbage long label)";
        let lines = wrap_grapheme_lines(text, 50.0, measure);
        assert_eq!(
            lines
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["rgb(g", "arbag", "e lon", "g lab", "el)"]
        );
        assert_eq!(
            lines
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn bounded_color_value_preserves_emoji_hard_lines_and_blank_lines() {
        let text = "👨‍👩‍👧‍👦x\r\n\n乙";
        let lines = wrap_grapheme_lines(text, 10.0, measure);
        assert_eq!(
            lines
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["👨‍👩‍👧‍👦", "x", "", "乙"]
        );
    }

    #[test]
    fn visible_last_line_ellipsis_does_not_split_graphemes_or_change_source() {
        let text = "A👨‍👩‍👧‍👦BC";
        assert_eq!(ellipsized_text(text, 30.0, measure), "A👨‍👩‍👧‍👦…");
        assert_eq!(text, "A👨‍👩‍👧‍👦BC");
        assert_eq!(ellipsized_text("short", 100.0, measure), "short…");
    }
}
