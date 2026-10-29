//! A bounded, inert HTML projection for native overlays, FIFO and copy cards.
//!
//! This is display data, never an editor/paste serialization. The caller must
//! retain the original HTML and SourceURL. HTML5 tree construction handles
//! entities, malformed markup and table foster parenting; only inline styles
//! and a small set of semantic tags are projected. It is not a browser CSS
//! layout engine (stylesheets, positioning, floats and CSS images are ignored).
//!
//! Images remain in document order. No paths are read and no URLs are fetched.
//! Resolve `LocalPath` / `RelativePath` through the host's existing image access
//! policy, not directly through `gpui::img` with untrusted HTML input. Remote,
//! active and unsupported image sources are explicit unavailable placeholders.

use html5ever::{
    parse_document,
    tendril::{StrTendril, TendrilSink},
    tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink},
    Attribute, ExpandedName, QualName,
};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HtmlPreview {
    pub blocks: Vec<HtmlBlock>,
    /// The display projection hit a limit; the original payload is untouched.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HtmlBlock {
    Paragraph(HtmlParagraph),
    Preformatted(HtmlParagraph),
    Table(HtmlTable),
    Rule,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HtmlParagraph {
    pub inlines: Vec<HtmlInline>,
    /// Zero for an outer list item, one for a nested list item, etc.
    pub indent: usize,
    pub marker: Option<HtmlListMarker>,
    pub alignment: HtmlAlignment,
    pub preserve_whitespace: bool,
    pub quote_depth: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HtmlInline {
    Text(HtmlRun),
    Image(HtmlImage),
    LineBreak,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlRun {
    pub text: String,
    pub style: HtmlTextStyle,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HtmlListMarker {
    Unordered,
    Ordered(i64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HtmlAlignment {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtmlColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlTextStyle {
    pub font_weight: u16,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub font_size_px: Option<f32>,
    pub color: Option<HtmlColor>,
    pub background: Option<HtmlColor>,
    pub font_family: Option<String>,
    pub monospace: bool,
}

impl Default for HtmlTextStyle {
    fn default() -> Self {
        Self {
            font_weight: 400,
            italic: false,
            underline: false,
            strikethrough: false,
            font_size_px: None,
            color: None,
            background: None,
            font_family: None,
            monospace: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlImage {
    pub source: HtmlImageSource,
    pub alt: String,
    pub width_px: Option<f32>,
    pub height_px: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HtmlImageSource {
    LocalPath(String),
    /// Relative references require an explicit host-authorized base, if any.
    RelativePath(String),
    EmbeddedData {
        mime: String,
        uri: String,
    },
    Unavailable {
        original: String,
        reason: HtmlImageUnavailable,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HtmlImageUnavailable {
    Missing,
    Remote,
    Unsupported,
    Invalid,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HtmlTable {
    pub rows: Vec<Vec<HtmlCell>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlCell {
    pub blocks: Vec<HtmlBlock>,
    pub header: bool,
    pub column_span: usize,
    pub row_span: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct HtmlPreviewLimits {
    pub max_input_bytes: usize,
    pub max_nodes: usize,
    pub max_depth: usize,
    pub max_text_chars: usize,
    pub max_blocks: usize,
    pub max_images: usize,
}

impl Default for HtmlPreviewLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 1_048_576,
            max_nodes: 12_000,
            max_depth: 96,
            max_text_chars: 16_000,
            max_blocks: 512,
            max_images: 64,
        }
    }
}

#[derive(Default)]
struct Node {
    name: Option<QualName>,
    attrs: RefCell<Vec<Attribute>>,
    text: RefCell<String>,
    is_text: bool,
    children: RefCell<Vec<Rc<Node>>>,
    parent: RefCell<Weak<Node>>,
    template: RefCell<Option<Rc<Node>>>,
    depth: Cell<usize>,
}

// The registry keeps all nodes alive while links are torn down. Deep hostile
// markup cannot recursively drop an Rc tree and overflow the UI thread stack.
struct Dom {
    document: Rc<Node>,
    nodes: RefCell<Vec<Rc<Node>>>,
    max_depth: Cell<usize>,
}

impl Default for Dom {
    fn default() -> Self {
        let document = Rc::new(Node::default());
        Self {
            document: document.clone(),
            nodes: RefCell::new(vec![document]),
            max_depth: Cell::new(0),
        }
    }
}

impl Drop for Dom {
    fn drop(&mut self) {
        for node in self.nodes.get_mut().iter() {
            node.children.borrow_mut().clear();
            node.template.borrow_mut().take();
        }
    }
}

impl Dom {
    fn register(&self, node: Node) -> Rc<Node> {
        let node = Rc::new(node);
        self.nodes.borrow_mut().push(node.clone());
        node
    }

    fn detach(node: &Rc<Node>) {
        if let Some(parent) = node.parent.borrow().upgrade() {
            parent
                .children
                .borrow_mut()
                .retain(|child| !Rc::ptr_eq(child, node));
        }
        *node.parent.borrow_mut() = Weak::new();
    }

    fn attach(&self, parent: &Rc<Node>, node: NodeOrText<Rc<Node>>, before: Option<&Rc<Node>>) {
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
                if let Some(previous) = index
                    .checked_sub(1)
                    .and_then(|i| children.get(i))
                    .filter(|node| node.is_text)
                {
                    previous.text.borrow_mut().push_str(&text);
                    return;
                }
                self.register(Node {
                    text: RefCell::new(text.to_string()),
                    is_text: true,
                    ..Default::default()
                })
            }
        };
        child.depth.set(parent.depth.get().saturating_add(1));
        self.max_depth
            .set(self.max_depth.get().max(child.depth.get()));
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
    type Output = Self;
    type ElemName<'a> = ExpandedName<'a>;
    fn finish(self) -> Self::Output {
        self
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
        let template = flags.template.then(|| self.register(Node::default()));
        self.register(Node {
            name: Some(name),
            attrs: RefCell::new(attrs),
            template: RefCell::new(template),
            ..Default::default()
        })
    }
    fn create_comment(&self, _: StrTendril) -> Self::Handle {
        self.register(Node::default())
    }
    fn create_pi(&self, _: StrTendril, _: StrTendril) -> Self::Handle {
        self.register(Node::default())
    }
    fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
        self.attach(parent, child, None);
    }
    fn append_before_sibling(&self, sibling: &Self::Handle, node: NodeOrText<Self::Handle>) {
        if let Some(parent) = sibling.parent.borrow().upgrade() {
            self.attach(&parent, node, Some(sibling));
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
            self.attach(parent, NodeOrText::AppendNode(child), None);
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

fn attr(node: &Node, key: &str) -> Option<String> {
    node.attrs
        .borrow()
        .iter()
        .find(|attr| attr.name.local.as_ref() == key)
        .map(|attr| attr.value.to_string())
}

fn tag(node: &Node) -> &str {
    node.name.as_ref().map_or("", |name| name.local.as_ref())
}

#[derive(Clone, Copy, Default)]
enum Whitespace {
    #[default]
    Normal,
    Pre,
    PreLine,
}

#[derive(Clone, Default)]
struct Style {
    text: HtmlTextStyle,
    alignment: HtmlAlignment,
    whitespace: Whitespace,
    hidden: bool,
    width: Option<f32>,
    height: Option<f32>,
}

fn positive_length(value: &str) -> Option<f32> {
    let value = value.trim().strip_suffix("px").unwrap_or(value.trim());
    let number: f32 = value.parse().ok()?;
    (number.is_finite() && number > 0.0).then_some(number.min(8192.0))
}

fn font_size(value: &str, parent: f32) -> Option<f32> {
    let value = value.trim();
    let (number, scale) = if let Some(v) = value.strip_suffix("rem") {
        (v, 14.0)
    } else if let Some(v) = value.strip_suffix("em") {
        (v, parent)
    } else if let Some(v) = value.strip_suffix('%') {
        (v, parent / 100.0)
    } else if let Some(v) = value.strip_suffix("pt") {
        (v, 96.0 / 72.0)
    } else {
        (value.strip_suffix("px").unwrap_or(value), 1.0)
    };
    let number = match value {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 12.0,
        "medium" => 14.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "smaller" => parent * 0.8,
        "larger" => parent * 1.2,
        _ => number.parse::<f32>().ok()? * scale,
    };
    (number.is_finite() && number > 0.0).then_some(number.clamp(6.0, 96.0))
}

fn hex_byte(value: &str) -> Option<u8> {
    u8::from_str_radix(value, 16).ok()
}

pub fn parse_html_color(value: &str) -> Option<HtmlColor> {
    let value = value.trim().to_ascii_lowercase();
    let color = |red, green, blue, alpha| HtmlColor {
        red,
        green,
        blue,
        alpha,
    };
    if let Some(hex) = value.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            3 | 4 => Some(color(
                hex_byte(&hex[0..1])? * 17,
                hex_byte(&hex[1..2])? * 17,
                hex_byte(&hex[2..3])? * 17,
                if hex.len() == 4 {
                    hex_byte(&hex[3..4])? * 17
                } else {
                    255
                },
            )),
            6 | 8 => Some(color(
                hex_byte(&hex[0..2])?,
                hex_byte(&hex[2..4])?,
                hex_byte(&hex[4..6])?,
                if hex.len() == 8 {
                    hex_byte(&hex[6..8])?
                } else {
                    255
                },
            )),
            _ => None,
        };
    }
    if let Some(body) = value
        .strip_prefix("rgb(")
        .or_else(|| value.strip_prefix("rgba("))
        .and_then(|v| v.strip_suffix(')'))
    {
        let parts: Vec<_> = body
            .split([',', '/', ' '])
            .filter(|part| !part.is_empty())
            .collect();
        if !(3..=4).contains(&parts.len()) {
            return None;
        }
        let channel = |value: &str| -> Option<u8> {
            let number = if let Some(value) = value.strip_suffix('%') {
                value.parse::<f32>().ok()? * 2.55
            } else {
                value.parse::<f32>().ok()?
            };
            number
                .is_finite()
                .then_some(number.clamp(0.0, 255.0).round() as u8)
        };
        let alpha = if let Some(alpha) = parts.get(3) {
            let number = if let Some(value) = alpha.strip_suffix('%') {
                value.parse::<f32>().ok()? * 2.55
            } else {
                alpha.parse::<f32>().ok()? * 255.0
            };
            if !number.is_finite() {
                return None;
            }
            number.clamp(0.0, 255.0).round() as u8
        } else {
            255
        };
        return Some(color(
            channel(parts[0])?,
            channel(parts[1])?,
            channel(parts[2])?,
            alpha,
        ));
    }
    let (r, g, b) = match value.as_str() {
        "transparent" => return Some(color(0, 0, 0, 0)),
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "purple" => (128, 0, 128),
        "fuchsia" | "magenta" => (255, 0, 255),
        "lime" => (0, 255, 0),
        "olive" => (128, 128, 0),
        "navy" => (0, 0, 128),
        "teal" => (0, 128, 128),
        "aqua" | "cyan" => (0, 255, 255),
        "orange" => (255, 165, 0),
        "rebeccapurple" => (102, 51, 153),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "lightgray" | "lightgrey" => (211, 211, 211),
        _ => return None,
    };
    Some(color(r, g, b, 255))
}

fn alignment(value: &str) -> Option<HtmlAlignment> {
    match value.trim() {
        "left" | "start" => Some(HtmlAlignment::Left),
        "center" => Some(HtmlAlignment::Center),
        "right" | "end" => Some(HtmlAlignment::Right),
        "justify" => Some(HtmlAlignment::Justify),
        _ => None,
    }
}

fn styled(node: &Node, parent: &Style) -> Style {
    let mut style = parent.clone();
    // Image box size does not inherit through the DOM.
    style.width = None;
    style.height = None;
    let tag = tag(node);
    match tag {
        "b" | "strong" | "th" => style.text.font_weight = 700,
        "i" | "em" | "cite" | "dfn" | "var" => style.text.italic = true,
        "u" | "ins" => style.text.underline = true,
        "s" | "strike" | "del" => style.text.strikethrough = true,
        "pre" => {
            style.whitespace = Whitespace::Pre;
            style.text.monospace = true;
        }
        "code" | "kbd" | "samp" | "tt" => style.text.monospace = true,
        "small" => {
            style.text.font_size_px = font_size("smaller", parent.text.font_size_px.unwrap_or(14.0))
        }
        "big" => {
            style.text.font_size_px = font_size("larger", parent.text.font_size_px.unwrap_or(14.0))
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            style.text.font_weight = 700;
            let scale = match tag {
                "h1" => 2.0,
                "h2" => 1.5,
                "h3" => 1.17,
                "h5" => 0.83,
                "h6" => 0.67,
                _ => 1.0,
            };
            style.text.font_size_px =
                Some((parent.text.font_size_px.unwrap_or(14.0) * scale).clamp(6.0, 96.0));
        }
        "mark" => style.text.background = parse_html_color("yellow"),
        "center" => style.alignment = HtmlAlignment::Center,
        _ => {}
    }
    if attr(node, "hidden").is_some() {
        style.hidden = true;
    }
    if let Some(value) =
        attr(node, "align").and_then(|value| alignment(&value.to_ascii_lowercase()))
    {
        style.alignment = value;
    }
    if tag == "font" {
        if let Some(color) = attr(node, "color").and_then(|value| parse_html_color(&value)) {
            style.text.color = Some(color);
        }
        if let Some(face) = attr(node, "face") {
            style.text.font_family = safe_font_family(&face);
        }
        if let Some(raw_size) = attr(node, "size") {
            if let Ok(size) = raw_size.trim().parse::<i32>() {
                // Legacy HTML font sizes (including relative +1/-1) use 3 as the base.
                let size = if raw_size.trim().starts_with(['+', '-']) {
                    3_i32.saturating_add(size)
                } else {
                    size
                };
                style.text.font_size_px =
                    Some([10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0][size.clamp(1, 7) as usize - 1]);
            }
        }
    }
    for declaration in attr(node, "style").unwrap_or_default().split(';') {
        let Some((key, raw_value)) = declaration.split_once(':') else {
            continue;
        };
        let value = raw_value.trim();
        let value = value.strip_suffix("!important").unwrap_or(value).trim();
        let lower = value.to_ascii_lowercase();
        match key.trim().to_ascii_lowercase().as_str() {
            "display" if lower == "none" => style.hidden = true,
            "visibility" if matches!(lower.as_str(), "hidden" | "collapse") => style.hidden = true,
            "font-weight" => {
                style.text.font_weight = match lower.as_str() {
                    "normal" => 400,
                    "bold" | "bolder" => 700,
                    "lighter" => 300,
                    _ => lower
                        .parse::<u16>()
                        .ok()
                        .filter(|weight| (1..=1000).contains(weight))
                        .unwrap_or(style.text.font_weight),
                };
            }
            "font-style" => match lower.as_str() {
                "italic" | "oblique" => style.text.italic = true,
                "normal" => style.text.italic = false,
                _ => {}
            },
            "text-decoration" | "text-decoration-line" => {
                // Parent text decorations still paint descendant glyphs in a browser.
                if lower.split_whitespace().any(|part| part == "underline") {
                    style.text.underline = true;
                }
                if lower.split_whitespace().any(|part| part == "line-through") {
                    style.text.strikethrough = true;
                }
            }
            "font-size" => {
                if let Some(size) = font_size(&lower, parent.text.font_size_px.unwrap_or(14.0)) {
                    style.text.font_size_px = Some(size);
                }
            }
            "font-family" => style.text.font_family = safe_font_family(value),
            "color" => {
                if let Some(color) = parse_html_color(&lower) {
                    style.text.color = Some(color);
                }
            }
            "background-color" | "background" => {
                if let Some(color) = parse_html_color(&lower) {
                    style.text.background = Some(color);
                }
            }
            "text-align" => {
                if let Some(align) = alignment(&lower) {
                    style.alignment = align;
                }
            }
            "white-space" => match lower.as_str() {
                "pre" | "pre-wrap" | "break-spaces" => style.whitespace = Whitespace::Pre,
                "pre-line" => style.whitespace = Whitespace::PreLine,
                "normal" | "nowrap" => style.whitespace = Whitespace::Normal,
                _ => {}
            },
            "width" => style.width = positive_length(&lower),
            "height" => style.height = positive_length(&lower),
            _ => {}
        }
    }
    style
}

fn safe_font_family(value: &str) -> Option<String> {
    let first = value.split(',').next()?.trim().trim_matches(['\'', '"']);
    (!first.is_empty() && first.len() <= 128 && !first.chars().any(char::is_control))
        .then(|| first.to_string())
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            result.push(hex_byte(hex)?);
            index += 3;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(result)
        .ok()
        .filter(|value| !value.chars().any(char::is_control))
}

/// Classifies without accessing a file, resolving against SourceURL or fetching.
pub fn classify_html_image_source(src: &str) -> HtmlImageSource {
    let src = src.trim();
    let unavailable = |reason| HtmlImageSource::Unavailable {
        original: src.to_string(),
        reason,
    };
    if src.is_empty() {
        return unavailable(HtmlImageUnavailable::Missing);
    }
    if src.chars().any(char::is_control) {
        return unavailable(HtmlImageUnavailable::Invalid);
    }
    let lower = src.to_ascii_lowercase();
    if lower.starts_with("data:") {
        let Some((metadata, _)) = src[5..].split_once(',') else {
            return unavailable(HtmlImageUnavailable::Invalid);
        };
        let metadata = metadata.to_ascii_lowercase();
        let mime = metadata.split(';').next().unwrap_or_default();
        if supported_image_mime(mime) {
            return HtmlImageSource::EmbeddedData {
                mime: mime.to_string(),
                uri: src.to_string(),
            };
        }
        return unavailable(HtmlImageUnavailable::Unsupported);
    }
    if lower.starts_with("file:") {
        let raw = &src[5..];
        let raw = if let Some(authority_and_path) = raw.strip_prefix("//") {
            let Some((authority, path)) = authority_and_path.split_once('/') else {
                return unavailable(HtmlImageUnavailable::Invalid);
            };
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return unavailable(HtmlImageUnavailable::Remote);
            }
            format!("/{path}")
        } else {
            raw.to_string()
        };
        let raw = raw.split(['?', '#']).next().unwrap_or_default();
        let Some(mut path) = percent_decode(raw) else {
            return unavailable(HtmlImageUnavailable::Invalid);
        };
        if path.starts_with('/')
            && path.as_bytes().get(2) == Some(&b':')
            && path.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic)
        {
            path.remove(0);
        }
        if path.replace('\\', "/").starts_with("//") {
            return unavailable(HtmlImageUnavailable::Remote);
        }
        if is_absolute_local(&path) {
            return HtmlImageSource::LocalPath(path);
        }
        return unavailable(HtmlImageUnavailable::Invalid);
    }
    if src.replace('\\', "/").starts_with("//")
        || lower.starts_with("http:")
        || lower.starts_with("https:")
    {
        return unavailable(HtmlImageUnavailable::Remote);
    }
    if is_absolute_local(src) {
        return HtmlImageSource::LocalPath(src.to_string());
    }
    if src.contains(':') {
        return unavailable(HtmlImageUnavailable::Unsupported);
    }
    if src.starts_with('#') || src.contains(['?', '#']) {
        return unavailable(HtmlImageUnavailable::Invalid);
    }
    HtmlImageSource::RelativePath(src.to_string())
}

fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp" | "image/tiff"
    )
}

/// Decodes only a supported embedded raster data URI, without image parsing or
/// I/O. The caller still needs its existing image decoder/dimension checks.
/// Invalid encodings and encoded sources over 8 MiB return `None`.
pub fn decode_embedded_image(source: &HtmlImageSource) -> Option<(Vec<u8>, String)> {
    let HtmlImageSource::EmbeddedData { mime, uri } = source else {
        return None;
    };
    if uri.len() > 8 * 1024 * 1024 || !uri.get(..5)?.eq_ignore_ascii_case("data:") {
        return None;
    }
    let (metadata, payload) = uri[5..].split_once(',')?;
    let metadata = metadata.to_ascii_lowercase();
    let actual_mime = metadata.split(';').next()?;
    if !supported_image_mime(actual_mime) || !actual_mime.eq_ignore_ascii_case(mime) {
        return None;
    }
    let decoded_payload = percent_decode_bytes(payload)?;
    let data = if metadata.split(';').any(|part| part == "base64") {
        decode_base64(&decoded_payload)?
    } else {
        decoded_payload
    };
    (!data.is_empty()).then(|| (data, actual_mime.to_string()))
}

fn percent_decode_bytes(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            result.push(hex_byte(hex)?);
            index += 3;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    Some(result)
}

fn decode_base64(source: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(source.len() / 4 * 3);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    let mut sextets = 0_usize;
    let mut padding = 0_usize;
    for &byte in source {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding += 1;
            if padding > 2 {
                return None;
            }
            continue;
        }
        if padding > 0 {
            return None;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        sextets += 1;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    // Two/three final sextets may omit padding; one can never encode a byte.
    if sextets % 4 == 1 || accumulator != 0 {
        return None;
    }
    if (padding == 1 && sextets % 4 != 3) || (padding == 2 && sextets % 4 != 2) {
        return None;
    }
    Some(output)
}

fn is_absolute_local(path: &str) -> bool {
    path.starts_with('/')
        || (path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path.as_bytes().get(1) == Some(&b':')
            && matches!(path.as_bytes().get(2), Some(b'/' | b'\\')))
}

#[derive(Clone, Copy, Default)]
struct FlowContext {
    indent: usize,
    list_depth: usize,
    quote_depth: usize,
}

struct Budget {
    limits: HtmlPreviewLimits,
    nodes: usize,
    chars: usize,
    blocks: usize,
    images: usize,
    truncated: bool,
}

impl Budget {
    fn visit(&mut self, depth: usize) -> bool {
        if depth > self.limits.max_depth || self.nodes >= self.limits.max_nodes {
            self.truncated = true;
            false
        } else {
            self.nodes += 1;
            true
        }
    }
    fn take_text(&mut self, text: &str) -> String {
        let remaining = self.limits.max_text_chars.saturating_sub(self.chars);
        let count = text.chars().count();
        if count > remaining {
            self.truncated = true;
        }
        let text: String = text.chars().take(remaining).collect();
        self.chars += count.min(remaining);
        text
    }
    fn block(&mut self) -> bool {
        if self.blocks >= self.limits.max_blocks {
            self.truncated = true;
            false
        } else {
            self.blocks += 1;
            true
        }
    }
}

#[derive(Default)]
struct Flow {
    blocks: Vec<HtmlBlock>,
    current: Option<HtmlParagraph>,
    pending_space: bool,
}

impl Flow {
    fn paragraph(&mut self, style: &Style, context: FlowContext) -> &mut HtmlParagraph {
        self.current.get_or_insert_with(|| HtmlParagraph {
            indent: context.indent,
            alignment: style.alignment,
            preserve_whitespace: matches!(style.whitespace, Whitespace::Pre),
            quote_depth: context.quote_depth,
            ..Default::default()
        })
    }
    fn run(&mut self, text: String, style: &Style, context: FlowContext, budget: &mut Budget) {
        let text = budget.take_text(&text);
        if text.is_empty() {
            return;
        }
        let paragraph = self.paragraph(style, context);
        if let Some(HtmlInline::Text(previous)) = paragraph
            .inlines
            .last_mut()
            .filter(|last| matches!(last, HtmlInline::Text(run) if run.style == style.text))
        {
            previous.text.push_str(&text);
        } else {
            paragraph.inlines.push(HtmlInline::Text(HtmlRun {
                text,
                style: style.text.clone(),
            }));
        }
    }
    fn text(&mut self, text: &str, style: &Style, context: FlowContext, budget: &mut Budget) {
        if matches!(style.whitespace, Whitespace::Pre) {
            self.space(style, context, budget);
            self.run(text.to_string(), style, context, budget);
            return;
        }
        let mut output = String::new();
        for ch in text.chars() {
            if ch == '\n' && matches!(style.whitespace, Whitespace::PreLine) {
                if !output.is_empty() {
                    self.run(std::mem::take(&mut output), style, context, budget);
                }
                self.line_break(style, context);
            } else if matches!(ch, ' ' | '\t' | '\r' | '\n' | '\u{c}') {
                self.pending_space = !output.is_empty()
                    || self
                        .current
                        .as_ref()
                        .and_then(|p| p.inlines.last())
                        .is_some_and(|inline| match inline {
                            HtmlInline::Text(run) => {
                                !run.text.is_empty() && !run.text.ends_with('\n')
                            }
                            HtmlInline::Image(_) => true,
                            HtmlInline::LineBreak => false,
                        });
            } else {
                if self.pending_space {
                    output.push(' ');
                    self.pending_space = false;
                }
                output.push(ch);
            }
        }
        self.run(output, style, context, budget);
    }
    fn space(&mut self, style: &Style, context: FlowContext, budget: &mut Budget) {
        if self.pending_space {
            self.pending_space = false;
            self.run(" ".into(), style, context, budget);
        }
    }
    fn line_break(&mut self, style: &Style, context: FlowContext) {
        self.pending_space = false;
        self.paragraph(style, context)
            .inlines
            .push(HtmlInline::LineBreak);
    }
    fn flush(&mut self, budget: &mut Budget, pre: bool) {
        self.pending_space = false;
        if let Some(paragraph) = self.current.take().filter(|p| !p.inlines.is_empty()) {
            if budget.block() {
                self.blocks.push(if pre || paragraph.preserve_whitespace {
                    HtmlBlock::Preformatted(paragraph)
                } else {
                    HtmlBlock::Paragraph(paragraph)
                });
            }
        }
    }
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
            | "p"
            | "pre"
            | "section"
            | "summary"
    )
}

fn skipped(tag: &str) -> bool {
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
            | "embed"
            | "canvas"
            | "video"
            | "audio"
            | "source"
            | "link"
            | "meta"
            | "base"
            | "title"
    )
}

fn children(
    node: &Rc<Node>,
    style: &Style,
    context: FlowContext,
    flow: &mut Flow,
    budget: &mut Budget,
    depth: usize,
) {
    for child in node.children.borrow().iter() {
        walk(child, style, context, flow, budget, depth + 1);
    }
}

fn walk(
    node: &Rc<Node>,
    parent: &Style,
    context: FlowContext,
    flow: &mut Flow,
    budget: &mut Budget,
    depth: usize,
) {
    if !budget.visit(depth) {
        return;
    }
    if node.is_text {
        flow.text(&node.text.borrow(), parent, context, budget);
        return;
    }
    let tag = tag(node);
    if skipped(tag) {
        return;
    }
    let style = styled(node, parent);
    if style.hidden {
        return;
    }
    match tag {
        "br" => {
            flow.line_break(&style, context);
            return;
        }
        "wbr" => return,
        "hr" => {
            flow.flush(budget, false);
            if budget.block() {
                flow.blocks.push(HtmlBlock::Rule);
            }
            return;
        }
        "img" => {
            if budget.images >= budget.limits.max_images {
                budget.truncated = true;
                return;
            }
            budget.images += 1;
            flow.space(&style, context, budget);
            let image = HtmlImage {
                source: classify_html_image_source(&attr(node, "src").unwrap_or_default()),
                alt: budget.take_text(&attr(node, "alt").unwrap_or_default()),
                width_px: style
                    .width
                    .or_else(|| attr(node, "width").and_then(|value| positive_length(&value))),
                height_px: style
                    .height
                    .or_else(|| attr(node, "height").and_then(|value| positive_length(&value))),
            };
            flow.paragraph(&style, context)
                .inlines
                .push(HtmlInline::Image(image));
            return;
        }
        "table" => {
            flow.flush(budget, false);
            if !budget.block() {
                return;
            }
            let mut table = HtmlTable::default();
            table_rows(node, &style, context, &mut table, budget, depth);
            if !table.rows.is_empty() {
                flow.blocks.push(HtmlBlock::Table(table));
            }
            return;
        }
        "ul" | "ol" => {
            flow.flush(budget, false);
            list(node, &style, context, flow, budget, depth);
            return;
        }
        _ => {}
    }
    if is_block(tag) {
        flow.flush(budget, false);
        let mut nested = Flow::default();
        let mut nested_context = context;
        if tag == "blockquote" {
            nested_context.quote_depth += 1;
        }
        children(node, &style, nested_context, &mut nested, budget, depth);
        nested.flush(budget, tag == "pre");
        // Preserve an explicit empty paragraph as a blank display line.
        if nested.blocks.is_empty() && matches!(tag, "p" | "pre") && budget.block() {
            let paragraph = HtmlParagraph {
                indent: context.indent,
                alignment: style.alignment,
                preserve_whitespace: tag == "pre",
                quote_depth: context.quote_depth,
                ..Default::default()
            };
            nested.blocks.push(if tag == "pre" {
                HtmlBlock::Preformatted(paragraph)
            } else {
                HtmlBlock::Paragraph(paragraph)
            });
        }
        flow.blocks.extend(nested.blocks);
    } else {
        children(node, &style, context, flow, budget, depth);
    }
}

fn list(
    node: &Rc<Node>,
    style: &Style,
    context: FlowContext,
    flow: &mut Flow,
    budget: &mut Budget,
    depth: usize,
) {
    let ordered = tag(node) == "ol";
    let reversed = attr(node, "reversed").is_some();
    let mut counter = attr(node, "start")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_else(|| {
            if reversed {
                node.children
                    .borrow()
                    .iter()
                    .filter(|node| tag(node) == "li")
                    .count() as i64
            } else {
                1
            }
        });
    for child in node.children.borrow().iter() {
        if tag(child) != "li" {
            walk(child, style, context, flow, budget, depth + 1);
            continue;
        }
        flow.flush(budget, false);
        if !budget.visit(depth + 1) {
            continue;
        }
        if let Some(value) = attr(child, "value").and_then(|value| value.parse::<i64>().ok()) {
            counter = value;
        }
        let marker = if ordered {
            HtmlListMarker::Ordered(counter)
        } else {
            HtmlListMarker::Unordered
        };
        counter = if reversed {
            counter.saturating_sub(1)
        } else {
            counter.saturating_add(1)
        };
        let child_style = styled(child, style);
        if child_style.hidden {
            continue;
        }
        let child_context = FlowContext {
            indent: context.list_depth,
            list_depth: context.list_depth + 1,
            ..context
        };
        let mut item = Flow::default();
        children(
            child,
            &child_style,
            child_context,
            &mut item,
            budget,
            depth + 1,
        );
        item.flush(budget, false);
        if let Some(HtmlBlock::Paragraph(paragraph) | HtmlBlock::Preformatted(paragraph)) =
            item.blocks.first_mut().filter(|block| match block {
                HtmlBlock::Paragraph(paragraph) | HtmlBlock::Preformatted(paragraph) => {
                    paragraph.marker.is_none() && paragraph.indent == child_context.indent
                }
                _ => false,
            })
        {
            paragraph.marker = Some(marker);
        } else if budget.block() {
            item.blocks.insert(
                0,
                HtmlBlock::Paragraph(HtmlParagraph {
                    marker: Some(marker),
                    indent: child_context.indent,
                    quote_depth: child_context.quote_depth,
                    alignment: child_style.alignment,
                    ..Default::default()
                }),
            );
        }
        flow.blocks.extend(item.blocks);
    }
    flow.flush(budget, false);
}

fn table_rows(
    node: &Rc<Node>,
    style: &Style,
    context: FlowContext,
    table: &mut HtmlTable,
    budget: &mut Budget,
    depth: usize,
) {
    for child in node.children.borrow().iter() {
        if !budget.visit(depth + 1) {
            continue;
        }
        let child_style = styled(child, style);
        if child_style.hidden {
            continue;
        }
        match tag(child) {
            "thead" | "tbody" | "tfoot" => {
                table_rows(child, &child_style, context, table, budget, depth + 1)
            }
            "tr" => {
                let mut row = Vec::new();
                for cell in child
                    .children
                    .borrow()
                    .iter()
                    .filter(|cell| matches!(tag(cell), "td" | "th"))
                {
                    if !budget.visit(depth + 2) {
                        continue;
                    }
                    let cell_style = styled(cell, &child_style);
                    if cell_style.hidden {
                        continue;
                    }
                    let mut content = Flow::default();
                    children(cell, &cell_style, context, &mut content, budget, depth + 2);
                    content.flush(budget, false);
                    let span = |key| {
                        attr(cell, key)
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(1)
                            .clamp(1, 1000)
                    };
                    row.push(HtmlCell {
                        blocks: content.blocks,
                        header: tag(cell) == "th",
                        column_span: span("colspan"),
                        row_span: span("rowspan"),
                    });
                }
                if !row.is_empty() {
                    table.rows.push(row);
                }
            }
            // Captions are projected as a single row before subsequent rows.
            "caption" => {
                let mut content = Flow::default();
                children(
                    child,
                    &child_style,
                    context,
                    &mut content,
                    budget,
                    depth + 1,
                );
                content.flush(budget, false);
                table.rows.push(vec![HtmlCell {
                    blocks: content.blocks,
                    header: false,
                    column_span: 1,
                    row_span: 1,
                }]);
            }
            _ => {}
        }
    }
}

pub fn parse_html_preview(html: &str) -> HtmlPreview {
    parse_html_preview_with_limits(html, &HtmlPreviewLimits::default())
}

pub fn parse_html_preview_with_limits(html: &str, limits: &HtmlPreviewLimits) -> HtmlPreview {
    // CF_HTML is the persisted/paste payload. Project only its selected content
    // in this inert view; neither protocol text nor unselected context is body.
    let html = if cliphost_core::cf_html::looks_like_complete_format(html) {
        cliphost_core::cf_html::fragment_range(html)
            .and_then(|(start, end)| html.get(start..end))
            .unwrap_or_else(|| html.find('<').map(|start| &html[start..]).unwrap_or(""))
    } else {
        html
    };
    if html.is_empty() {
        return HtmlPreview::default();
    }
    let mut end = html.len().min(limits.max_input_bytes);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let mut parser = parse_document(Dom::default(), Default::default());
    let mut offset = 0;
    let mut parser_truncated = end < html.len();
    // Stop feeding after the DOM node/depth budget is reached. A chunk is at
    // most 1 KiB, so the parser itself has a bounded overshoot before the exact
    // projection limits apply. This also bounds HTML5 scope work on deep input.
    while offset < end {
        let mut next = (offset + 1024).min(end);
        while !html.is_char_boundary(next) {
            next -= 1;
        }
        parser.process(StrTendril::from_slice(&html[offset..next]));
        offset = next;
        let sink = &parser.tokenizer.sink.sink;
        if sink.nodes.borrow().len() > limits.max_nodes || sink.max_depth.get() > limits.max_depth {
            parser_truncated |= offset < end;
            break;
        }
    }
    let dom = parser.finish();
    let mut budget = Budget {
        limits: *limits,
        nodes: 0,
        chars: 0,
        blocks: 0,
        images: 0,
        truncated: parser_truncated,
    };
    let mut flow = Flow::default();
    walk(
        &dom.document,
        &Style::default(),
        FlowContext::default(),
        &mut flow,
        &mut budget,
        0,
    );
    flow.flush(&mut budget, false);
    HtmlPreview {
        blocks: flow.blocks,
        truncated: budget.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_clipboard_envelope_keeps_table_preview_without_protocol_text() {
        let document = "<html xmlns:x='urn:schemas-microsoft-com:office:excel'><head><style>.cell{color:red}</style></head><body><!--StartFragment--><table><tr><td class='cell'>中文😀</td></tr></table><!--EndFragment--></body></html>";
        let raw = cliphost_core::cf_html::build_clipboard_html(
            document,
            Some("https://example.invalid/wps"),
        );
        let expected = parse_html_preview(document);
        assert!(matches!(expected.blocks.first(), Some(HtmlBlock::Table(_))));
        assert_eq!(parse_html_preview(&raw), expected);
        assert_eq!(parse_html_preview(&format!("\u{feff} \r\n{raw}")), expected);
    }

    #[test]
    fn clipboard_preview_excludes_unselected_context_and_supports_text_only_fragments() {
        let fragment = "<table><tr><td>selected😀</td></tr></table>";
        let document = format!("<html><head><style>.cell{{color:red}}</style></head><body>before<!--StartFragment-->{fragment}<!--EndFragment-->after</body></html>");
        let raw = cliphost_core::cf_html::build_clipboard_html(&document, None);
        assert_eq!(parse_html_preview(&raw), parse_html_preview(fragment));
        for fragment in ["纯文本😀", "Hello<b>world😀</b>"] {
            let header = "Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
            let raw = header
                .replace(
                    "StartFragment:0000000000",
                    &format!("StartFragment:{:010}", header.len()),
                )
                .replace(
                    "EndFragment:0000000000",
                    &format!("EndFragment:{:010}", header.len() + fragment.len()),
                )
                + fragment;
            assert_eq!(parse_html_preview(&raw), parse_html_preview(fragment));
        }
    }

    fn paragraph(block: &HtmlBlock) -> &HtmlParagraph {
        match block {
            HtmlBlock::Paragraph(p) | HtmlBlock::Preformatted(p) => p,
            _ => panic!("not a paragraph"),
        }
    }
    fn text(p: &HtmlParagraph) -> String {
        p.inlines
            .iter()
            .map(|inline| match inline {
                HtmlInline::Text(run) => run.text.as_str(),
                HtmlInline::LineBreak => "\n",
                HtmlInline::Image(_) => "[image]",
            })
            .collect()
    }
    fn runs(p: &HtmlParagraph) -> Vec<&HtmlRun> {
        p.inlines
            .iter()
            .filter_map(|inline| {
                if let HtmlInline::Text(run) = inline {
                    Some(run)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn paragraphs_and_inline_styles_keep_spaces_entities_and_unicode() {
        let result = parse_html_preview(
            "<p>A <b>B &amp; 中😀</b> C <i>I</i><u>U</u><del>D</del></p><p>next<br>line</p>",
        );
        assert_eq!(result.blocks.len(), 2);
        let p = paragraph(&result.blocks[0]);
        assert_eq!(text(p), "A B & 中😀 C IUD");
        let runs = runs(p);
        assert_eq!(runs[1].style.font_weight, 700);
        assert!(runs[3].style.italic);
        assert!(runs[4].style.underline);
        assert!(runs[5].style.strikethrough);
        assert_eq!(text(paragraph(&result.blocks[1])), "next\nline");
        assert!(!result.truncated);
    }

    #[test]
    fn common_inline_css_inherits_and_allows_weight_and_italic_overrides() {
        let result = parse_html_preview("<div style='font-size:20px;color:#123;background-color:rgba(100,50,0,.5);font-family: &quot;Segoe UI&quot;, sans-serif;text-align:center'><b><span style='font-weight:normal;font-style:italic;font-size:150%;text-decoration:underline line-through'>styled</span></b></div>");
        let p = paragraph(&result.blocks[0]);
        let run = runs(p)[0];
        assert_eq!(p.alignment, HtmlAlignment::Center);
        assert_eq!(run.style.font_weight, 400);
        assert!(run.style.italic && run.style.underline && run.style.strikethrough);
        assert_eq!(run.style.font_size_px, Some(30.0));
        assert_eq!(run.style.font_family.as_deref(), Some("Segoe UI"));
        assert_eq!(
            run.style.color,
            Some(HtmlColor {
                red: 17,
                green: 34,
                blue: 51,
                alpha: 255
            })
        );
        assert_eq!(run.style.background.unwrap().alpha, 128);
    }

    #[test]
    fn preformatted_and_preline_text_preserve_explicit_lines() {
        let result = parse_html_preview("<pre>  x\n    &lt;y&gt;  </pre><div style='white-space:pre-line'>A   B\nC</div><p>A&nbsp;&nbsp;B</p>");
        assert!(matches!(result.blocks[0], HtmlBlock::Preformatted(_)));
        assert_eq!(text(paragraph(&result.blocks[0])), "  x\n    <y>  ");
        assert!(runs(paragraph(&result.blocks[0]))[0].style.monospace);
        assert_eq!(text(paragraph(&result.blocks[1])), "A B\nC");
        assert_eq!(text(paragraph(&result.blocks[2])), "A\u{a0}\u{a0}B");
        let result =
            parse_html_preview("<p>A<br>   B</p><div style='white-space:pre-line'>C\n  D</div>");
        assert_eq!(text(paragraph(&result.blocks[0])), "A\nB");
        assert_eq!(text(paragraph(&result.blocks[1])), "C\nD");
    }

    #[test]
    fn lists_have_one_marker_per_item_and_keep_nested_order() {
        let result = parse_html_preview("<ol start='4'><li><p>A</p><p>more</p><ul><li>inner</li></ul></li><li value='9'>B</li></ol><ol reversed><li>C</li><li>D</li></ol>");
        let p: Vec<_> = result.blocks.iter().map(paragraph).collect();
        assert_eq!(
            p.iter().map(|p| text(p)).collect::<Vec<_>>(),
            ["A", "more", "inner", "B", "C", "D"]
        );
        assert_eq!(p[0].marker, Some(HtmlListMarker::Ordered(4)));
        assert_eq!(p[1].marker, None);
        assert_eq!(p[2].marker, Some(HtmlListMarker::Unordered));
        assert_eq!(p[2].indent, 1);
        assert_eq!(p[3].marker, Some(HtmlListMarker::Ordered(9)));
        assert_eq!(p[4].marker, Some(HtmlListMarker::Ordered(2)));
        assert_eq!(p[5].marker, Some(HtmlListMarker::Ordered(1)));
        let result = parse_html_preview("<ul>prefix<li>A</li>suffix</ul>after");
        assert_eq!(
            result
                .blocks
                .iter()
                .map(|block| text(paragraph(block)))
                .collect::<Vec<_>>(),
            ["prefix", "A", "suffix", "after"]
        );
        let result = parse_html_preview("<ul><li><ul><li>nested</li></ul></li></ul>");
        assert_eq!(result.blocks.len(), 2);
        assert_eq!(paragraph(&result.blocks[0]).indent, 0);
        assert_eq!(paragraph(&result.blocks[1]).indent, 1);
    }

    #[test]
    fn html5_table_tree_has_cells_spans_and_nested_content() {
        let result = parse_html_preview("<table><tr><th colspan='2'>head</th></tr><tr><td>A<p>B</p></td><td rowspan='2'><img src='file:///C:/a.png'>C</td></tr></table>");
        let HtmlBlock::Table(table) = &result.blocks[0] else {
            panic!("table")
        };
        assert_eq!(table.rows.len(), 2);
        assert!(table.rows[0][0].header);
        assert_eq!(table.rows[0][0].column_span, 2);
        assert_eq!(
            runs(paragraph(&table.rows[0][0].blocks[0]))[0]
                .style
                .font_weight,
            700
        );
        assert_eq!(table.rows[1][0].blocks.len(), 2);
        assert_eq!(table.rows[1][1].row_span, 2);
        assert_eq!(text(paragraph(&table.rows[1][1].blocks[0])), "[image]C");
    }

    #[test]
    fn images_stay_between_the_original_text_runs_and_do_not_fetch() {
        let result = parse_html_preview("<p>A<img src='file:///C:/Image/a%20b.png' alt='local' style='width:24px;height:12px'>B<img src='https://example.invalid/p.png'>C<img src='data:image/png;base64,AAAA'>D</p>");
        let p = paragraph(&result.blocks[0]);
        assert_eq!(text(p), "A[image]B[image]C[image]D");
        let HtmlInline::Image(local) = &p.inlines[1] else {
            panic!("local")
        };
        assert_eq!(
            local.source,
            HtmlImageSource::LocalPath("C:/Image/a b.png".into())
        );
        assert_eq!(local.width_px, Some(24.0));
        assert_eq!(local.height_px, Some(12.0));
        assert_eq!(local.alt, "local");
        assert!(matches!(
            &p.inlines[3],
            HtmlInline::Image(HtmlImage {
                source: HtmlImageSource::Unavailable {
                    reason: HtmlImageUnavailable::Remote,
                    ..
                },
                ..
            })
        ));
        assert!(
            matches!(&p.inlines[5], HtmlInline::Image(HtmlImage { source: HtmlImageSource::EmbeddedData { mime, .. }, .. }) if mime == "image/png")
        );
    }

    #[test]
    fn unsafe_elements_hidden_content_and_event_handlers_are_inert() {
        let source = "<head><title>skip</title><style>body{color:red}</style></head><p onclick='x()'>safe<script>bad()</script><svg><text>skip</text></svg><iframe>skip</iframe><template>skip</template><span hidden>skip</span><b style='display:none'>skip</b><i style='visibility:hidden'>skip</i></p>";
        let original = source.to_string();
        let result = parse_html_preview(source);
        assert_eq!(text(paragraph(&result.blocks[0])), "safe");
        assert_eq!(source, original);
        assert!(matches!(
            classify_html_image_source("javascript:alert(1)"),
            HtmlImageSource::Unavailable {
                reason: HtmlImageUnavailable::Unsupported,
                ..
            }
        ));
        assert!(matches!(
            classify_html_image_source("data:image/svg+xml;base64,AAAA"),
            HtmlImageSource::Unavailable {
                reason: HtmlImageUnavailable::Unsupported,
                ..
            }
        ));
    }

    #[test]
    fn local_source_classification_never_turns_remote_or_invalid_urls_into_paths() {
        for src in [
            "file://server/share/a.png",
            "file:///%5C%5Cserver/a.png",
            "/\\server/a.png",
            "\\\\server\\a.png",
            "//server/a.png",
            "https://example/a.png",
        ] {
            assert!(
                matches!(
                    classify_html_image_source(src),
                    HtmlImageSource::Unavailable {
                        reason: HtmlImageUnavailable::Remote,
                        ..
                    }
                ),
                "{src}"
            );
        }
        assert_eq!(
            classify_html_image_source("file://localhost/C:/a.png"),
            HtmlImageSource::LocalPath("C:/a.png".into())
        );
        assert_eq!(
            classify_html_image_source("C:\\Image\\a.png"),
            HtmlImageSource::LocalPath("C:\\Image\\a.png".into())
        );
        assert_eq!(
            classify_html_image_source("Image/a.png"),
            HtmlImageSource::RelativePath("Image/a.png".into())
        );
        for src in [
            "file:///C:/a%00.png",
            "file:///C:/a%ZZ.png",
            "file:relative.png",
            "\n",
            "a.png?remote=1",
        ] {
            assert!(
                matches!(
                    classify_html_image_source(src),
                    HtmlImageSource::Unavailable { .. }
                ),
                "{src}"
            );
        }
    }

    #[test]
    fn projection_limits_keep_unicode_boundaries_and_report_truncation() {
        let limits = HtmlPreviewLimits {
            max_text_chars: 3,
            max_images: 1,
            ..Default::default()
        };
        let result = parse_html_preview_with_limits(
            "<p>中😀ab<img src='a.png'><img src='b.png'></p>",
            &limits,
        );
        assert_eq!(text(paragraph(&result.blocks[0])), "中😀a[image]");
        assert!(result.truncated);
        let result = parse_html_preview_with_limits(
            "<table><tr><td>A</td><td>B</td></tr></table>",
            &HtmlPreviewLimits {
                max_blocks: 2,
                ..Default::default()
            },
        );
        let HtmlBlock::Table(table) = &result.blocks[0] else {
            panic!("keep partial table")
        };
        assert_eq!(text(paragraph(&table.rows[0][0].blocks[0])), "A");
        assert!(table.rows[0][1].blocks.is_empty());
        assert!(result.truncated);
        let result = parse_html_preview_with_limits(
            "中😀abc",
            &HtmlPreviewLimits {
                max_input_bytes: 4,
                ..Default::default()
            },
        );
        assert_eq!(text(paragraph(&result.blocks[0])), "中");
        assert!(result.truncated);
        let result = parse_html_preview_with_limits(
            "<p>A</p><p>B</p>",
            &HtmlPreviewLimits {
                max_blocks: 1,
                ..Default::default()
            },
        );
        assert_eq!(result.blocks.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn deep_markup_is_bounded_and_drops_without_recursive_tree_destruction() {
        let source = format!("{}x{}", "<div>".repeat(2000), "</div>".repeat(2000));
        let result = parse_html_preview(&source);
        assert!(result.truncated);
        assert!(result.blocks.is_empty());
    }

    #[test]
    fn colors_validate_hex_rgb_percent_and_alpha() {
        assert_eq!(parse_html_color("#abc8").unwrap().alpha, 136);
        assert_eq!(
            parse_html_color("rgb(100% 0% 50% / 25%)").unwrap(),
            HtmlColor {
                red: 255,
                green: 0,
                blue: 128,
                alpha: 64
            }
        );
        assert_eq!(parse_html_color("transparent").unwrap().alpha, 0);
        for value in [
            "#中abc",
            "#12",
            "rgb(NaN,0,0)",
            "rgba(1,2)",
            "url(https://x)",
        ] {
            assert!(parse_html_color(value).is_none());
        }
    }

    #[test]
    fn embedded_raster_data_decodes_base64_or_percent_encoding_without_io() {
        let decode = |uri: &str| decode_embedded_image(&classify_html_image_source(uri));
        assert_eq!(
            decode("data:image/png;base64,aGVsbG8="),
            Some((b"hello".to_vec(), "image/png".into()))
        );
        assert_eq!(
            decode("data:image/PNG;base64,AAECA%2Fw%3D"),
            Some((vec![0, 1, 2, 3, 252], "image/png".into()))
        );
        assert_eq!(
            decode("data:image/png,%89PNG%00%FF"),
            Some((vec![137, 80, 78, 71, 0, 255], "image/png".into()))
        );
        assert_eq!(
            decode("data:image/png;base64,AQ"),
            Some((vec![1], "image/png".into()))
        );
        for uri in [
            "data:image/svg+xml;base64,AQ==",
            "data:image/png;base64,A",
            "data:image/png;base64,AQ===",
            "data:image/png;base64,AQ=A",
            "data:image/png;base64,AR==",
            "data:image/png,%ZZ",
            "data:image/png,",
        ] {
            assert!(decode(uri).is_none(), "{uri}");
        }
        assert!(decode_embedded_image(&HtmlImageSource::LocalPath("C:/a.png".into())).is_none());
    }
}
