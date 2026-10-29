//! Source-preserving list autofill, including the intermediate space snapshot
//! required by Quill's two-stage history. Neither candidate is published here.

use super::{
    html_list_autofill::{plan_html_list_autofill, HtmlListAutofillPlan},
    HtmlBlock, HtmlBlockKind, HtmlBlockMark, HtmlCanvasBlock, HtmlCanvasKind, HtmlDocument,
    HtmlEditError, PendingFormat,
};
use std::ops::Range;

pub(super) struct HtmlListAutofillEdit {
    pub(super) space_document: HtmlDocument,
    pub(super) document: HtmlDocument,
    pub(super) selection: Range<usize>,
    /// Canvas UTF-8 offsets, converted to UTF-16 by the parent using each
    /// corresponding snapshot. Quill history restores line ends for this change.
    pub(super) undo_caret: usize,
    pub(super) redo_caret: usize,
}

pub(super) fn apply_html_list_autofill(
    document: &HtmlDocument,
    plan: &HtmlListAutofillPlan,
    pending: &[PendingFormat],
) -> Result<HtmlListAutofillEdit, HtmlEditError> {
    let replanned = plan_html_list_autofill(
        document,
        plan.prefix_range.end..plan.prefix_range.end,
        " ",
        true,
    )?
    .ok_or(HtmlEditError::InvalidTextRange)?;
    if &replanned != plan {
        return Err(HtmlEditError::InvalidTextRange);
    }
    let original = document.canvas_projection();
    let (owner, format) = owner_at(document, plan.line_start)?;
    // A code/BR owner with several visible rows cannot be converted by changing
    // its shared source tags. No new split-owner architecture is introduced.
    // Replanning validates these bounds as the whole Boundary-delimited owner.
    // Inspect just that slice: unrelated rows need no format lookup or repeated
    // projection rebuild, and a large note still takes one text scan here.
    if original
        .text
        .get(plan.line_start..plan.newline)
        .ok_or(HtmlEditError::InvalidTextRange)?
        .contains('\n')
        || !original.spans.iter().any(|span| {
            span.range.start == plan.newline
                && matches!(&span.kind, HtmlCanvasKind::Boundary { left, .. } if left.source_range == owner.source_range)
        })
    {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }

    let caret = plan.prefix_range.end;
    let mut space_expected = original.text.clone();
    space_expected.insert(caret, ' ');
    let mut space_document = document.clone();
    if space_document.replace_canvas_range(caret..caret, " ")? != caret + 1 {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    for setting in pending {
        super::apply_canvas_inline_mark(
            &mut space_document,
            caret..caret + 1,
            setting.mark.clone(),
            setting.enabled,
        )?;
    }
    if space_document.canvas_projection().text != space_expected {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }

    let mut expected = original.text.clone();
    expected.replace_range(plan.prefix_range.clone(), "");
    let mut candidate = space_document.clone();
    if candidate.replace_canvas_range(plan.prefix_range.start..caret + 1, "")?
        != plan.prefix_range.start
        || candidate.canvas_projection().text != expected
    {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    // Quill replaces mutually exclusive block blots when applying a list. Clear
    // only this source owner via the existing safe isolation helpers; retain its
    // indentation, payload attributes, comments and unselected siblings.
    let mut neutral = false;
    for _ in 0..8 {
        let (current_owner, current) = owner_at(&candidate, plan.line_start)?;
        let cancel = if current.code {
            Some(HtmlBlockMark::CodeBlock)
        } else if let HtmlBlockKind::Header(level) = current.kind {
            Some(HtmlBlockMark::Header(level))
        } else if current.quote {
            Some(HtmlBlockMark::Blockquote)
        } else {
            None
        };
        let Some(cancel) = cancel else {
            neutral = true;
            break;
        };
        candidate.toggle_canvas_block(&current_owner, cancel)?;
        if candidate.canvas_projection().text != expected {
            return Err(HtmlEditError::UnsupportedBlockStructure);
        }
    }
    if !neutral {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    let (current_owner, current) = owner_at(&candidate, plan.line_start)?;
    if current.list.is_some() {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    candidate.toggle_canvas_block(&current_owner, HtmlBlockMark::List(plan.kind))?;
    let (_, final_format) = owner_at(&candidate, plan.line_start)?;
    if final_format.list != Some(plan.kind)
        || final_format.code
        || final_format.quote
        || matches!(final_format.kind, HtmlBlockKind::Header(_))
        || final_format.indent != format.indent
        || candidate.canvas_projection().text != expected
    {
        return Err(HtmlEditError::UnsupportedBlockStructure);
    }
    let undo_caret = plan.newline + 1;
    let redo_caret = plan.newline - plan.prefix_range.len();
    if space_expected.as_bytes().get(undo_caret) != Some(&b'\n')
        || expected.as_bytes().get(redo_caret) != Some(&b'\n')
        || !space_expected.is_char_boundary(undo_caret)
        || !expected.is_char_boundary(redo_caret)
        || !plan.selection.is_empty()
        || plan.selection.start != plan.line_start
        || plan.selection.end > expected.len().saturating_sub(1)
        || !expected.is_char_boundary(plan.selection.start)
    {
        return Err(HtmlEditError::InvalidTextRange);
    }
    Ok(HtmlListAutofillEdit {
        space_document,
        document: candidate,
        selection: plan.selection.clone(),
        undo_caret,
        redo_caret,
    })
}

fn owner_at(
    document: &HtmlDocument,
    offset: usize,
) -> Result<(HtmlCanvasBlock, HtmlBlock), HtmlEditError> {
    let owner = super::canvas_block_owners(document, offset..offset)
        .into_iter()
        .next()
        .ok_or(HtmlEditError::MissingBlock)?;
    let format = document
        .canvas_block_format(&owner)
        .ok_or(HtmlEditError::MissingBlock)?;
    Ok((owner, format))
}

#[cfg(test)]
mod tests {
    use super::super::{HtmlListKind, HtmlMark};
    use super::*;

    fn planned(document: &HtmlDocument, caret: usize) -> HtmlListAutofillPlan {
        plan_html_list_autofill(document, caret..caret, " ", true)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn bullet_with_unicode_suffix_returns_two_snapshots_and_history_line_ends() {
        let source = "<p vendor='keep'><span data-u='keep'>-尾&#x1F600;</span><!--stay--></p>";
        let original = HtmlDocument::parse(source);
        let plan = planned(&original, 1);
        let edit = apply_html_list_autofill(&original, &plan, &[]).unwrap();
        assert_eq!(original.source(), source);
        assert_eq!(edit.space_document.canvas_projection().text, "- 尾😀\n");
        assert_eq!(edit.document.canvas_projection().text, "尾😀\n");
        assert_eq!(edit.selection, 0..0);
        assert_eq!(edit.undo_caret, "- 尾😀".len());
        assert_eq!(edit.redo_caret, "尾😀".len());
        assert_eq!(
            owner_at(&edit.document, 0).unwrap().1.list,
            Some(HtmlListKind::Bullet)
        );
        for preserved in ["vendor='keep'", "data-u='keep'", "&#x1F600;", "<!--stay-->"] {
            assert!(edit.document.source().contains(preserved), "{preserved}");
        }
    }

    #[test]
    fn ordered_marker_removes_all_leading_marker_whitespace_and_keeps_indent() {
        let source = "<p class='ql-indent-2' data-u='keep'> \t12.X</p>";
        let original = HtmlDocument::parse(source);
        let plan = planned(&original, 5);
        let edit = apply_html_list_autofill(&original, &plan, &[]).unwrap();
        assert_eq!(edit.space_document.canvas_projection().text, " \t12. X\n");
        assert_eq!(edit.document.canvas_projection().text, "X\n");
        let format = owner_at(&edit.document, 0).unwrap().1;
        assert_eq!(format.list, Some(HtmlListKind::Ordered));
        assert_eq!(format.indent, 2);
        assert_eq!((edit.undo_caret, edit.redo_caret), (7, 1));
        assert!(edit.document.source().contains("data-u='keep'"));
    }

    #[test]
    fn pending_inline_format_applies_only_to_the_intermediate_space() {
        let original = HtmlDocument::parse("<p data-u='keep'>-tail</p>");
        let plan = planned(&original, 1);
        let pending = [PendingFormat {
            mark: HtmlMark::Bold,
            enabled: true,
        }];
        let edit = apply_html_list_autofill(&original, &plan, &pending).unwrap();
        let space = edit
            .space_document
            .runs()
            .iter()
            .find(|run| run.text == " ")
            .unwrap();
        assert!(space.style.bold);
        assert!(edit
            .space_document
            .runs()
            .iter()
            .filter(|run| run.text != " ")
            .all(|run| !run.style.bold));
        assert_eq!(edit.document.canvas_projection().text, "tail\n");
        assert!(edit.document.runs().iter().all(|run| !run.style.bold));
    }

    #[test]
    fn header_quote_and_single_line_code_are_replaced_by_list_without_flattening() {
        for source in [
            "<h2 vendor='keep'><span data-u='keep'>-tail</span></h2>",
            "<blockquote vendor='keep'><p data-u='keep'>-tail</p></blockquote>",
            "<pre vendor='keep'><span data-u='keep'>-tail</span></pre>",
        ] {
            let original = HtmlDocument::parse(source);
            let plan = planned(&original, 1);
            let edit = apply_html_list_autofill(&original, &plan, &[]).unwrap();
            assert_eq!(edit.document.canvas_projection().text, "tail\n");
            let format = owner_at(&edit.document, 0).unwrap().1;
            assert_eq!(format.list, Some(HtmlListKind::Bullet));
            assert!(
                !format.code && !format.quote && !matches!(format.kind, HtmlBlockKind::Header(_))
            );
            assert!(edit.document.source().contains("vendor='keep'"));
            assert!(edit.document.source().contains("data-u='keep'"));
            assert_eq!(original.source(), source);
        }
    }

    #[test]
    fn shared_pre_br_and_nested_container_refuse_without_exposing_space_snapshot() {
        for source in [
            "<pre vendor='keep'>-\ntail</pre>",
            "<p vendor='keep'>-<br>tail</p>",
            "<div vendor='keep'>-<p>child</p>tail</div>",
        ] {
            let original = HtmlDocument::parse(source);
            let plan = planned(&original, 1);
            assert!(apply_html_list_autofill(&original, &plan, &[]).is_err());
            assert_eq!(original.source(), source);
        }
    }

    #[test]
    fn stale_prefix_kind_newline_or_selection_is_revalidated_before_source_changes() {
        let source = "<p data-u='keep'>-tail</p>";
        let original = HtmlDocument::parse(source);
        let good = planned(&original, 1);
        let mut invalid = vec![good.clone(); 4];
        invalid[0].prefix_range.start = 1;
        invalid[1].kind = HtmlListKind::Ordered;
        invalid[2].newline += 1;
        invalid[3].selection = 99..99;
        for plan in invalid {
            assert!(apply_html_list_autofill(&original, &plan, &[]).is_err());
            assert_eq!(original.source(), source);
        }
    }

    #[test]
    fn pending_format_failure_keeps_the_complete_original_source() {
        let source = "<p vendor='keep'><span data-u='keep'>-tail</span><!--stay--></p>";
        let original = HtmlDocument::parse(source);
        let plan = planned(&original, 1);
        let pending = [PendingFormat {
            mark: HtmlMark::Color("expression(x)".into()),
            enabled: true,
        }];
        assert!(apply_html_list_autofill(&original, &plan, &pending).is_err());
        assert_eq!(original.source(), source);
    }
}
