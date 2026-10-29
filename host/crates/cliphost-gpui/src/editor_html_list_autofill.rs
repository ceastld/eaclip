//! Quill 1.3.7's list-autofill keyboard binding, planned in canvas UTF-8
//! coordinates. This recognizes a typed unmodified space, never pasted text.
//! Applying a plan must preserve source on a clone and reject unsafe owners.

use crate::editor_html::{HtmlCanvasKind, HtmlDocument, HtmlEditError, HtmlListKind};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HtmlListAutofillPlan {
    /// The current optimized text leaf prefix before inserting the space.
    /// Adjacent source runs with identical Quill inline formats form one leaf.
    pub(super) prefix_range: Range<usize>,
    /// Quill getLine's start, including the whole PRE owner for code blocks.
    pub(super) line_start: usize,
    /// That owner's structural newline, before inserting or deleting text.
    pub(super) newline: usize,
    pub(super) kind: HtmlListKind,
    /// Collapsed canvas selection after removing the prefix and typed space.
    pub(super) selection: Range<usize>,
}

/// `None` leaves the normal input path in charge. A successful plan describes
/// two user history steps: first insert one space; cut off history; then remove
/// prefix plus space and apply list to the owner; cut off history again. The
/// executor revalidates this plan against the unchanged original document.
pub(super) fn plan_html_list_autofill(
    document: &HtmlDocument,
    selection: Range<usize>,
    inserted: &str,
    unmodified: bool,
) -> Result<Option<HtmlListAutofillPlan>, HtmlEditError> {
    let projection = document.canvas_projection();
    let text = &projection.text;
    let terminal = text.len().saturating_sub(1);
    if selection.start > selection.end
        || selection.end > terminal
        || !text.is_char_boundary(selection.start)
        || !text.is_char_boundary(selection.end)
    {
        return Err(HtmlEditError::InvalidTextRange);
    }
    if inserted != " " || !unmodified || !selection.is_empty() {
        return Ok(None);
    }
    let caret = selection.start;
    // Block.path(index, true) chooses the left leaf at an inline boundary.
    // Include images in that choice so an image's trailing edge cannot acquire
    // the prefix from an unrelated text leaf before or after the image.
    let Some(leaf_index) = projection.spans.iter().position(|span| {
        matches!(
            span.kind,
            HtmlCanvasKind::Text { .. } | HtmlCanvasKind::Image { .. }
        ) && span.range.start <= caret
            && caret <= span.range.end
    }) else {
        return Ok(None);
    };
    let leaf = &projection.spans[leaf_index];
    let HtmlCanvasKind::Text { run } = leaf.kind else {
        return Ok(None);
    };
    let run = document.runs().get(run).ok_or(HtmlEditError::MissingRun)?;
    let local = caret - leaf.range.start;
    if leaf.range.len() != run.text.len() || !run.text.is_char_boundary(local) {
        return Err(HtmlEditError::InvalidTextRange);
    }
    let Some(inline) = document.canvas_inline_style(match leaf.kind {
        HtmlCanvasKind::Text { run } => run,
        _ => unreachable!(),
    }) else {
        return Ok(None);
    };
    // Quill Inline.optimize merges equivalent wrappers, then Text.optimize
    // merges their adjacent text leaves. Source runs retain those wrappers, so
    // reproduce only this inline equivalence rather than concatenating a line.
    let mut prefix_start = leaf.range.start;
    for previous in projection.spans[..leaf_index].iter().rev() {
        let HtmlCanvasKind::Text { run: previous_run } = previous.kind else {
            break;
        };
        let previous_text = document
            .runs()
            .get(previous_run)
            .ok_or(HtmlEditError::MissingRun)?;
        if previous.range.end != prefix_start
            || previous_text.block_index != run.block_index
            || document.canvas_inline_style(previous_run).as_ref() != Some(&inline)
        {
            break;
        }
        prefix_start = previous.range.start;
    }
    let prefix = &text[prefix_start..caret];
    let Some(kind) = marker_kind(prefix) else {
        return Ok(None);
    };

    let mut owner_start = 0;
    let mut owner_line = None;
    for span in &projection.spans {
        if let HtmlCanvasKind::Boundary { left, .. } = &span.kind {
            if left.index == Some(run.block_index) {
                owner_line = Some((owner_start, span.range.start, left));
                break;
            }
            owner_start = span.range.end;
        }
    }
    let (line_start, newline, owner) =
        owner_line.ok_or(HtmlEditError::UnsupportedBlockStructure)?;
    let format = document
        .canvas_block_format(owner)
        .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
    if format.list.is_some() {
        return Ok(None);
    }
    // The binding compares getLine's offset with the current *leaf* prefix
    // length. Since both end at the caret, a previous leaf makes offset larger
    // and suppresses autofill. Do not concatenate styled runs or reset PRE's
    // context at a literal newline; its getLine is the complete code owner.
    if caret < line_start || caret > newline || caret - line_start > prefix.len() {
        return Ok(None);
    }
    let next_caret = caret - prefix.len();
    Ok(Some(HtmlListAutofillPlan {
        prefix_range: prefix_start..caret,
        line_start,
        newline,
        kind,
        selection: next_caret..next_caret,
    }))
}

fn marker_kind(prefix: &str) -> Option<HtmlListKind> {
    // JavaScript \s differs from Rust is_whitespace: FEFF is included, while
    // NEL (0085) is not. \d in this binding accepts only ASCII digits.
    let marker = prefix.trim_start_matches(js_whitespace);
    match marker {
        "-" | "*" => Some(HtmlListKind::Bullet),
        _ => {
            let digits = marker.strip_suffix('.')?;
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then_some(HtmlListKind::Ordered)
        }
    }
}

fn js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(source: &str, caret: usize) -> Option<HtmlListAutofillPlan> {
        let document = HtmlDocument::parse(source);
        let before = document.source().to_owned();
        let plan = plan_html_list_autofill(&document, caret..caret, " ", true).unwrap();
        assert_eq!(document.source(), before);
        plan
    }

    #[test]
    fn html_list_autofill_markers_and_suffix() {
        for (marker, kind) in [
            ("-", HtmlListKind::Bullet),
            ("*", HtmlListKind::Bullet),
            ("1.", HtmlListKind::Ordered),
            ("01.", HtmlListKind::Ordered),
            ("999999999999999999999999.", HtmlListKind::Ordered),
        ] {
            let plan =
                planned(&format!("<p vendor='keep'>{marker}tail</p>"), marker.len()).unwrap();
            assert_eq!(plan.prefix_range, 0..marker.len());
            assert_eq!(plan.line_start, 0);
            assert_eq!(plan.newline, marker.len() + 4);
            assert_eq!(plan.kind, kind);
            assert_eq!(plan.selection, 0..0);
        }
    }

    #[test]
    fn html_list_autofill_js_whitespace_unicode_and_entities() {
        let prefix = "\t\u{a0}\u{feff}\u{3000}1.";
        let source = format!("<p>😀中</p><p>{prefix}尾</p>");
        let start = "😀中\n".len();
        let plan = planned(&source, start + prefix.len()).unwrap();
        assert_eq!(plan.prefix_range, start..start + prefix.len());
        assert_eq!(plan.line_start, start);
        assert_eq!(plan.newline, start + prefix.len() + "尾".len());
        assert_eq!(plan.selection, start..start);
        assert_eq!(
            planned("<p>&#49;&#46;</p>", 2).unwrap().kind,
            HtmlListKind::Ordered
        );
        for prefix in ["\u{85}-", "１.", "1. ", "-\n", "--", "+", "."] {
            assert!(planned(&format!("<p>{prefix}</p>"), prefix.len()).is_none());
        }
    }

    #[test]
    fn html_list_autofill_typed_space_collapsed_and_modifier_gates() {
        let document = HtmlDocument::parse("<p>-</p>");
        for (selection, inserted, unmodified) in [
            (1..1, "\u{a0}", true),
            (1..1, "  ", true),
            (1..1, "- ", true),
            (1..1, "", true),
            (1..1, " ", false),
            (0..1, " ", true),
        ] {
            assert!(
                plan_html_list_autofill(&document, selection, inserted, unmodified)
                    .unwrap()
                    .is_none()
            );
        }
        for marker in ["[]", "[ ]", "[x]"] {
            assert!(planned(&format!("<p>{marker}</p>"), marker.len()).is_none());
        }
    }

    #[test]
    fn html_list_autofill_existing_list_is_pass_through() {
        for source in [
            "<ul><li>-</li></ul>",
            "<ol><li>1.</li></ol>",
            "<p data-list='bullet'>*</p>",
        ] {
            let document = HtmlDocument::parse(source);
            let caret = document.canvas_projection().text.len() - 1;
            assert!(planned(source, caret).is_none());
        }
    }

    #[test]
    fn html_list_autofill_current_leaf_and_inclusive_inline_boundary() {
        for (source, caret) in [
            ("<p><b>1</b>.</p>", 2),
            ("<p> <b>-</b></p>", 2),
            ("<p>x<b>-</b></p>", 2),
        ] {
            assert!(planned(source, caret).is_none());
        }
        for source in ["<p><b>-</b>x</p>", "<p>-<b>x</b></p>"] {
            assert_eq!(planned(source, 1).unwrap().prefix_range, 0..1);
        }
        assert!(planned("<p><b>-</b>x</p>", 2).is_none());
        for source in [
            "<p><b>1</b><b>.</b></p>",
            "<p><span>1</span>.</p>",
            "<p>1<!--vendor--><span>.</span></p>",
            "<h2><span>1</span>.</h2>",
        ] {
            assert_eq!(planned(source, 2).unwrap().prefix_range, 0..2);
        }
        // Canvas headings look bold by default, but Quill's heading attribute
        // does not make its text leaf equal to an explicit bold leaf.
        assert!(planned("<h2>1<b>.</b></h2>", 2).is_none());
        assert!(planned("<p>1<span style='color:red'>.</span></p>", 2).is_none());
    }

    #[test]
    fn html_list_autofill_block_context_and_whole_pre_owner() {
        for source in [
            "<h2>-</h2>",
            "<blockquote><p>-</p></blockquote>",
            "<p class='ql-indent-2'>-</p>",
            "<pre>-</pre>",
        ] {
            assert_eq!(planned(source, 1).unwrap().line_start, 0);
        }
        let plan = planned("<p>A</p><pre> \n-</pre>", 5).unwrap();
        assert_eq!(plan.prefix_range, 2..5);
        assert_eq!(plan.line_start, 2);
        assert_eq!(plan.newline, 5);
        assert_eq!(plan.selection, 2..2);
        assert!(planned("<pre>x\n-</pre>", 3).is_none());
        assert!(planned("<p>A<br>-</p>", 3).is_none());
    }

    #[test]
    fn html_list_autofill_image_leaf_does_not_supply_a_text_prefix() {
        assert!(planned("<p><img src='a.png'>-</p>", 4).is_none());
        assert!(planned("<p>-<img src='a.png'>x</p>", 4).is_none());
        // A suffix image does not prevent the left marker leaf from matching.
        assert_eq!(planned("<p>-<img src='a.png'></p>", 1).unwrap().newline, 4);
    }

    #[test]
    fn html_list_autofill_invalid_utf8_terminal_and_reversed_ranges() {
        let document = HtmlDocument::parse("<p>😀-<img src='a.png'></p>");
        let before = document.source().to_owned();
        for selection in [1..1, 6..6, 7..7, 4..2, 9..9, 0..99] {
            assert_eq!(
                plan_html_list_autofill(&document, selection, " ", true),
                Err(HtmlEditError::InvalidTextRange)
            );
            assert_eq!(document.source(), before);
        }
        assert!(planned("", 0).is_none());
        assert!(planned("<p><br></p>", 0).is_none());
    }
}
