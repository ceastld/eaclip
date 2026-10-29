//! Pure Tab planning in canvas UTF-8 coordinates. Source edits are applied by
//! the caller on a clone; this planner never rewrites HTML or guesses a source
//! span. Binding order mirrors the local Quill 1.3.7 keyboard module.

use crate::editor_html::{HtmlBlock, HtmlCanvasBlock, HtmlCanvasKind, HtmlDocument, HtmlEditError};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HtmlTabPlan {
    pub(super) edits: Vec<HtmlTabTextEdit>,
    pub(super) indents: Vec<HtmlTabIndent>,
    pub(super) selection: Range<usize>,
    pub(super) history_cutoff: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HtmlTabTextEdit {
    pub(super) range: Range<usize>,
    pub(super) text: &'static str,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HtmlTabIndent {
    pub(super) line_start: usize,
    pub(super) level: u8,
}

struct Line {
    start: usize,
    newline: usize,
    owner: HtmlCanvasBlock,
    format: HtmlBlock,
}

pub(super) fn plan_html_tab(
    document: &HtmlDocument,
    selection: Range<usize>,
    shift: bool,
) -> Result<Option<HtmlTabPlan>, HtmlEditError> {
    let projection = document.canvas_projection();
    let text = &projection.text;
    if selection.start > selection.end
        || selection.end > text.len()
        || !text.is_char_boundary(selection.start)
        || !text.is_char_boundary(selection.end)
    {
        return Err(HtmlEditError::InvalidTextRange);
    }
    let terminal = text.len().saturating_sub(1);
    let selection = selection.start.min(terminal)..selection.end.min(terminal);
    let mut start = 0;
    let mut lines = Vec::new();
    for (newline, _) in text.match_indices('\n') {
        let span = projection
            .spans
            .iter()
            .find(|span| span.range.start <= start && start < span.range.end)
            .ok_or(HtmlEditError::InvalidTextRange)?;
        let owner = match &span.kind {
            HtmlCanvasKind::Boundary { left, .. } => left.clone(),
            HtmlCanvasKind::Text { run } => {
                indexed_owner(document, document.runs()[*run].block_index)
            }
            HtmlCanvasKind::Image { index } => {
                indexed_owner(document, document.images()[*index].block_index)
            }
            HtmlCanvasKind::Break { block, .. } => indexed_owner(document, *block),
        };
        let format = document
            .canvas_block_format(&owner)
            .ok_or(HtmlEditError::UnsupportedBlockStructure)?;
        lines.push(Line {
            start,
            newline,
            owner,
            format,
        });
        start = newline + 1;
    }
    let current = lines
        .iter()
        .find(|line| line.start <= selection.start && selection.start <= line.newline)
        .ok_or(HtmlEditError::InvalidTextRange)?;
    let selected = lines
        .iter()
        .filter(|line| {
            if selection.is_empty() {
                line.start <= selection.start && selection.start <= line.newline
            } else {
                selection.start <= line.newline && selection.end > line.start
            }
        })
        .collect::<Vec<_>>();
    let common_structural = selected.iter().all(|line| line.format.list.is_some())
        || selected.iter().all(|line| line.format.quote)
        || selected.iter().all(|line| line.format.indent > 0);
    // Quill getLine returns a whole PRE CodeBlock, so its context offset is
    // local to that owner, not reset at every literal newline in its text leaf.
    let context_start = if current.format.code {
        lines
            .iter()
            .find(|line| line.owner == current.owner)
            .map_or(current.start, |line| line.start)
    } else {
        current.start
    };
    if common_structural && (!selection.is_empty() || selection.start == context_start) {
        let indents = selected
            .iter()
            .filter_map(|line| {
                let level = i16::from(line.format.indent) + if shift { -1 } else { 1 };
                // Quill's indent attributor rejects outside its 1..=8 whitelist;
                // zero explicitly removes indentation. Boundary presses are handled
                // no-ops, rather than an opportunity to insert a literal tab.
                ((0..=8).contains(&level) && level != i16::from(line.format.indent)).then_some(
                    HtmlTabIndent {
                        line_start: line.start,
                        level: level as u8,
                    },
                )
            })
            .collect();
        return Ok(Some(HtmlTabPlan {
            edits: Vec::new(),
            indents,
            selection,
            history_cutoff: false,
        }));
    }
    if selected.iter().all(|line| line.format.code) {
        let last_owner = &selected
            .last()
            .ok_or(HtmlEditError::InvalidTextRange)?
            .owner;
        let mut edits = Vec::new();
        for line in &lines {
            // Code indentation includes the endpoint line when the endpoint is
            // exactly its start within the same code owner. Do not spill into an
            // adjacent unselected PRE/paragraph just because global indices fit.
            let covered = selection.start <= line.newline
                && (line.start < selection.end
                    || line.start == selection.end && &line.owner == last_owner);
            if !covered || !line.format.code {
                continue;
            }
            if shift {
                if text[line.start..line.newline].starts_with("  ") {
                    edits.push(HtmlTabTextEdit {
                        range: line.start..line.start + 2,
                        text: "",
                    });
                }
            } else {
                edits.push(HtmlTabTextEdit {
                    range: line.start..line.start,
                    text: "  ",
                });
            }
        }
        let mapped = map_position(selection.start, &edits)..map_position(selection.end, &edits);
        return Ok(Some(HtmlTabPlan {
            edits,
            indents: Vec::new(),
            selection: mapped,
            history_cutoff: false,
        }));
    }
    if shift {
        if selection.is_empty()
            && selection.start > current.start
            && text.as_bytes().get(selection.start - 1) == Some(&b'\t')
        {
            let cursor = selection.start - 1;
            return Ok(Some(HtmlTabPlan {
                edits: vec![HtmlTabTextEdit {
                    range: cursor..selection.start,
                    text: "",
                }],
                indents: Vec::new(),
                selection: cursor..cursor,
                history_cutoff: false,
            }));
        }
        return Ok(None);
    }
    let cursor = selection.start + 1;
    Ok(Some(HtmlTabPlan {
        edits: vec![HtmlTabTextEdit {
            range: selection,
            text: "\t",
        }],
        indents: Vec::new(),
        selection: cursor..cursor,
        history_cutoff: true,
    }))
}

fn indexed_owner(document: &HtmlDocument, index: usize) -> HtmlCanvasBlock {
    HtmlCanvasBlock {
        index: Some(index),
        source_range: document.blocks()[index].source_range.clone(),
    }
}

fn map_position(position: usize, edits: &[HtmlTabTextEdit]) -> usize {
    let mut delta: isize = 0;
    for edit in edits {
        if position < edit.range.start {
            break;
        }
        if position < edit.range.end {
            return (edit.range.start as isize + delta) as usize + edit.text.len();
        }
        delta += edit.text.len() as isize - edit.range.len() as isize;
    }
    (position as isize + delta) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(source: &str, range: Range<usize>, shift: bool) -> Option<HtmlTabPlan> {
        plan_html_tab(&HtmlDocument::parse(source), range, shift).unwrap()
    }
    #[test]
    fn html_tab_list_at_start_or_selection_indents_and_midrow_falls_back() {
        let source = "<ul><li>AB</li></ul>";
        let first = plan(source, 0..0, false).unwrap();
        assert_eq!(
            first.indents,
            vec![HtmlTabIndent {
                line_start: 0,
                level: 1
            }]
        );
        assert!(first.edits.is_empty());
        assert_eq!(first.selection, 0..0);
        assert!(!first.history_cutoff);
        let selected = plan(source, 1..2, false).unwrap();
        assert_eq!(selected.indents, first.indents);
        assert_eq!(selected.selection, 1..2);
        let middle = plan(source, 1..1, false).unwrap();
        assert!(middle.indents.is_empty());
        assert_eq!(
            middle.edits,
            vec![HtmlTabTextEdit {
                range: 1..1,
                text: "\t"
            }]
        );
        assert!(middle.history_cutoff);
        assert_eq!(plan(source, 1..1, true), None);
    }
    #[test]
    fn html_tab_common_presence_and_per_line_levels_match_quill_structural_priority() {
        let mixed = plan("<ul><li>A</li></ul><p>B</p>", 0..3, false).unwrap();
        assert!(mixed.indents.is_empty());
        assert_eq!(mixed.edits[0].text, "\t");
        let both = plan(
            "<p class='ql-indent-1'>A</p><p class='ql-indent-3'>B</p>",
            0..3,
            false,
        )
        .unwrap();
        assert_eq!(
            both.indents,
            vec![
                HtmlTabIndent {
                    line_start: 0,
                    level: 2
                },
                HtmlTabIndent {
                    line_start: 2,
                    level: 4
                }
            ]
        );
        let quote = plan("<blockquote>AB</blockquote>", 0..0, true).unwrap();
        assert!(quote.edits.is_empty() && quote.indents.is_empty());
        let code = plan("<pre class='ql-indent-1'>A\nB</pre>", 0..0, false).unwrap();
        assert_eq!(code.indents[0].level, 2);
        let code_second_row = plan("<pre class='ql-indent-1'>A\nB</pre>", 2..2, false).unwrap();
        assert!(code_second_row.indents.is_empty());
        assert_eq!(code_second_row.edits[0].range, 2..2);
        assert_eq!(code_second_row.edits[0].text, "  ");
    }
    #[test]
    fn html_tab_structural_limits_are_handled_noops_and_halfopen_end_excludes_next_row() {
        let maximum = plan("<ul><li class='ql-indent-8'>A</li></ul>", 0..0, false).unwrap();
        assert!(maximum.edits.is_empty() && maximum.indents.is_empty());
        let minimum = plan("<ul><li>A</li></ul>", 0..0, true).unwrap();
        assert!(minimum.edits.is_empty() && minimum.indents.is_empty());
        let selected = plan("<ul><li>A</li><li>B</li></ul>", 0..2, false).unwrap();
        assert_eq!(
            selected.indents,
            vec![HtmlTabIndent {
                line_start: 0,
                level: 1
            }]
        );
        let outdent = plan("<ul><li class='ql-indent-1'>A</li></ul>", 0..0, true).unwrap();
        assert_eq!(outdent.indents[0].level, 0);
    }
    #[test]
    fn html_tab_code_includes_endpoint_within_owner_and_preserves_unicode_selection() {
        let code = plan("<pre>A😀\nB</pre>", 1..6, false).unwrap();
        assert_eq!(
            code.edits,
            vec![
                HtmlTabTextEdit {
                    range: 0..0,
                    text: "  "
                },
                HtmlTabTextEdit {
                    range: 6..6,
                    text: "  "
                }
            ]
        );
        assert_eq!(code.selection, 3..10);
        assert!(!code.history_cutoff);
        let sibling = plan("<pre>A</pre><pre>B</pre>", 0..2, false).unwrap();
        assert_eq!(
            sibling.edits,
            vec![HtmlTabTextEdit {
                range: 0..0,
                text: "  "
            }]
        );
        assert_eq!(sibling.selection, 2..4);
    }
    #[test]
    fn html_tab_code_outdent_requires_two_spaces_and_never_removes_literal_tab() {
        let source = "<pre>  A\n B\n\tC\n  D</pre>";
        let selected = plan(source, 1..13, true).unwrap();
        assert_eq!(
            selected.edits,
            vec![
                HtmlTabTextEdit {
                    range: 0..2,
                    text: ""
                },
                HtmlTabTextEdit {
                    range: 10..12,
                    text: ""
                }
            ]
        );
        assert_eq!(selected.selection, 0..9);
        let no_indent = plan("<pre>\tA</pre>", 1..1, true).unwrap();
        assert!(no_indent.edits.is_empty());
        assert_eq!(no_indent.selection, 1..1);
    }
    #[test]
    fn html_tab_normal_shift_removes_previous_literal_tab_only_or_passes_through() {
        let removal = plan("<p>A\tB</p>", 2..2, true).unwrap();
        assert_eq!(
            removal.edits,
            vec![HtmlTabTextEdit {
                range: 1..2,
                text: ""
            }]
        );
        assert_eq!(removal.selection, 1..1);
        assert!(!removal.history_cutoff);
        assert_eq!(plan("<p>A B</p>", 2..2, true), None);
        assert_eq!(plan("<p>A\tB</p>", 0..2, true), None);
    }
    #[test]
    fn html_tab_normal_selection_replaces_with_single_literal_tab_and_protects_terminal() {
        let source = "<p>A😀B</p>";
        let selected = plan(source, 1..5, false).unwrap();
        assert_eq!(
            selected.edits,
            vec![HtmlTabTextEdit {
                range: 1..5,
                text: "\t"
            }]
        );
        assert_eq!(selected.selection, 2..2);
        assert!(selected.history_cutoff);
        let end = plan(source, 7..7, false).unwrap();
        assert_eq!(end.edits[0].range, 6..6);
        let all = plan(source, 0..7, false).unwrap();
        assert_eq!(all.edits[0].range, 0..6);
        assert_eq!(all.selection, 1..1);
    }
    #[test]
    fn html_tab_rejects_non_utf8_ranges_and_object_interiors_without_source_mutation() {
        let source = "<p>😀<img src='owned' vendor='keep'>B</p>";
        let document = HtmlDocument::parse(source);
        for range in [1..1, 5..5, 4..6, 3..2, 0..10] {
            assert_eq!(
                plan_html_tab(&document, range, false),
                Err(HtmlEditError::InvalidTextRange)
            );
            assert_eq!(document.source(), source);
        }
        let atom = plan_html_tab(&document, 4..7, false).unwrap().unwrap();
        assert_eq!(atom.edits[0].range, 4..7);
    }
}
