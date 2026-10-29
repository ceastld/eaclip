//! Executes a validated Tab plan against a source-preserving candidate document.
//! Original UTF-8 coordinates stay stable by applying text edits from the end;
//! the parent owns keyboard context, IME/read-only guards, selection and history.

use super::{
    html_tab::HtmlTabPlan, HtmlBlockMark, HtmlCanvasKind, HtmlDocument, HtmlEditError, HtmlStyle,
};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

pub(super) fn apply_html_tab_plan(
    document: &mut HtmlDocument,
    plan: &HtmlTabPlan,
) -> Result<(), HtmlEditError> {
    if !plan.edits.is_empty() && !plan.indents.is_empty() {
        return Err(HtmlEditError::InvalidTextRange);
    }
    let original = document.canvas_projection();
    let limit = original.text.len().saturating_sub(1);
    for (index, edit) in plan.edits.iter().enumerate() {
        validate_range(&original.text, &edit.range, limit)?;
        if original.spans.iter().any(|span| {
            matches!(span.kind, HtmlCanvasKind::Image { .. })
                && [edit.range.start, edit.range.end]
                    .into_iter()
                    .any(|edge| span.range.start < edge && edge < span.range.end)
        }) {
            return Err(HtmlEditError::InvalidTextRange);
        }
        // These are exactly the three text operations in this keyboard family:
        // a selected literal Tab, code-line two-space prefixes, or removal of
        // an existing Tab/two-space prefix. No newline or arbitrary text import.
        match edit.text {
            "\t" => {}
            "  " if edit.range.is_empty() => {}
            "" if matches!(original.text.get(edit.range.clone()), Some("\t" | "  ")) => {}
            _ => return Err(HtmlEditError::InvalidTextRange),
        }
        if let Some(previous) = index.checked_sub(1).map(|index| &plan.edits[index]) {
            if previous.range.end > edit.range.start || previous.range.start == edit.range.start {
                return Err(HtmlEditError::InvalidTextRange);
            }
        }
    }
    for (index, indent) in plan.indents.iter().enumerate() {
        if indent.level > 8
            || indent.line_start > limit
            || !original.text.is_char_boundary(indent.line_start)
            || (indent.line_start != 0
                && original.text.as_bytes().get(indent.line_start - 1) != Some(&b'\n'))
            || plan.indents[..index]
                .iter()
                .any(|previous| previous.line_start == indent.line_start)
        {
            return Err(HtmlEditError::InvalidTextRange);
        }
    }
    // A PRE or a BR-containing container can own several logical canvas rows.
    // Setting its one source attribute must not change an uncovered row. This
    // executor does not manufacture split owners merely to apply indentation.
    let mut indent_owners = HashMap::new();
    for indent in &plan.indents {
        let owner = super::canvas_block_owners(document, indent.line_start..indent.line_start)
            .into_iter()
            .next()
            .ok_or(HtmlEditError::MissingBlock)?;
        let current = document
            .canvas_block_format(&owner)
            .ok_or(HtmlEditError::MissingBlock)?;
        let target = indent_owners
            .entry((owner.source_range.start, owner.source_range.end))
            .or_insert_with(|| (indent.level, current.indent != indent.level, HashSet::new()));
        if target.0 != indent.level {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
        target.2.insert(indent.line_start);
    }
    if !indent_owners.is_empty() {
        for start in std::iter::once(0)
            .chain(
                original
                    .text
                    .match_indices('\n')
                    .map(|(index, _)| index + 1),
            )
            .filter(|start| *start <= limit)
        {
            let owner = super::canvas_block_owners(document, start..start)
                .into_iter()
                .next()
                .ok_or(HtmlEditError::MissingBlock)?;
            if indent_owners
                .get(&(owner.source_range.start, owner.source_range.end))
                .is_some_and(|(_, changing, covered)| *changing && !covered.contains(&start))
            {
                return Err(HtmlEditError::UnsupportedBlockStructure);
            }
        }
    }
    let mut expected = original.text.clone();
    for edit in plan.edits.iter().rev() {
        expected.replace_range(edit.range.clone(), edit.text);
    }
    validate_range(&expected, &plan.selection, expected.len().saturating_sub(1))?;
    if plan.history_cutoff {
        let [edit] = plan.edits.as_slice() else {
            return Err(HtmlEditError::InvalidTextRange);
        };
        if edit.text != "\t"
            || !plan.indents.is_empty()
            || !plan.selection.is_empty()
            || plan.selection.start != edit.range.start + 1
        {
            return Err(HtmlEditError::InvalidTextRange);
        }
    }

    let mut candidate = document.clone();
    for edit in plan.edits.iter().rev() {
        let mut step = candidate.canvas_projection().text;
        validate_range(&step, &edit.range, step.len().saturating_sub(1))?;
        step.replace_range(edit.range.clone(), edit.text);
        let caret = candidate.replace_canvas_range(edit.range.clone(), edit.text)?;
        if caret != edit.range.start + edit.text.len() || candidate.canvas_projection().text != step
        {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    for indent in plan.indents.iter().rev() {
        let owner = super::canvas_block_owners(&candidate, indent.line_start..indent.line_start)
            .into_iter()
            .next()
            .ok_or(HtmlEditError::MissingBlock)?;
        let current = candidate
            .canvas_block_format(&owner)
            .ok_or(HtmlEditError::MissingBlock)?;
        if current.indent != indent.level {
            candidate.toggle_canvas_block(&owner, HtmlBlockMark::Indent(indent.level))?;
        }
        // A local class must not merely expose a different inherited indent.
        // Re-query after each source change; prior owner source ranges expire.
        let owner = super::canvas_block_owners(&candidate, indent.line_start..indent.line_start)
            .into_iter()
            .next()
            .ok_or(HtmlEditError::MissingBlock)?;
        if candidate
            .canvas_block_format(&owner)
            .ok_or(HtmlEditError::MissingBlock)?
            .indent
            != indent.level
            || candidate.canvas_projection().text != original.text
        {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    if plan.history_cutoff {
        let caret = plan.selection.start;
        // Quill's ordinary Tab inserts an op with no attributes: clear inherited
        // inline marks only on the new Tab, retaining block font bases and all
        // surrounding content. This is part of the same candidate transaction.
        super::set_paste_inline_style(&mut candidate, caret - 1..caret, &HtmlStyle::default())?;
    }
    let final_projection = candidate.canvas_projection();
    if final_projection.text != expected {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    for indent in &plan.indents {
        let owner = super::canvas_block_owners(&candidate, indent.line_start..indent.line_start)
            .into_iter()
            .next()
            .ok_or(HtmlEditError::MissingBlock)?;
        if candidate
            .canvas_block_format(&owner)
            .ok_or(HtmlEditError::MissingBlock)?
            .indent
            != indent.level
        {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    validate_range(
        &final_projection.text,
        &plan.selection,
        final_projection.text.len().saturating_sub(1),
    )?;
    if final_projection.spans.iter().any(|span| {
        matches!(span.kind, HtmlCanvasKind::Image { .. })
            && [plan.selection.start, plan.selection.end]
                .into_iter()
                .any(|edge| span.range.start < edge && edge < span.range.end)
    }) {
        return Err(HtmlEditError::InvalidTextRange);
    }
    *document = candidate;
    Ok(())
}

fn validate_range(text: &str, range: &Range<usize>, limit: usize) -> Result<(), HtmlEditError> {
    if range.start > range.end
        || range.end > limit
        || !text.is_char_boundary(range.start)
        || !text.is_char_boundary(range.end)
    {
        return Err(HtmlEditError::InvalidTextRange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::html_tab::{HtmlTabIndent, HtmlTabTextEdit};
    use super::*;

    fn plan(
        edits: Vec<HtmlTabTextEdit>,
        indents: Vec<HtmlTabIndent>,
        selection: Range<usize>,
    ) -> HtmlTabPlan {
        HtmlTabPlan {
            edits,
            indents,
            selection,
            history_cutoff: false,
        }
    }

    fn text_edit(range: Range<usize>, text: &'static str) -> HtmlTabTextEdit {
        HtmlTabTextEdit { range, text }
    }

    fn assert_rejected(source: &str, plan: &HtmlTabPlan) {
        let mut document = HtmlDocument::parse(source);
        assert!(apply_html_tab_plan(&mut document, plan).is_err());
        assert_eq!(document.source(), source);
    }

    #[test]
    fn code_prefix_insert_preserves_unicode_entities_comments_and_attributes() {
        let source =
            "<pre vendor='keep'><span data-u='α'>&#x1F600; &amp;</span><!--keep-->\nB</pre>";
        let mut document = HtmlDocument::parse(source);
        let before = document.canvas_projection().text;
        let second = before.find('\n').unwrap() + 1;
        let plan = plan(
            vec![text_edit(0..0, "  "), text_edit(second..second, "  ")],
            vec![],
            2..before.len() + 3,
        );
        apply_html_tab_plan(&mut document, &plan).unwrap();
        assert_eq!(
            document.canvas_projection().text,
            format!("  {}  {}", &before[..second], &before[second..])
        );
        for preserved in [
            "vendor='keep'",
            "data-u='α'",
            "&#x1F600;",
            "&amp;",
            "<!--keep-->",
        ] {
            assert!(document.source().contains(preserved), "{preserved}");
        }
    }

    #[test]
    fn code_prefix_delete_retains_unselected_raw_entities_and_inline_nodes() {
        let source = "<pre data-v='keep'><span data-u='keep'>&#32; &#x1F600;</span><!--stay-->\n  &amp;</pre>";
        let mut document = HtmlDocument::parse(source);
        let before = document.canvas_projection().text;
        let second = before.find('\n').unwrap() + 1;
        let plan = plan(
            vec![text_edit(0..2, ""), text_edit(second..second + 2, "")],
            vec![],
            0..before.len() - 5,
        );
        apply_html_tab_plan(&mut document, &plan).unwrap();
        assert_eq!(document.canvas_projection().text, "😀\n&\n");
        for preserved in [
            "data-v='keep'",
            "data-u='keep'",
            "&#x1F600;",
            "&amp;",
            "<!--stay-->",
        ] {
            assert!(document.source().contains(preserved), "{preserved}");
        }
    }

    #[test]
    fn selected_list_rows_get_their_own_exact_levels_and_keep_payload() {
        let source = "<ol data-v='keep'><li class='ql-indent-1' data-i='one'>甲</li><!--mid--><li data-i='two'>乙</li></ol>";
        let mut document = HtmlDocument::parse(source);
        let before = document.canvas_projection().text;
        let second = before.find('\n').unwrap() + 1;
        let plan = plan(
            vec![],
            vec![
                HtmlTabIndent {
                    line_start: 0,
                    level: 2,
                },
                HtmlTabIndent {
                    line_start: second,
                    level: 1,
                },
            ],
            0..before.len() - 1,
        );
        apply_html_tab_plan(&mut document, &plan).unwrap();
        assert_eq!(document.canvas_projection().text, before);
        for (start, level) in [(0, 2), (second, 1)] {
            let owner = super::super::canvas_block_owners(&document, start..start).remove(0);
            let format = document.canvas_block_format(&owner).unwrap();
            assert_eq!(format.indent, level);
            assert!(format.list.is_some());
        }
        for preserved in [
            "data-v='keep'",
            "data-i='one'",
            "data-i='two'",
            "<!--mid-->",
        ] {
            assert!(document.source().contains(preserved), "{preserved}");
        }
    }

    #[test]
    fn inherited_single_owner_indent_can_return_to_zero_without_losing_attributes() {
        let source = "<div class='ql-indent-2' vendor='outer'><ol data-o='keep'><li data-i='keep'>A</li></ol></div>";
        let mut document = HtmlDocument::parse(source);
        let before = document.canvas_projection().text;
        let plan = plan(
            vec![],
            vec![HtmlTabIndent {
                line_start: 0,
                level: 0,
            }],
            0..1,
        );
        apply_html_tab_plan(&mut document, &plan).unwrap();
        let owner = super::super::canvas_block_owners(&document, 0..0).remove(0);
        assert_eq!(document.canvas_block_format(&owner).unwrap().indent, 0);
        assert_eq!(document.canvas_projection().text, before);
        for preserved in ["vendor='outer'", "data-o='keep'", "data-i='keep'"] {
            assert!(document.source().contains(preserved), "{preserved}");
        }
    }

    #[test]
    fn partial_shared_pre_or_br_indent_refuses_but_full_owner_coverage_is_safe() {
        for source in [
            "<div class='ql-indent-2' vendor='keep'><pre data-u='keep'>A\nB</pre></div>",
            "<div class='ql-indent-2' vendor='keep'>A<br>B</div>",
        ] {
            let mut document = HtmlDocument::parse(source);
            let before = document.canvas_projection().text;
            assert_eq!(before, "A\nB\n");
            let partial = plan(
                vec![],
                vec![HtmlTabIndent {
                    line_start: 0,
                    level: 1,
                }],
                0..1,
            );
            assert_eq!(
                apply_html_tab_plan(&mut document, &partial),
                Err(HtmlEditError::UnsupportedBlockStructure)
            );
            assert_eq!(document.source(), source);
            let complete = plan(
                vec![],
                vec![
                    HtmlTabIndent {
                        line_start: 0,
                        level: 1,
                    },
                    HtmlTabIndent {
                        line_start: 2,
                        level: 1,
                    },
                ],
                0..3,
            );
            apply_html_tab_plan(&mut document, &complete).unwrap();
            assert_eq!(document.canvas_projection().text, before);
            assert!(document.source().contains("vendor='keep'"));
            let conflicted = plan(
                vec![],
                vec![
                    HtmlTabIndent {
                        line_start: 0,
                        level: 0,
                    },
                    HtmlTabIndent {
                        line_start: 2,
                        level: 2,
                    },
                ],
                0..3,
            );
            let applied_source = document.source().to_string();
            assert!(apply_html_tab_plan(&mut document, &conflicted).is_err());
            assert_eq!(document.source(), applied_source);
        }
    }

    #[test]
    fn invalid_mixed_overlapping_utf8_atom_and_terminal_plans_never_mutate_source() {
        let source = "<p>😀<img src='x.png' vendor='keep'>A</p>";
        let document = HtmlDocument::parse(source);
        let text = document.canvas_projection().text;
        let plans = [
            plan(
                vec![text_edit(0..0, "  ")],
                vec![HtmlTabIndent {
                    line_start: 0,
                    level: 1,
                }],
                2..2,
            ),
            plan(
                vec![text_edit(0..4, "\t"), text_edit(0..0, "  ")],
                vec![],
                1..1,
            ),
            plan(vec![text_edit(1..1, "  ")], vec![], 2..2),
            plan(vec![text_edit(5..5, "  ")], vec![], 2..2),
            plan(vec![text_edit(text.len()..text.len(), "  ")], vec![], 2..2),
            plan(
                vec![],
                vec![HtmlTabIndent {
                    line_start: 0,
                    level: 9,
                }],
                0..0,
            ),
        ];
        for plan in &plans {
            assert_rejected(source, plan);
        }
    }

    #[test]
    fn a_later_shared_container_failure_rolls_back_earlier_indent_changes() {
        let source = "<p data-first='keep'>A</p><div vendor='keep'>B<p>C</p>D</div>";
        let document = HtmlDocument::parse(source);
        let text = document.canvas_projection().text;
        let start = text.find('B').unwrap();
        // Execute the ordinary first owner before the unsafe shared owner.
        let plan = plan(
            vec![],
            vec![
                HtmlTabIndent {
                    line_start: start,
                    level: 1,
                },
                HtmlTabIndent {
                    line_start: 0,
                    level: 1,
                },
            ],
            0..text.len() - 1,
        );
        let mut candidate = document.clone();
        assert_eq!(
            apply_html_tab_plan(&mut candidate, &plan),
            Err(HtmlEditError::UnsupportedBlockStructure)
        );
        assert_eq!(candidate.source(), source);
    }

    #[test]
    fn ordinary_tab_is_atomic_and_clears_only_its_inherited_inline_attributes() {
        let source = "<p vendor='keep'><b><a href='https://example.test' data-u='keep'>AB</a></b><!--stay--></p>";
        let mut document = HtmlDocument::parse(source);
        let mut plan = plan(vec![text_edit(1..2, "\t")], vec![], 2..2);
        plan.history_cutoff = true;
        apply_html_tab_plan(&mut document, &plan).unwrap();
        assert_eq!(document.canvas_projection().text, "A\t\n");
        let original = document.runs().iter().find(|run| run.text == "A").unwrap();
        assert!(original.style.bold && original.style.link.is_some());
        let tab = document.runs().iter().find(|run| run.text == "\t").unwrap();
        assert!(!tab.style.bold && tab.style.link.is_none());
        for preserved in ["vendor='keep'", "data-u='keep'", "<!--stay-->"] {
            assert!(document.source().contains(preserved), "{preserved}");
        }
    }

    #[test]
    fn ordinary_tab_keeps_heading_base_and_rejects_a_false_cutoff_or_bad_selection() {
        let source = "<h2 vendor='keep'><i>AB</i></h2>";
        let mut document = HtmlDocument::parse(source);
        let mut plan = plan(vec![text_edit(1..1, "\t")], vec![], 2..2);
        plan.history_cutoff = true;
        apply_html_tab_plan(&mut document, &plan).unwrap();
        let tab = document.runs().iter().find(|run| run.text == "\t").unwrap();
        assert!(tab.style.bold && !tab.style.italic);
        assert!(document.source().contains("vendor='keep'"));
        let mut invalid = plan;
        invalid.edits[0].text = "  ";
        assert_rejected(source, &invalid);
        let invalid = self::plan(vec![text_edit(1..1, "  ")], vec![], 99..99);
        assert_rejected(source, &invalid);
    }
}
