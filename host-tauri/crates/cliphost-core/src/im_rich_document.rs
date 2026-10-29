//! Shared lossless QQ / WeChat rich-edit document for core persistence and native editors.
//!
//! The preview DTO is deliberately not the source of truth: it omits unknown
//! elements and metadata. Keep the original XML and patch only a selected
//! EditElement. No parser tree is serialized, no images are copied/deleted and
//! no filesystem or clipboard access happens in this module.

use std::{fmt, ops::Range};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RichOrigin {
    Qq,
    Wechat,
    Wework,
    Unknown,
}

impl RichOrigin {
    pub fn from_token(token: &str) -> Self {
        match token.trim().to_ascii_lowercase().as_str() {
            "qq" => Self::Qq,
            "wechat" => Self::Wechat,
            "wework" => Self::Wework,
            _ => Self::Unknown,
        }
    }

    pub fn as_token(self) -> &'static str {
        match self {
            Self::Qq => "qq",
            Self::Wechat => "wechat",
            Self::Wework => "wework",
            Self::Unknown => "unknown",
        }
    }

    fn uses_wechat_codes(self) -> bool {
        matches!(self, Self::Wechat | Self::Wework)
    }
}

/// Stable for the lifetime of one document, including moves and local edits.
/// Reparse after a source edit creates a new document; do not reuse its IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RichNodeId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RichNodeKind {
    Text,
    Image,
    File,
    Placeholder,
    /// Unknown EditElement or other XML. Kept verbatim, never implicitly edited.
    Opaque,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RichRewriteReport {
    pub converted_nodes: usize,
    /// No cross-family protocol mapping is known for these complete objects.
    pub preserved_unmapped_nodes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RichEditError {
    MissingEnvelope,
    MalformedXml { offset: usize, reason: &'static str },
    UnknownNode(RichNodeId),
    UnsupportedNode(RichNodeId),
    UnsupportedTextMarkup(RichNodeId),
    InvalidXmlCharacter,
    EmptyPath,
}

impl fmt::Display for RichEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnvelope => f.write_str("QQ/WeChat rich-edit envelope was not found"),
            Self::MalformedXml { offset, reason } => {
                write!(f, "Invalid rich-edit XML at byte {offset}: {reason}")
            }
            Self::UnknownNode(id) => write!(f, "Rich-edit node {} no longer exists", id.0),
            Self::UnsupportedNode(id) => write!(
                f,
                "Rich-edit node {} cannot be changed by this control",
                id.0
            ),
            Self::UnsupportedTextMarkup(id) => write!(
                f,
                "Rich-edit text node {} contains unsupported nested XML; edit its source instead",
                id.0
            ),
            Self::InvalidXmlCharacter => {
                f.write_str("Input contains a character that XML 1.0 cannot represent")
            }
            Self::EmptyPath => f.write_str("Image/file path cannot be empty"),
        }
    }
}

impl std::error::Error for RichEditError {}

#[derive(Clone, Debug)]
pub struct RichNode {
    id: RichNodeId,
    kind: RichNodeKind,
    raw: String,
    text: String,
    path: String,
    metadata: bool,
    opening: Option<Opening>,
    body: Option<Range<usize>>,
    text_parts: Vec<TextPart>,
    text_editable: bool,
}

impl RichNode {
    pub fn id(&self) -> RichNodeId {
        self.id
    }
    pub fn kind(&self) -> RichNodeKind {
        self.kind
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn raw_xml(&self) -> &str {
        &self.raw
    }
    /// Whitespace/comments/Info belong to the payload, but are not editor rows.
    pub fn is_metadata(&self) -> bool {
        self.metadata
    }
    pub fn can_edit_text(&self) -> bool {
        self.kind == RichNodeKind::Text && self.text_editable
    }
    pub fn can_edit_path(&self) -> bool {
        matches!(self.kind, RichNodeKind::Image | RichNodeKind::File)
    }
}

#[derive(Clone, Debug)]
pub struct ImRichDocument {
    original: String,
    prefix: String,
    root_open: String,
    root_close: String,
    suffix: String,
    root_name: String,
    root_self_closing: Option<usize>,
    origin: RichOrigin,
    nodes: Vec<RichNode>,
    next_id: u64,
}

impl ImRichDocument {
    pub fn parse(raw: &str, origin: RichOrigin) -> Result<Self, RichEditError> {
        let mut cursor = 0;
        let root = loop {
            let Some(token) = next_token(raw, &mut cursor)? else {
                return Err(RichEditError::MissingEnvelope);
            };
            if let TokenKind::Start(opening) = token.kind {
                if matches!(
                    opening.name.as_str(),
                    "QQRichEditFormat" | "WeChatRichEditFormat"
                ) {
                    break opening;
                }
            }
        };
        let parsed = parse_element(raw, root.clone())?;
        let origin = if root.name == "WeChatRichEditFormat" {
            if origin == RichOrigin::Wework {
                origin
            } else {
                RichOrigin::Wechat
            }
        } else if origin == RichOrigin::Unknown {
            infer_qq_origin(raw, &parsed.children)?
        } else {
            origin
        };
        let mut nodes = Vec::new();
        let mut next_id = 1;
        for range in parsed.children {
            nodes.push(parse_node(&raw[range], RichNodeId(next_id), origin)?);
            next_id += 1;
        }
        Ok(Self {
            original: raw.to_owned(),
            prefix: raw[..root.range.start].to_owned(),
            root_open: raw[root.range.clone()].to_owned(),
            root_close: raw[parsed.close.clone()].to_owned(),
            suffix: raw[parsed.end..].to_owned(),
            root_name: root.name,
            root_self_closing: root.slash.map(|slash| slash - root.range.start),
            origin,
            nodes,
            next_id,
        })
    }

    pub fn original_xml(&self) -> &str {
        &self.original
    }

    /// Byte span of the validated envelope in the original source. This keeps
    /// callers from finding fake roots inside comments/CDATA or cutting at a
    /// close-tag string embedded in a text object's character data.
    pub fn envelope_range(&self) -> Range<usize> {
        self.prefix.len()..self.original.len() - self.suffix.len()
    }
    pub fn origin(&self) -> RichOrigin {
        self.origin
    }
    pub fn nodes(&self) -> &[RichNode] {
        &self.nodes
    }
    pub fn content_nodes(&self) -> impl Iterator<Item = &RichNode> {
        self.nodes.iter().filter(|node| !node.metadata)
    }
    pub fn node(&self, id: RichNodeId) -> Option<&RichNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Byte-identical when untouched, including declarations, comments, unknown
    /// nodes, attribute quoting/order, whitespace and data outside the envelope.
    pub fn serialize(&self) -> String {
        let mut xml = self.prefix.clone();
        xml.push_str(&self.root_open);
        for node in &self.nodes {
            xml.push_str(&node.raw);
        }
        xml.push_str(&self.root_close);
        xml.push_str(&self.suffix);
        xml
    }

    pub fn is_changed(&self) -> bool {
        self.serialize() != self.original
    }

    /// Change only the known envelope name and direct EditElement type values.
    /// Same-family output remains byte-identical, including legacy QQ wrappers
    /// carrying WeChat codes. Unknown objects and emoji placeholders are never
    /// converted into preview text or assigned an invented destination type.
    pub fn rewrite_origin(
        &mut self,
        target: RichOrigin,
    ) -> Result<RichRewriteReport, RichEditError> {
        let target = if target == RichOrigin::Unknown {
            RichOrigin::Qq
        } else {
            target
        };
        if self.origin.uses_wechat_codes() == target.uses_wechat_codes() {
            self.origin = target;
            return Ok(RichRewriteReport::default());
        }

        let mut report = RichRewriteReport::default();
        let mut nodes = self.nodes.clone();
        for node in &mut nodes {
            let code = match node.kind {
                RichNodeKind::Text => Some(0),
                RichNodeKind::Image => Some(if target.uses_wechat_codes() { 1 } else { 3 }),
                RichNodeKind::File => Some(if target.uses_wechat_codes() { 3 } else { 4 }),
                RichNodeKind::Placeholder | RichNodeKind::Opaque => {
                    if !node.metadata {
                        report.preserved_unmapped_nodes += 1;
                    }
                    None
                }
            };
            let Some(code) = code else { continue };
            let Some(attribute) = node.opening.as_ref().and_then(|opening| {
                opening
                    .attrs
                    .iter()
                    .find(|attribute| attribute.name == "type")
            }) else {
                continue;
            };
            let current = decode_xml(&node.raw[attribute.value.clone()])
                .and_then(|value| value.trim().parse::<u8>().ok());
            if current == Some(code) {
                continue;
            }
            let mut raw = node.raw.clone();
            raw.replace_range(attribute.value.clone(), &code.to_string());
            // Reparse only the modified object so IDs and editor span-based
            // path/text controls still describe its current lexical source.
            *node = parse_node(&raw, node.id, target)?;
            report.converted_nodes += 1;
        }

        let root_name = if target.uses_wechat_codes() {
            "WeChatRichEditFormat"
        } else {
            "QQRichEditFormat"
        };
        let mut root_open = self.root_open.clone();
        root_open.replace_range(1..1 + self.root_name.len(), root_name);
        let mut root_close = self.root_close.clone();
        if !root_close.is_empty() {
            let start = root_close
                .find(&self.root_name)
                .ok_or_else(|| xml_error(0, "validated envelope closing name no longer exists"))?;
            root_close.replace_range(start..start + self.root_name.len(), root_name);
        }
        let root_self_closing = self
            .root_self_closing
            .map(|slash| slash - self.root_name.len() + root_name.len());

        // All patches have been validated before mutating the document. A
        // conversion failure therefore leaves the complete source untouched.
        self.nodes = nodes;
        self.origin = target;
        self.root_open = root_open;
        self.root_close = root_close;
        self.root_self_closing = root_self_closing;
        self.root_name = root_name.to_owned();
        Ok(report)
    }

    /// Changes character data only. Attributes, comments and processing
    /// instructions in this EditElement survive. Nested elements are refused.
    /// A CDATA node remains CDATA; `]]>` is split into valid adjacent sections.
    pub fn replace_text(&mut self, id: RichNodeId, text: &str) -> Result<(), RichEditError> {
        validate_xml_chars(text)?;
        let index = self.index(id)?;
        let node = &self.nodes[index];
        if node.kind != RichNodeKind::Text {
            return Err(RichEditError::UnsupportedNode(id));
        }
        if !node.text_editable {
            return Err(RichEditError::UnsupportedTextMarkup(id));
        }
        if node.text == text {
            return Ok(());
        }
        let mut raw = node.raw.clone();
        if let Some(slash) = node.opening.as_ref().and_then(|opening| opening.slash) {
            raw.remove(slash);
            raw.push_str(&escape_text(text));
            raw.push_str("</EditElement>");
        } else if let Some(first) = node.text_parts.first() {
            // Replacing semantic text must not replace comments/PI or metadata.
            // Adjacent CDATA/entity runs may contain several character tokens.
            for (i, part) in node.text_parts.iter().enumerate().rev() {
                let replacement = if i == 0 {
                    encode_text(text, first.cdata)
                } else {
                    String::new()
                };
                raw.replace_range(part.range.clone(), &replacement);
            }
        } else {
            let start = node.body.as_ref().expect("paired text body").start;
            raw.insert_str(start, &escape_text(text));
        }
        self.nodes[index] = parse_node(&raw, id, self.origin)?;
        Ok(())
    }

    /// Patches only filepath's value; every other attribute and the body remain.
    /// This is a reference edit, never an operation on the referenced file.
    pub fn set_path(&mut self, id: RichNodeId, path: &str) -> Result<(), RichEditError> {
        validate_path(path)?;
        let index = self.index(id)?;
        let node = &self.nodes[index];
        if !node.can_edit_path() {
            return Err(RichEditError::UnsupportedNode(id));
        }
        if node.path == path {
            return Ok(());
        }
        let opening = node.opening.as_ref().expect("image/file element");
        let mut raw = node.raw.clone();
        if let Some(attribute) = opening
            .attrs
            .iter()
            .find(|attribute| attribute.name == "filepath")
        {
            raw.replace_range(
                attribute.value.clone(),
                &escape_attribute(path, attribute.quote),
            );
        } else {
            let offset = opening.slash.unwrap_or(opening.range.end - 1);
            raw.insert_str(
                offset,
                &format!(" filepath=\"{}\"", escape_attribute(path, b'"')),
            );
        }
        self.nodes[index] = parse_node(&raw, id, self.origin)?;
        Ok(())
    }

    /// Removes the selected known node only. Opaque payloads are source-only.
    pub fn remove(&mut self, id: RichNodeId) -> Result<(), RichEditError> {
        let index = self.editable_index(id)?;
        self.nodes.remove(index);
        Ok(())
    }

    /// Moves one complete node, preserving its raw XML and stable ID. `None`
    /// means the end of the root; metadata and unknown nodes are never rebuilt.
    pub fn move_before(
        &mut self,
        id: RichNodeId,
        before: Option<RichNodeId>,
    ) -> Result<(), RichEditError> {
        let index = self.editable_index(id)?;
        if before == Some(id) {
            return Ok(());
        }
        let target = match before {
            Some(before) => self.index(before)?,
            None => self.nodes.len(),
        };
        let node = self.nodes.remove(index);
        self.nodes
            .insert(if index < target { target - 1 } else { target }, node);
        Ok(())
    }

    pub fn insert_text(
        &mut self,
        before: Option<RichNodeId>,
        text: &str,
    ) -> Result<RichNodeId, RichEditError> {
        validate_xml_chars(text)?;
        self.insert(
            before,
            format!(
                "<EditElement type=\"0\">{}</EditElement>",
                encode_text(text, true)
            ),
        )
    }

    pub fn insert_image(
        &mut self,
        before: Option<RichNodeId>,
        path: &str,
    ) -> Result<RichNodeId, RichEditError> {
        self.insert_path(
            before,
            path,
            if self.origin.uses_wechat_codes() {
                1
            } else {
                3
            },
        )
    }

    pub fn insert_file(
        &mut self,
        before: Option<RichNodeId>,
        path: &str,
    ) -> Result<RichNodeId, RichEditError> {
        self.insert_path(
            before,
            path,
            if self.origin.uses_wechat_codes() {
                3
            } else {
                4
            },
        )
    }

    /// Display projection only. Do not write this in place of qqRichText.
    /// Unknown nodes remain explicit in the projection rather than disappearing.
    pub fn plain_summary(&self) -> String {
        let mut text = String::new();
        for node in self.content_nodes() {
            match node.kind {
                RichNodeKind::Text | RichNodeKind::Placeholder => text.push_str(&node.text),
                RichNodeKind::Image => text.push_str("[图片]"),
                RichNodeKind::File => text.push_str("[文件]"),
                RichNodeKind::Opaque => text.push_str("[未知内容]"),
            }
        }
        text
    }

    fn index(&self, id: RichNodeId) -> Result<usize, RichEditError> {
        self.nodes
            .iter()
            .position(|node| node.id == id)
            .ok_or(RichEditError::UnknownNode(id))
    }

    fn editable_index(&self, id: RichNodeId) -> Result<usize, RichEditError> {
        let index = self.index(id)?;
        if self.nodes[index].kind == RichNodeKind::Opaque {
            return Err(RichEditError::UnsupportedNode(id));
        }
        Ok(index)
    }

    fn insert_path(
        &mut self,
        before: Option<RichNodeId>,
        path: &str,
        code: u8,
    ) -> Result<RichNodeId, RichEditError> {
        validate_path(path)?;
        self.insert(
            before,
            format!(
                "<EditElement type=\"{code}\" filepath=\"{}\" />",
                escape_attribute(path, b'"')
            ),
        )
    }

    fn insert(
        &mut self,
        before: Option<RichNodeId>,
        raw: String,
    ) -> Result<RichNodeId, RichEditError> {
        let index = match before {
            Some(id) => self.index(id)?,
            None => self.nodes.len(),
        };
        let id = RichNodeId(self.next_id);
        let node = parse_node(&raw, id, self.origin)?;
        if let Some(slash) = self.root_self_closing.take() {
            self.root_open.remove(slash);
            self.root_close = format!("</{}>", self.root_name);
        }
        self.nodes.insert(index, node);
        self.next_id += 1;
        Ok(id)
    }
}

#[derive(Clone, Debug)]
struct Attribute {
    name: String,
    value: Range<usize>,
    quote: u8,
}
#[derive(Clone, Debug)]
struct Opening {
    name: String,
    range: Range<usize>,
    attrs: Vec<Attribute>,
    slash: Option<usize>,
}
#[derive(Clone, Debug)]
struct TextPart {
    range: Range<usize>,
    cdata: bool,
}
struct Token {
    range: Range<usize>,
    kind: TokenKind,
}
enum TokenKind {
    Start(Opening),
    End(String),
    Text,
    Cdata,
    Comment,
    Pi,
    Declaration,
}
struct ParsedElement {
    children: Vec<Range<usize>>,
    close: Range<usize>,
    end: usize,
}

fn xml_error(offset: usize, reason: &'static str) -> RichEditError {
    RichEditError::MalformedXml { offset, reason }
}

/// Lexical spans are sufficient here; unlike a DOM serializer this retains all
/// bytes. CDATA/comments/quotes prevent fake tags from becoming editable nodes.
fn next_token(raw: &str, cursor: &mut usize) -> Result<Option<Token>, RichEditError> {
    if *cursor == raw.len() {
        return Ok(None);
    }
    let start = *cursor;
    let rest = &raw[start..];
    let (end, kind) = if !rest.starts_with('<') {
        (
            start + rest.find('<').unwrap_or(rest.len()),
            TokenKind::Text,
        )
    } else if rest.starts_with("<![CDATA[") {
        (
            start
                + 9
                + rest[9..]
                    .find("]]>")
                    .ok_or_else(|| xml_error(start, "unterminated CDATA"))?
                + 3,
            TokenKind::Cdata,
        )
    } else if rest.starts_with("<!--") {
        (
            start
                + 4
                + rest[4..]
                    .find("-->")
                    .ok_or_else(|| xml_error(start, "unterminated comment"))?
                + 3,
            TokenKind::Comment,
        )
    } else if rest.starts_with("<?") {
        (
            start
                + 2
                + rest[2..]
                    .find("?>")
                    .ok_or_else(|| xml_error(start, "unterminated processing instruction"))?
                + 2,
            TokenKind::Pi,
        )
    } else if rest.starts_with("<!") {
        (declaration_end(raw, start)?, TokenKind::Declaration)
    } else if rest.starts_with("</") {
        let end = start
            + rest
                .find('>')
                .ok_or_else(|| xml_error(start, "unterminated closing tag"))?
            + 1;
        let name = raw[start + 2..end - 1].trim();
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || b"/<>='\"".contains(&byte))
        {
            return Err(xml_error(start, "invalid closing tag"));
        }
        (end, TokenKind::End(name.to_owned()))
    } else {
        let opening = parse_opening(raw, start)?;
        (opening.range.end, TokenKind::Start(opening))
    };
    *cursor = end;
    Ok(Some(Token {
        range: start..end,
        kind,
    }))
}

fn declaration_end(raw: &str, start: usize) -> Result<usize, RichEditError> {
    let mut quote = None;
    let mut depth: usize = 0;
    for (index, &byte) in raw.as_bytes().iter().enumerate().skip(start + 2) {
        if let Some(held) = quote {
            if byte == held {
                quote = None;
            }
        } else {
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'[' => depth += 1,
                b']' => depth = depth.saturating_sub(1),
                b'>' if depth == 0 => return Ok(index + 1),
                _ => {}
            }
        }
    }
    Err(xml_error(start, "unterminated declaration"))
}

fn parse_opening(raw: &str, start: usize) -> Result<Opening, RichEditError> {
    let bytes = raw.as_bytes();
    let mut index = start + 1;
    let name_start = index;
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !b"/>=".contains(&bytes[index])
    {
        index += 1;
    }
    if index == name_start
        || raw[name_start..index]
            .bytes()
            .any(|byte| b"<'\"".contains(&byte))
    {
        return Err(xml_error(start, "invalid opening tag"));
    }
    let name = raw[name_start..index].to_owned();
    let mut attrs = Vec::<Attribute>::new();
    let mut slash = None;
    loop {
        let previous = index;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        match bytes.get(index).copied() {
            Some(b'>') => {
                return Ok(Opening {
                    name,
                    range: start..index + 1,
                    attrs,
                    slash,
                })
            }
            Some(b'/') if bytes.get(index + 1) == Some(&b'>') => {
                slash = Some(index);
                index += 1;
            }
            Some(_) => {
                if previous == index {
                    return Err(xml_error(index, "attribute requires whitespace"));
                }
                let attr_start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && !b"=/>'\"<".contains(&bytes[index])
                {
                    index += 1;
                }
                if index == attr_start {
                    return Err(xml_error(index, "invalid attribute name"));
                }
                let attr_name = raw[attr_start..index].to_owned();
                if attrs.iter().any(|attr| attr.name == attr_name) {
                    return Err(xml_error(index, "duplicate attribute"));
                }
                while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                if bytes.get(index) != Some(&b'=') {
                    return Err(xml_error(index, "attribute requires equals"));
                }
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                let quote = bytes
                    .get(index)
                    .copied()
                    .filter(|byte| matches!(byte, b'\'' | b'"'))
                    .ok_or_else(|| xml_error(index, "attribute requires quotes"))?;
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'<' {
                        return Err(xml_error(index, "unescaped angle in attribute"));
                    }
                    index += 1;
                }
                if index == bytes.len() {
                    return Err(xml_error(value_start, "unterminated attribute"));
                }
                attrs.push(Attribute {
                    name: attr_name,
                    value: value_start..index,
                    quote,
                });
                index += 1;
            }
            None => return Err(xml_error(start, "unterminated opening tag")),
        }
    }
}

fn parse_element(raw: &str, opening: Opening) -> Result<ParsedElement, RichEditError> {
    if opening.slash.is_some() {
        return Ok(ParsedElement {
            children: Vec::new(),
            close: opening.range.end..opening.range.end,
            end: opening.range.end,
        });
    }
    let mut cursor = opening.range.end;
    let mut stack = vec![opening.name];
    let mut children = Vec::new();
    let mut child_start = None;
    while let Some(token) = next_token(raw, &mut cursor)? {
        let top_level = stack.len() == 1;
        match token.kind {
            TokenKind::Start(child) => {
                if top_level {
                    child_start = Some(token.range.start);
                }
                if child.slash.is_none() {
                    stack.push(child.name);
                } else if top_level {
                    children.push(token.range);
                    child_start = None;
                }
            }
            TokenKind::End(name) => {
                if stack.last() != Some(&name) {
                    return Err(xml_error(token.range.start, "mismatched closing tag"));
                }
                stack.pop();
                if stack.is_empty() {
                    return Ok(ParsedElement {
                        children,
                        close: token.range,
                        end: cursor,
                    });
                }
                if stack.len() == 1 {
                    children.push(child_start.take().expect("direct element start")..cursor);
                }
            }
            _ if top_level => children.push(token.range),
            _ => {}
        }
    }
    Err(xml_error(cursor, "unclosed element"))
}

fn parse_node(raw: &str, id: RichNodeId, origin: RichOrigin) -> Result<RichNode, RichEditError> {
    let mut node = RichNode {
        id,
        kind: RichNodeKind::Opaque,
        raw: raw.to_owned(),
        text: String::new(),
        path: String::new(),
        metadata: false,
        opening: None,
        body: None,
        text_parts: Vec::new(),
        text_editable: false,
    };
    let mut cursor = 0;
    let Some(token) = next_token(raw, &mut cursor)? else {
        return Ok(node);
    };
    let TokenKind::Start(opening) = token.kind else {
        node.metadata = matches!(
            token.kind,
            TokenKind::Comment | TokenKind::Pi | TokenKind::Declaration
        ) || raw.trim().is_empty();
        return Ok(node);
    };
    if opening.name != "EditElement" {
        node.metadata = opening.name == "Info";
        return Ok(node);
    }
    let type_code = opening
        .attrs
        .iter()
        .find(|attr| attr.name == "type")
        .and_then(|attr| decode_xml(&raw[attr.value.clone()]))
        .and_then(|value| value.trim().parse::<u8>().ok());
    node.kind = match type_code {
        Some(0) => RichNodeKind::Text,
        Some(1) => RichNodeKind::Image,
        Some(2) | Some(6) if !origin.uses_wechat_codes() => RichNodeKind::Placeholder,
        Some(2) => RichNodeKind::Placeholder,
        Some(3) if origin.uses_wechat_codes() => RichNodeKind::File,
        Some(3) => RichNodeKind::Image,
        Some(4) if !origin.uses_wechat_codes() => RichNodeKind::File,
        _ => RichNodeKind::Opaque,
    };
    if matches!(node.kind, RichNodeKind::Image | RichNodeKind::File) {
        if let Some(attr) = opening.attrs.iter().find(|attr| attr.name == "filepath") {
            match decode_xml(&raw[attr.value.clone()]) {
                Some(path) => node.path = path,
                None => node.kind = RichNodeKind::Opaque,
            }
        }
    } else if node.kind == RichNodeKind::Placeholder {
        node.text = if type_code == Some(6) {
            opening
                .attrs
                .iter()
                .find(|attr| attr.name == "fileid")
                .and_then(|attr| decode_xml(&raw[attr.value.clone()]))
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "[emoji]".to_owned())
        } else {
            "[emoji]".to_owned()
        };
    } else if node.kind == RichNodeKind::Text {
        node.text_editable = true;
        if opening.slash.is_none() {
            let parsed = parse_element(raw, opening.clone())?;
            node.body = Some(opening.range.end..parsed.close.start);
            cursor = opening.range.end;
            while cursor < parsed.close.start {
                let token = next_token(raw, &mut cursor)?.expect("text body token");
                match token.kind {
                    TokenKind::Text => {
                        if let Some(text) = decode_xml(&raw[token.range.clone()]) {
                            node.text.push_str(&text);
                        } else {
                            node.text_editable = false;
                            node.text.push_str(&raw[token.range.clone()]);
                        }
                        node.text_parts.push(TextPart {
                            range: token.range,
                            cdata: false,
                        });
                    }
                    TokenKind::Cdata => {
                        node.text
                            .push_str(&raw[token.range.start + 9..token.range.end - 3]);
                        node.text_parts.push(TextPart {
                            range: token.range,
                            cdata: true,
                        });
                    }
                    TokenKind::Comment | TokenKind::Pi => {}
                    _ => node.text_editable = false,
                }
            }
        }
    }
    node.opening = Some(opening);
    Ok(node)
}

// Some WeChat clipboard versions wrap their own type codes in a QQ envelope.
// Match core's process-free fallback, without mistaking tags inside CDATA or
// comments for metadata. The explicit origin supplied by the DTO takes priority.
fn infer_qq_origin(raw: &str, children: &[Range<usize>]) -> Result<RichOrigin, RichEditError> {
    let mut has_wechat_image = false;
    let mut has_qq_image = false;
    for range in children {
        let child = &raw[range.clone()];
        let mut cursor = 0;
        let Some(Token {
            kind: TokenKind::Start(opening),
            ..
        }) = next_token(child, &mut cursor)?
        else {
            continue;
        };
        if opening.name == "Info" {
            return Ok(RichOrigin::Qq);
        }
        if opening.name != "EditElement" {
            continue;
        }
        let has_path = opening
            .attrs
            .iter()
            .find(|attr| attr.name == "filepath")
            .is_some_and(|attr| !child[attr.value.clone()].is_empty());
        if !has_path {
            continue;
        }
        match opening
            .attrs
            .iter()
            .find(|attr| attr.name == "type")
            .and_then(|attr| decode_xml(&child[attr.value.clone()]))
            .as_deref()
            .map(str::trim)
        {
            Some("1") => has_wechat_image = true,
            Some("3") => has_qq_image = true,
            _ => {}
        }
    }
    Ok(if has_wechat_image && !has_qq_image {
        RichOrigin::Wechat
    } else {
        RichOrigin::Qq
    })
}

fn validate_xml_chars(text: &str) -> Result<(), RichEditError> {
    if text.chars().all(|ch| matches!(ch as u32, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)) { Ok(()) }
    else { Err(RichEditError::InvalidXmlCharacter) }
}

fn validate_path(path: &str) -> Result<(), RichEditError> {
    validate_xml_chars(path)?;
    if path.trim().is_empty() {
        Err(RichEditError::EmptyPath)
    } else {
        Ok(())
    }
}

fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attribute(text: &str, quote: u8) -> String {
    let text = escape_text(text)
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#9;");
    if quote == b'\'' {
        text.replace('\'', "&apos;")
    } else {
        text.replace('"', "&quot;")
    }
}

fn encode_text(text: &str, cdata: bool) -> String {
    if cdata {
        format!("<![CDATA[{}]]>", text.replace("]]>", "]]]]><![CDATA[>"))
    } else {
        escape_text(text)
    }
}

/// No DTD/external entity resolution. An unknown entity makes its text/path
/// source-only; editing it as decoded text could otherwise silently corrupt it.
fn decode_xml(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let end = rest.find(';')?;
        let entity = &rest[..end];
        let ch = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = if let Some(number) = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                {
                    u32::from_str_radix(number, 16).ok()?
                } else {
                    entity.strip_prefix('#')?.parse::<u32>().ok()?
                };
                char::from_u32(number)?
            }
        };
        validate_xml_chars(&ch.to_string()).ok()?;
        out.push(ch);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_same_encoding_family_keeps_complete_raw_and_legacy_wrappers() {
        for (raw, source, target) in [
            ("prefix<?xml version='1.0'?><QQRichEditFormat vendor='keep'><Info version='1001'/><EditElement type='1' filepath='old.png' private='a'><![CDATA[vendor body]]></EditElement><EditElement type='6' fileid='face'/><Vendor><Blob/></Vendor></QQRichEditFormat>suffix", RichOrigin::Qq, RichOrigin::Qq),
            ("prefix<WeChatRichEditFormat vendor='keep'><Info version='1001'/><EditElement type='1' filepath='old.png'/><EditElement type='3' filepath='file.txt'/><EditElement type='2' vendor='emoji'/></WeChatRichEditFormat>suffix", RichOrigin::Wechat, RichOrigin::Wework),
            ("prefix<QQRichEditFormat vendor='legacy'><Info vendor='keep'/><EditElement type='1' filepath='image.png'/><EditElement type='3' filepath='file.txt'/><EditElement type='99'><Blob/></EditElement></QQRichEditFormat>suffix", RichOrigin::Wework, RichOrigin::Wechat),
        ] {
            let mut doc = ImRichDocument::parse(raw, source).unwrap();
            let ids: Vec<_> = doc.nodes().iter().map(RichNode::id).collect();
            assert_eq!(doc.rewrite_origin(target).unwrap(), RichRewriteReport::default());
            assert_eq!(doc.serialize(), raw);
            assert_eq!(doc.nodes().iter().map(RichNode::id).collect::<Vec<_>>(), ids);
            assert!(!doc.is_changed());
        }
    }

    #[test]
    fn native_cross_family_changes_only_root_and_recognized_type_values() {
        let raw = "prefix<?xml version='1.0'?>\r\n<QQRichEditFormat vendor='root &amp; value'>\n<Info version='1001' opaque='keep'/><!-- <EditElement type='3'/> -->\n<EditElement type='0' style='keep'><![CDATA[text </QQRichEditFormat> <EditElement type='3'/>]]><!--text-comment--><?vendor keep?></EditElement><EditElement type = '&#51;' vendor='image' filepath = 'old &amp; 图.png'><![CDATA[image-body]]></EditElement><EditElement type = '&#49;' filepath='emoji-image.png' keep='same code'/><EditElement type='4' filepath='file.txt' vendor='file'><Nested/></EditElement><EditElement type='2' face='keep'/><EditElement type='6' fileid='face-9'/><EditElement type='99' vendor='unknown'><![CDATA[opaque <EditElement type='3'/>]]></EditElement><Vendor><EditElement type='3' filepath='nested-is-opaque.png'/></Vendor>\n</QQRichEditFormat>suffix";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Qq).unwrap();
        let before: Vec<_> = doc.nodes().iter().map(RichNode::id).collect();
        let image = doc
            .nodes()
            .iter()
            .find(|node| node.path() == "old & 图.png")
            .unwrap()
            .id();
        let expected = raw
            .replacen(
                "<QQRichEditFormat vendor=",
                "<WeChatRichEditFormat vendor=",
                1,
            )
            .replacen(
                "</QQRichEditFormat>suffix",
                "</WeChatRichEditFormat>suffix",
                1,
            )
            .replacen("type = '&#51;'", "type = '1'", 1)
            .replacen(
                "type='4' filepath='file.txt'",
                "type='3' filepath='file.txt'",
                1,
            );
        assert_eq!(
            doc.rewrite_origin(RichOrigin::Wechat).unwrap(),
            RichRewriteReport {
                converted_nodes: 2,
                preserved_unmapped_nodes: 4,
            }
        );
        assert_eq!(doc.serialize(), expected);
        assert_eq!(
            doc.nodes().iter().map(RichNode::id).collect::<Vec<_>>(),
            before
        );
        assert_eq!(
            doc.rewrite_origin(RichOrigin::Wework).unwrap(),
            RichRewriteReport::default()
        );
        assert_eq!(doc.serialize(), expected);
        doc.set_path(image, "new & ' 图.png").unwrap();
        assert_eq!(
            doc.serialize(),
            expected.replacen(
                "filepath = 'old &amp; 图.png'",
                "filepath = 'new &amp; &apos; 图.png'",
                1
            )
        );
    }

    #[test]
    fn native_wechat_to_qq_keeps_file_image_metadata_and_unmapped_objects() {
        let raw = "prefix<WeChatRichEditFormat vendor='keep'><Info private='keep'/><EditElement type='0'><![CDATA[text]]></EditElement><EditElement type = '&#49;' filepath='image.png' vendor='keep'><Blob/></EditElement><EditElement type='3' filepath='file.txt'/><EditElement type='2' vendor='emoji'/><EditElement type='4' vendor='unknown WeChat type'><Blob/></EditElement></WeChatRichEditFormat>suffix";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Wework).unwrap();
        let expected = raw
            .replacen(
                "<WeChatRichEditFormat vendor=",
                "<QQRichEditFormat vendor=",
                1,
            )
            .replacen(
                "</WeChatRichEditFormat>suffix",
                "</QQRichEditFormat>suffix",
                1,
            )
            .replacen("type = '&#49;'", "type = '3'", 1)
            .replacen(
                "type='3' filepath='file.txt'",
                "type='4' filepath='file.txt'",
                1,
            );
        assert_eq!(
            doc.rewrite_origin(RichOrigin::Qq).unwrap(),
            RichRewriteReport {
                converted_nodes: 2,
                preserved_unmapped_nodes: 2,
            }
        );
        assert_eq!(doc.serialize(), expected);
        assert!(doc
            .nodes()
            .iter()
            .any(|node| node.kind() == RichNodeKind::Opaque
                && node.raw_xml().contains("unknown WeChat type")));
        let file = doc
            .nodes()
            .iter()
            .find(|node| node.path() == "file.txt")
            .unwrap()
            .id();
        doc.set_path(file, "new-file.txt").unwrap();
        assert_eq!(
            doc.serialize(),
            expected.replacen("filepath='file.txt'", "filepath='new-file.txt'", 1)
        );
    }

    #[test]
    fn native_cross_family_self_closing_root_retains_editor_insert_spans() {
        let mut doc = ImRichDocument::parse(
            "prefix<QQRichEditFormat vendor='keep' />suffix",
            RichOrigin::Qq,
        )
        .unwrap();
        doc.rewrite_origin(RichOrigin::Wechat).unwrap();
        assert_eq!(
            doc.serialize(),
            "prefix<WeChatRichEditFormat vendor='keep' />suffix"
        );
        let image = doc.insert_image(None, "new.png").unwrap();
        assert_eq!(doc.node(image).unwrap().kind(), RichNodeKind::Image);
        assert_eq!(doc.serialize(), "prefix<WeChatRichEditFormat vendor='keep' ><EditElement type=\"1\" filepath=\"new.png\" /></WeChatRichEditFormat>suffix");
        doc.rewrite_origin(RichOrigin::Unknown).unwrap();
        assert!(doc
            .serialize()
            .contains("<QQRichEditFormat vendor='keep' >"));
        assert!(doc.serialize().contains("type=\"3\" filepath=\"new.png\""));
    }

    #[test]
    fn original_envelope_span_and_encoded_type_ignore_fake_metadata_and_close_tags() {
        let prefix = "prefix<!--<QQRichEditFormat><Info/></QQRichEditFormat>-->";
        let envelope = r#"<QQRichEditFormat vendor='keep'><EditElement type='0'><![CDATA[</QQRichEditFormat><Info/><EditElement type='3' filepath='fake.png'/>]]></EditElement><EditElement type='&#49;' filepath='old &amp; 图.png' vendor="untouched"/></QQRichEditFormat>"#;
        let raw = format!("{prefix}{envelope}suffix");
        let mut document = ImRichDocument::parse(&raw, RichOrigin::Unknown).unwrap();
        assert_eq!(&raw[document.envelope_range()], envelope);
        assert_eq!(document.origin(), RichOrigin::Wechat);
        assert_eq!(document.serialize(), raw);
        let image = document
            .nodes()
            .iter()
            .find(|node| node.kind() == RichNodeKind::Image)
            .unwrap();
        assert_eq!(image.path(), "old & 图.png");
        let image_id = image.id();
        document.set_path(image_id, "new & ' 图.png").unwrap();
        assert_eq!(
            document.serialize(),
            raw.replacen(
                "filepath='old &amp; 图.png'",
                "filepath='new &amp; &apos; 图.png'",
                1
            )
        );
    }

    fn ids(doc: &ImRichDocument, kind: RichNodeKind) -> Vec<RichNodeId> {
        doc.nodes()
            .iter()
            .filter(|node| node.kind() == kind)
            .map(RichNode::id)
            .collect()
    }

    #[test]
    fn untouched_roundtrip_keeps_every_byte_and_unknown_payload() {
        let raw = "header\0<?xml version='1.0'?>\r\n<QQRichEditFormat private='v > x'>\n<Info version=\"1001\" />\n<!-- <EditElement type='0'>fake</EditElement> -->\n<EditElement type='0' color='blue'><![CDATA[  中😀\r\n]]></EditElement><EditElement type='99' payload='a &amp; b'><Nested/></EditElement><EditElement type='3' filepath='C:\\a&amp;b.png' extra='yes'/></QQRichEditFormat>suffix\0";
        let doc = ImRichDocument::parse(raw, RichOrigin::Qq).unwrap();
        assert_eq!(doc.serialize(), raw);
        assert!(!doc.is_changed());
        assert_eq!(ids(&doc, RichNodeKind::Text).len(), 1);
        assert_eq!(
            doc.node(ids(&doc, RichNodeKind::Image)[0]).unwrap().path(),
            "C:\\a&b.png"
        );
        assert_eq!(doc.content_nodes().count(), 3);
    }

    #[test]
    fn editing_text_preserves_attributes_info_comments_unknown_nodes_and_other_text() {
        let raw = "<QQRichEditFormat><Info version='1001'/><EditElement type='0' color='blue'><![CDATA[old]]><!--keep--><?x keep?></EditElement><EditElement type='6' fileid='face' private='keep'/><EditElement type='99'><Blob/></EditElement><EditElement type='0'>other &amp; text</EditElement></QQRichEditFormat>";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Qq).unwrap();
        let id = ids(&doc, RichNodeKind::Text)[0];
        doc.replace_text(id, "新😀 ]]> <&").unwrap();
        assert_eq!(
            doc.serialize(),
            raw.replace("<![CDATA[old]]>", "<![CDATA[新😀 ]]]]><![CDATA[> <&]]>")
        );
        assert_eq!(doc.node(id).unwrap().text(), "新😀 ]]> <&");
        let xml = doc.serialize();
        doc.replace_text(id, "新😀 ]]> <&").unwrap();
        assert_eq!(doc.serialize(), xml);
        assert_eq!(
            doc.node(ids(&doc, RichNodeKind::Placeholder)[0])
                .unwrap()
                .text(),
            "face"
        );
    }

    #[test]
    fn path_change_is_an_attribute_patch_including_quotes_angles_and_entities() {
        let raw = "<QQRichEditFormat><EditElement type = '3' private='>' filepath = 'C:\\old.png'><Blob encoding='unknown'/></EditElement><EditElement type='4' filepath=\"D:\\old.txt\" /></QQRichEditFormat>";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Qq).unwrap();
        let id = ids(&doc, RichNodeKind::Image)[0];
        doc.set_path(id, "D:\\a'&<>\".png").unwrap();
        assert_eq!(
            doc.serialize(),
            raw.replace("C:\\old.png", "D:\\a&apos;&amp;&lt;&gt;\".png")
        );
        assert_eq!(doc.node(id).unwrap().path(), "D:\\a'&<>\".png");
        let file = ids(&doc, RichNodeKind::File)[0];
        doc.set_path(file, "D:\\a\"&.txt").unwrap();
        assert!(doc
            .serialize()
            .contains("filepath=\"D:\\a&quot;&amp;.txt\""));
    }

    #[test]
    fn moves_delete_and_insert_keep_unknown_nodes_and_stable_ids() {
        let raw = "<QQRichEditFormat><Info version='v'/><EditElement type='0'>A</EditElement><Unknown><X/></Unknown><EditElement type='1' filepath='a.png' extra='x'/><EditElement type='0'>B</EditElement></QQRichEditFormat>";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Qq).unwrap();
        let texts = ids(&doc, RichNodeKind::Text);
        let image = ids(&doc, RichNodeKind::Image)[0];
        doc.move_before(image, Some(texts[0])).unwrap();
        assert_eq!(doc.content_nodes().next().unwrap().id(), image);
        assert_eq!(
            doc.node(image).unwrap().raw_xml(),
            "<EditElement type='1' filepath='a.png' extra='x'/>"
        );
        doc.remove(texts[1]).unwrap();
        let inserted = doc.insert_file(Some(texts[0]), "C:\\a&b.txt").unwrap();
        assert_eq!(doc.node(inserted).unwrap().kind(), RichNodeKind::File);
        assert!(doc.serialize().contains("<Unknown><X/></Unknown>"));
        assert!(doc.serialize().contains("<Info version='v'/>"));
        let unknown = ids(&doc, RichNodeKind::Opaque)
            .into_iter()
            .find(|id| !doc.node(*id).unwrap().is_metadata())
            .unwrap();
        let before = doc.serialize();
        assert!(doc.remove(unknown).is_err());
        assert!(doc.move_before(image, Some(RichNodeId(999))).is_err());
        assert_eq!(doc.serialize(), before);
    }

    #[test]
    fn wechat_and_wework_codes_and_selfclosing_envelope_are_kept() {
        let raw = "prefix<WeChatRichEditFormat version='x' />suffix";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Wework).unwrap();
        assert_eq!(doc.serialize(), raw);
        assert_eq!(doc.origin(), RichOrigin::Wework);
        let text = doc.insert_text(None, "甲").unwrap();
        let image = doc.insert_image(None, "a.png").unwrap();
        let file = doc.insert_file(None, "b.txt").unwrap();
        assert_eq!(doc.node(text).unwrap().kind(), RichNodeKind::Text);
        assert!(doc.node(image).unwrap().raw_xml().contains("type=\"1\""));
        assert!(doc.node(file).unwrap().raw_xml().contains("type=\"3\""));
        assert_eq!(doc.plain_summary(), "甲[图片][文件]");
        assert!(doc
            .serialize()
            .starts_with("prefix<WeChatRichEditFormat version='x' >"));
        assert!(doc.serialize().ends_with("</WeChatRichEditFormat>suffix"));
        assert!(ImRichDocument::parse(&doc.serialize(), RichOrigin::Wework).is_ok());
    }

    #[test]
    fn nested_xml_and_unknown_entities_cannot_be_flattened_by_text_edit() {
        for body in ["a<Private flag='keep'>b</Private>c", "&private;"] {
            let raw = format!(
                "<QQRichEditFormat><EditElement type='0'>{body}</EditElement></QQRichEditFormat>"
            );
            let mut doc = ImRichDocument::parse(&raw, RichOrigin::Qq).unwrap();
            let id = ids(&doc, RichNodeKind::Text)[0];
            assert!(!doc.node(id).unwrap().can_edit_text());
            assert_eq!(
                doc.replace_text(id, "replacement"),
                Err(RichEditError::UnsupportedTextMarkup(id))
            );
            assert_eq!(doc.serialize(), raw);
        }
    }

    #[test]
    fn empty_text_and_absent_path_can_be_edited_without_losing_attributes() {
        let raw = "<QQRichEditFormat><EditElement type='0' style='keep' /><EditElement type='0'><!--keep--></EditElement><EditElement type='3' flag='keep'/></QQRichEditFormat>";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Qq).unwrap();
        let text = ids(&doc, RichNodeKind::Text);
        doc.replace_text(text[0], "a<&").unwrap();
        doc.replace_text(text[1], "b").unwrap();
        doc.set_path(ids(&doc, RichNodeKind::Image)[0], "new.png")
            .unwrap();
        assert!(doc
            .serialize()
            .contains("<EditElement type='0' style='keep' >a&lt;&amp;</EditElement>"));
        assert!(doc
            .serialize()
            .contains("<EditElement type='0'>b<!--keep--></EditElement>"));
        assert!(doc
            .serialize()
            .contains("<EditElement type='3' flag='keep' filepath=\"new.png\"/>"));
    }

    #[test]
    fn malformed_or_fake_envelopes_are_not_edited_and_failures_do_not_mutate() {
        assert!(ImRichDocument::parse("<!--<QQRichEditFormat/>-->", RichOrigin::Qq).is_err());
        for raw in ["<QQRichEditFormat><EditElement></QQRichEditFormat>", "<QQRichEditFormat><EditElement type='0' type='3'/></QQRichEditFormat>", "<QQRichEditFormat><EditElement type='0'><![CDATA[never closed</EditElement></QQRichEditFormat>"] {
            assert!(ImRichDocument::parse(raw, RichOrigin::Qq).is_err());
        }
        let mut doc = ImRichDocument::parse(
            "<QQRichEditFormat><EditElement type='0'>x</EditElement></QQRichEditFormat>",
            RichOrigin::Qq,
        )
        .unwrap();
        let before = doc.serialize();
        let id = ids(&doc, RichNodeKind::Text)[0];
        assert_eq!(
            doc.replace_text(id, "x\0y"),
            Err(RichEditError::InvalidXmlCharacter)
        );
        assert_eq!(doc.insert_image(None, "  "), Err(RichEditError::EmptyPath));
        assert_eq!(doc.serialize(), before);
    }

    #[test]
    fn process_free_origin_fallback_accepts_wechat_codes_in_qq_envelope() {
        let raw = "<QQRichEditFormat><!--<Info version='fake'/>--><EditElement type='1' filepath='a.png'/></QQRichEditFormat>";
        let mut doc = ImRichDocument::parse(raw, RichOrigin::Unknown).unwrap();
        assert_eq!(doc.origin(), RichOrigin::Wechat);
        let file = doc.insert_file(None, "a.txt").unwrap();
        assert!(doc.node(file).unwrap().raw_xml().contains("type=\"3\""));
        // QQ NT type 1 is also an image; Info or the explicit DTO origin wins.
        assert_eq!(
            ImRichDocument::parse(
                &raw.replace("<!--<Info version='fake'/>-->", "<Info version='1001'/>"),
                RichOrigin::Unknown
            )
            .unwrap()
            .origin(),
            RichOrigin::Qq
        );
        assert_eq!(
            ImRichDocument::parse(raw, RichOrigin::Qq).unwrap().origin(),
            RichOrigin::Qq
        );
    }
}
