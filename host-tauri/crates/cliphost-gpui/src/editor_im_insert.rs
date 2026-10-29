//! Exact IM canvas atom insertion. The canvas text is an address projection;
//! only the selected XML character data or complete selected objects change.

use super::*;

/// Replace a canvas selection with one image/file object. All offsets and the
/// returned caret are UTF-8 byte offsets; U+FFFC occupies three bytes.
/// Validation, deletion, source splitting and reparse operate on local clones.
/// No failure can delete the original selection or otherwise change its source.
pub(super) fn insert_im_canvas_atom(
    document: &mut ImRichDocument,
    selection: Range<usize>,
    image: bool,
    path: &str,
) -> Result<usize, String> {
    let original = im_canvas_projection(document);
    if selection.start > selection.end
        || !original.text.is_char_boundary(selection.start)
        || !original.text.is_char_boundary(selection.end)
    {
        return Err("Invalid IM canvas selection".into());
    }

    // Use the existing core validator and origin-specific type codes before
    // touching the selection. This generated node is not kept in the candidate.
    let mut validated = document.clone();
    let atom_id = insert_atom(&mut validated, None, image, path)?;
    let atom_xml = validated
        .node(atom_id)
        .ok_or("Inserted IM object no longer exists")?
        .raw_xml()
        .to_string();

    // Keep the original decoded text anchor. Deleting X from "\rX\n" makes
    // the candidate project "\r\n" as one LF before the atom is inserted;
    // locating the caret in that temporary projection would be one byte late.
    // Range removal retains every text element, and removes only whole atoms.
    let text_anchor = original.spans.iter().find_map(|span| match span.kind {
        ImCanvasKind::Text(id)
            if span.range.start <= selection.start && selection.start <= span.range.end =>
        {
            Some((span, id))
        }
        _ => None,
    });
    let text_anchor = if let Some((span, id)) = text_anchor {
        let node = document.node(id).ok_or("Text object no longer exists")?;
        let offset = canvas_offset_in_raw_text(node.text(), selection.start - span.range.start)
            .ok_or("Invalid normalized text insertion position")?;
        let index = document
            .nodes()
            .iter()
            .position(|node| node.id() == id)
            .ok_or("Text object no longer exists")?;
        let removed_before = original
            .spans
            .iter()
            .filter(|other| {
                matches!(other.kind, ImCanvasKind::Atom { .. })
                    && other.source.start < span.source.start
                    && selection.start < other.range.end
                    && selection.end > other.range.start
            })
            .count();
        Some((
            index - removed_before,
            offset,
            node.text()[..offset].to_string(),
        ))
    } else {
        None
    };

    let mut candidate = document.clone();
    let caret = replace_im_canvas_range(&mut candidate, selection.clone(), "")?;
    let remaining = im_canvas_projection(&candidate);
    if let Some((index, offset, prefix)) = text_anchor {
        let node = candidate
            .nodes()
            .get(index)
            .ok_or("Text object no longer exists")?;
        if !node.can_edit_text() || node.text().get(..offset) != Some(prefix.as_str()) {
            return Err("Text insertion anchor no longer matches its original prefix".into());
        }
        if offset == 0 {
            let before = Some(node.id());
            insert_atom(&mut candidate, before, image, path)?;
        } else if offset == node.text().len() {
            let before = candidate.nodes().get(index + 1).map(|node| node.id());
            insert_atom(&mut candidate, before, image, path)?;
        } else {
            let span = remaining
                .spans
                .iter()
                .find(|span| matches!(span.kind, ImCanvasKind::Text(id) if id == node.id()))
                .ok_or("Text object no longer has a canvas source span")?;
            let replacement =
                split_text_around_atom(node.raw_xml(), node.text(), offset, &atom_xml)?;
            let mut source = candidate.serialize();
            source.replace_range(span.source.clone(), &replacement);
            candidate = ImRichDocument::parse(&source, candidate.origin())
                .map_err(|error| error.to_string())?;
        }
    } else {
        // At an object/text boundary, insert beside the original node without
        // rebuilding it. When there is only metadata, core appends after it.
        let before = remaining
            .spans
            .iter()
            .find(|span| span.range.start >= caret)
            .map(|span| span_id(&span.kind))
            .or_else(|| {
                let last_id = remaining.spans.last().map(|span| span_id(&span.kind))?;
                let index = candidate
                    .nodes()
                    .iter()
                    .position(|node| node.id() == last_id)?;
                candidate.nodes().get(index + 1).map(|node| node.id())
            });
        insert_atom(&mut candidate, before, image, path)?;
    }

    // Core insertion at an empty/self-closing root may expand its envelope.
    // Always validate the final locally patched source, not a synthetic DOM.
    let source = candidate.serialize();
    let final_document =
        ImRichDocument::parse(&source, document.origin()).map_err(|error| error.to_string())?;
    let expected = format!(
        "{}{}{}",
        &original.text[..selection.start],
        CANVAS_OBJECT,
        &original.text[selection.end..]
    );
    if im_canvas_projection(&final_document).text != expected {
        return Err("IM object insertion could not preserve the surrounding canvas text".into());
    }
    *document = final_document;
    Ok(selection.start + CANVAS_OBJECT.len())
}

fn insert_atom(
    document: &mut ImRichDocument,
    before: Option<RichNodeId>,
    image: bool,
    path: &str,
) -> Result<RichNodeId, String> {
    if image {
        document.insert_image(before, path)
    } else {
        document.insert_file(before, path)
    }
    .map_err(|error| error.to_string())
}

fn span_id(kind: &ImCanvasKind) -> RichNodeId {
    match *kind {
        ImCanvasKind::Text(id) | ImCanvasKind::Atom { id, .. } => id,
    }
}

/// Split one already validated editable text node at a decoded character
/// boundary. Only the tag pair is cloned; inner comments/instructions remain
/// once at their original side of the lexical split. Untouched entity spelling
/// and literal CRLF/ZWSP are copied as raw XML, never re-encoded.
fn split_text_around_atom(
    raw: &str,
    text: &str,
    offset: usize,
    atom: &str,
) -> Result<String, String> {
    if offset == 0 || offset == text.len() || !text.is_char_boundary(offset) {
        return Err("Text object must be split at an interior character boundary".into());
    }
    let opening_end = xml_opening_end(raw, 0).ok_or("Invalid text object opening tag")?;
    let closing_start = raw.rfind("</").ok_or("Text object has no closing tag")?;
    let parts = xml_canvas_parts(raw, text)?;
    let part = parts
        .iter()
        .find(|part| part.decoded.start <= offset && offset <= part.decoded.end)
        .ok_or("Text object has no source at this insertion position")?;
    let local = offset - part.decoded.start;
    let (split, close_cdata, open_cdata) = if part.cdata {
        if local == 0 {
            (part.raw.start - "<![CDATA[".len(), "", "")
        } else if local == part.text.len() {
            (part.raw.end + "]]>".len(), "", "")
        } else {
            (part.raw.start + local, "]]>", "<![CDATA[")
        }
    } else if part.entity {
        if local == 0 {
            (part.raw.start, "", "")
        } else if local == part.text.len() {
            (part.raw.end, "", "")
        } else {
            return Err("An XML character reference cannot be split".into());
        }
    } else {
        (part.raw.start + local, "", "")
    };
    if split < opening_end || split > closing_start || !raw.is_char_boundary(split) {
        return Err("Invalid XML source insertion position".into());
    }
    Ok(format!(
        "{}{}{}{}{}{}{}{}{}",
        &raw[..opening_end],
        &raw[opening_end..split],
        close_cdata,
        &raw[closing_start..],
        atom,
        &raw[..opening_end],
        open_cdata,
        &raw[split..closing_start],
        &raw[closing_start..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(raw: &str) -> ImRichDocument {
        ImRichDocument::parse(raw, RichOrigin::Qq).unwrap()
    }

    fn insert_at_every_boundary(raw: &str, image: bool) {
        let original = document(raw);
        let text = im_canvas_projection(&original).text;
        for at in text.char_indices().map(|(at, _)| at).chain([text.len()]) {
            let mut edited = original.clone();
            let caret = insert_im_canvas_atom(&mut edited, at..at, image, "C:\\a&b.png").unwrap();
            assert_eq!(caret, at + 3);
            assert_eq!(
                im_canvas_projection(&edited).text,
                format!("{}{CANVAS_OBJECT}{}", &text[..at], &text[at..])
            );
            assert_eq!(edited.origin(), original.origin());
        }
    }

    #[test]
    fn atom_splits_plain_unicode_text_without_touching_entities_or_vendor_attributes() {
        let raw = "prefix<QQRichEditFormat vendor='root'><Info version='1001'/><EditElement type = '0' vendor='text'>A&#x4e2d;😀&amp;B</EditElement><Unknown/></QQRichEditFormat>suffix";
        insert_at_every_boundary(raw, true);
        let mut edited = document(raw);
        let at = "A中".len();
        assert_eq!(
            insert_im_canvas_atom(&mut edited, at..at, true, "img.png").unwrap(),
            at + 3
        );
        assert_eq!(edited.serialize(), "prefix<QQRichEditFormat vendor='root'><Info version='1001'/><EditElement type = '0' vendor='text'>A&#x4e2d;</EditElement><EditElement type=\"3\" filepath=\"img.png\" /><EditElement type = '0' vendor='text'>😀&amp;B</EditElement><Unknown/></QQRichEditFormat>suffix");
    }

    #[test]
    fn cdata_split_closes_and_reopens_only_the_character_section() {
        let raw = "<QQRichEditFormat><EditElement type='0' vendor='keep'><!--before--><![CDATA[A\r\n中\u{200b}😀]]><?after keep?></EditElement></QQRichEditFormat>";
        insert_at_every_boundary(raw, false);
        let mut edited = document(raw);
        let at = "A\n中".len();
        insert_im_canvas_atom(&mut edited, at..at, false, "file.txt").unwrap();
        assert_eq!(edited.serialize(), "<QQRichEditFormat><EditElement type='0' vendor='keep'><!--before--><![CDATA[A\r\n中]]></EditElement><EditElement type=\"4\" filepath=\"file.txt\" /><EditElement type='0' vendor='keep'><![CDATA[\u{200b}😀]]><?after keep?></EditElement></QQRichEditFormat>");
        assert_eq!(edited.serialize().matches("<!--before-->").count(), 1);
        assert_eq!(edited.serialize().matches("<?after keep?>").count(), 1);
    }

    #[test]
    fn multipart_boundaries_keep_all_inner_metadata_once_and_cdata_delimiter_payload() {
        let raw = "<QQRichEditFormat><EditElement type='0' vendor='x'>A&#13;&#10;<!--first--><?vendor x?><![CDATA[中]]]]><![CDATA[>B]]>&amp;C<!--last--></EditElement></QQRichEditFormat>";
        insert_at_every_boundary(raw, true);
        let original = document(raw);
        let text = im_canvas_projection(&original).text;
        for at in text.char_indices().map(|(at, _)| at).chain([text.len()]) {
            let mut edited = original.clone();
            insert_im_canvas_atom(&mut edited, at..at, true, "img.png").unwrap();
            let xml = edited.serialize();
            assert!(xml.contains("&#13;&#10;"));
            // At the boundary after '&', the two characters belong to the
            // split siblings; the untouched entity token itself must survive.
            assert_eq!(xml.matches("&amp;").count(), 1);
            assert_eq!(xml.matches("<!--first-->").count(), 1);
            assert_eq!(xml.matches("<?vendor x?>").count(), 1);
            assert_eq!(xml.matches("<!--last-->").count(), 1);
        }
    }

    #[test]
    fn replacement_across_text_and_atoms_removes_only_selection() {
        let raw = "before<QQRichEditFormat vendor='root'><Info keep='yes'/><EditElement type='0' vendor='left'>AB&amp;C</EditElement><EditElement type='3' filepath='old.png' keep='old'/><EditElement type='4' filepath='old.txt'/><EditElement type='0' vendor='right'><![CDATA[D\r\nE\u{200b}F]]><!--inner--></EditElement><Unknown/></QQRichEditFormat>after";
        let mut edited = document(raw);
        let text = im_canvas_projection(&edited).text;
        let end = text.find('E').unwrap();
        let caret = insert_im_canvas_atom(&mut edited, 1..end, false, "new.txt").unwrap();
        assert_eq!(caret, 4);
        assert_eq!(
            im_canvas_projection(&edited).text,
            format!("A{CANVAS_OBJECT}E\u{200b}F{CANVAS_OBJECT}")
        );
        let xml = edited.serialize();
        assert!(xml.starts_with("before<QQRichEditFormat vendor='root'><Info keep='yes'/>"));
        assert!(xml.contains("vendor='left'>A</EditElement>"));
        assert!(xml.contains("vendor='right'><![CDATA[E\u{200b}F]]><!--inner-->"));
        assert!(!xml.contains("old.png") && !xml.contains("old.txt"));
        assert!(xml.ends_with("<Unknown/></QQRichEditFormat>after"));
    }

    #[test]
    fn atom_and_text_edges_retain_original_raw_nodes_and_do_not_add_separators() {
        let raw = "<QQRichEditFormat><Info keep='yes'/><EditElement type='3' filepath='first.png' vendor='atom'/><!--between--><EditElement type='0' vendor='text'>AB</EditElement><EditElement type='4' filepath='last.txt'/><!--tail--></QQRichEditFormat>";
        let original = document(raw);
        let untouched = original
            .content_nodes()
            .map(|node| node.raw_xml().to_string())
            .collect::<Vec<_>>();
        for at in [0, 3, 5, 8] {
            let mut edited = original.clone();
            insert_im_canvas_atom(&mut edited, at..at, true, "new.png").unwrap();
            for node in &untouched {
                assert!(edited.serialize().contains(node));
            }
            assert_eq!(edited.serialize().matches("<!--between-->").count(), 1);
            assert_eq!(edited.serialize().matches("<!--tail-->").count(), 1);
            assert!(!im_canvas_projection(&edited).text.contains('\n'));
        }
    }

    #[test]
    fn empty_text_metadata_only_and_self_closing_roots_accept_atoms() {
        for raw in [
            "prefix<QQRichEditFormat/>suffix",
            "<QQRichEditFormat><Info keep='yes'/><!--keep--></QQRichEditFormat>",
            "<QQRichEditFormat><EditElement type='0' vendor='empty'/></QQRichEditFormat>",
            "<QQRichEditFormat><EditElement type='0'><!--keep--></EditElement></QQRichEditFormat>",
        ] {
            let mut edited = document(raw);
            assert_eq!(
                insert_im_canvas_atom(&mut edited, 0..0, false, "file.txt").unwrap(),
                3
            );
            assert_eq!(im_canvas_projection(&edited).text, CANVAS_OBJECT);
            assert!(edited.serialize().contains("filepath=\"file.txt\""));
            if raw.starts_with("prefix") {
                assert!(
                    edited.serialize().starts_with("prefix")
                        && edited.serialize().ends_with("suffix")
                );
            }
            if raw.contains("<!--keep-->") {
                assert_eq!(edited.serialize().matches("<!--keep-->").count(), 1);
            }
            if raw.contains("vendor='empty'") {
                assert!(edited
                    .serialize()
                    .contains("<EditElement type='0' vendor='empty'/>"));
            }
        }
    }

    #[test]
    fn wechat_and_wework_insert_their_image_and_file_codes_at_exact_unicode_caret() {
        for origin in [RichOrigin::Wechat, RichOrigin::Wework] {
            for image in [true, false] {
                let mut edited = ImRichDocument::parse("<WeChatRichEditFormat vendor='root'><EditElement type='0'>中😀文</EditElement></WeChatRichEditFormat>", origin).unwrap();
                insert_im_canvas_atom(&mut edited, 3..3, image, "C:\\a'&\"b").unwrap();
                let atom = edited
                    .content_nodes()
                    .find(|node| {
                        node.kind()
                            == if image {
                                RichNodeKind::Image
                            } else {
                                RichNodeKind::File
                            }
                    })
                    .unwrap();
                assert!(atom
                    .raw_xml()
                    .contains(if image { "type=\"1\"" } else { "type=\"3\"" }));
                assert_eq!(atom.path(), "C:\\a'&\"b");
                assert_eq!(edited.origin(), origin);
                assert_eq!(
                    im_canvas_projection(&edited).text,
                    format!("中{CANVAS_OBJECT}😀文")
                );
            }
        }
    }

    #[test]
    fn protected_objects_invalid_ranges_and_paths_leave_the_whole_source_unchanged() {
        let raw = "prefix<QQRichEditFormat><EditElement type='0'>中A</EditElement><Unknown/><EditElement type='0'><Private/></EditElement><EditElement type='0'>&private;</EditElement><EditElement type='0'>B</EditElement></QQRichEditFormat>suffix";
        let original = document(raw);
        let text = im_canvas_projection(&original).text;
        for (range, path) in [
            (0..text.len(), "ok.png"),
            (4..7, "ok.png"),
            (7..10, "ok.png"),
            (10..13, "ok.png"),
            (0..1, "ok.png"),
            (8..8, "ok.png"),
            (text.len() + 1..text.len() + 1, "ok.png"),
            (4..3, "ok.png"),
            (0..3, "  "),
            (0..3, "bad\0path"),
        ] {
            let mut edited = original.clone();
            assert!(insert_im_canvas_atom(&mut edited, range, true, path).is_err());
            assert_eq!(edited.serialize(), raw);
            assert_eq!(edited.original_xml(), original.original_xml());
        }
        // Insertion beside an unknown object is safe; selecting it is not.
        let mut edited = original.clone();
        insert_im_canvas_atom(&mut edited, 4..4, true, "ok.png").unwrap();
        assert!(edited.serialize().contains("<Unknown/>"));
    }

    #[test]
    fn temporary_crlf_join_after_deletion_does_not_move_original_character_anchor() {
        for body in [
            "<![CDATA[A\rX\nB]]>",
            "A&#13;X&#10;B",
            "A\r<!--keep-->X<?keep x?>\nB",
        ] {
            let raw = format!("<QQRichEditFormat><EditElement type='0' vendor='keep'>{body}</EditElement></QQRichEditFormat>");
            let mut edited = document(&raw);
            assert_eq!(im_canvas_projection(&edited).text, "A\rX\nB");
            assert_eq!(
                insert_im_canvas_atom(&mut edited, 2..3, true, "a.png").unwrap(),
                5
            );
            assert_eq!(
                im_canvas_projection(&edited).text,
                format!("A\r{CANVAS_OBJECT}\nB")
            );
            let nodes = edited.content_nodes().collect::<Vec<_>>();
            assert_eq!(nodes[0].text(), "A\r");
            assert_eq!(nodes[2].text(), "\nB");
            if body.contains("&#13;") {
                assert!(edited.serialize().contains("&#13;"));
            }
            if body.contains("<!--keep-->") {
                assert_eq!(edited.serialize().matches("<!--keep-->").count(), 1);
                assert_eq!(edited.serialize().matches("<?keep x?>").count(), 1);
            }
        }
    }

    #[test]
    fn every_valid_mixed_canvas_range_inserts_exactly_or_preserves_protected_source() {
        let raw = "prefix<QQRichEditFormat vendor='root'><Info keep='1'/><EditElement type='0' vendor='text'>A&#x4e2d;\rX\n&amp;B<!--keep--></EditElement><EditElement type='3' filepath='old'/><EditElement type='0'><![CDATA[C\r\n😀D]]></EditElement><Unknown keep='protected'/></QQRichEditFormat>suffix";
        let original = document(raw);
        let projection = im_canvas_projection(&original);
        let boundaries = projection
            .text
            .char_indices()
            .map(|(at, _)| at)
            .chain([projection.text.len()])
            .collect::<Vec<_>>();
        for (index, &from) in boundaries.iter().enumerate() {
            for &to in &boundaries[index..] {
                let mut edited = original.clone();
                let protected = projection.spans.iter().any(|span| {
                    matches!(
                        span.kind,
                        ImCanvasKind::Atom {
                            protected: true,
                            ..
                        }
                    ) && from < span.range.end
                        && to > span.range.start
                });
                let result = insert_im_canvas_atom(&mut edited, from..to, true, "new.png");
                if protected {
                    assert!(result.is_err(), "protected range {from}..{to}");
                    assert_eq!(edited.serialize(), raw);
                } else {
                    assert_eq!(result.unwrap(), from + 3, "range {from}..{to}");
                    assert_eq!(
                        im_canvas_projection(&edited).text,
                        format!(
                            "{}{CANVAS_OBJECT}{}",
                            &projection.text[..from],
                            &projection.text[to..]
                        ),
                        "range {from}..{to}"
                    );
                    assert!(edited.serialize().contains("<Unknown keep='protected'/>"));
                    assert_eq!(edited.serialize().matches("<!--keep-->").count(), 1);
                    assert!(edited
                        .serialize()
                        .starts_with("prefix<QQRichEditFormat vendor='root'><Info keep='1'/>"));
                    assert!(edited.serialize().ends_with("</QQRichEditFormat>suffix"));
                }
            }
        }
    }
}
