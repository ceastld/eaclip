//! Bounded clipboard HTML conversion, never a serializer for an existing source
//! document. This uses HTML5 tokenization plus the inert HTML projection. Browser
//! computed styles, arbitrary stylesheets and Quill's visual margin matcher are
//! deliberately unavailable; only explicit supported semantics are imported.

use crate::editor_html::{HtmlBlock, HtmlBlockKind, HtmlCanvasKind, HtmlDocument, HtmlStyle};
use html5ever::{
    tendril::StrTendril,
    tokenizer::{
        states::RawKind, BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer,
    },
};
use std::{cell::RefCell, ops::Range};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct HtmlClipboardPlan {
    pub(super) text: String,
    pub(super) spans: Vec<HtmlClipboardSpan>,
    pub(super) lines: Vec<HtmlClipboardLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HtmlClipboardSpan {
    pub(super) range: Range<usize>,
    pub(super) style: HtmlStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HtmlClipboardLine {
    /// UTF-8 offset of a retained '\n', including ordinary unformatted rows.
    pub(super) newline: usize,
    pub(super) format: HtmlBlock,
}

impl HtmlClipboardPlan {
    pub(super) fn from_html(fragment: &str) -> Result<Self, String> {
        let tokenizer = Tokenizer::new(ClipboardTokens::default(), Default::default());
        let input = BufferQueue::default();
        input.push_back(StrTendril::from(fragment));
        let _ = tokenizer.feed(&input);
        tokenizer.end();
        let sanitized = tokenizer.sink.state.into_inner().output;
        let document = HtmlDocument::parse(sanitized);
        let projection = document.canvas_projection();
        let mut builder = PlanBuilder::default();
        for span in projection.spans {
            match span.kind {
                HtmlCanvasKind::Text { run } => {
                    let run = &document.runs()[run];
                    let block = clipboard_block(&document.blocks()[run.block_index]);
                    let mut style = run.style.clone();
                    style.code = false; // inline code is not in this editor's formats
                    if let Some(link) = &style.link {
                        style.link = Some(safe_link(link));
                    }
                    builder.text(&run.text, style, &block, block.code);
                }
                HtmlCanvasKind::Break { block, .. } => {
                    builder.newline(
                        &clipboard_block(&document.blocks()[block]),
                        !builder.boundary_just_emitted,
                    );
                    builder.boundary_just_emitted = false;
                }
                HtmlCanvasKind::Boundary { left, .. } => {
                    // Empty owners without a text/br child produce no Quill
                    // newline; a source-preserving canvas's virtual row differs.
                    if let Some(format) = left.index.map(|index| &document.blocks()[index]) {
                        let source = &document.source()[format.source_range.clone()];
                        let owner = source
                            .strip_prefix('<')
                            .unwrap_or("")
                            .split(|character: char| !character.is_ascii_alphanumeric())
                            .next()
                            .unwrap_or("");
                        if matches!(
                            owner,
                            "p" | "div"
                                | "li"
                                | "blockquote"
                                | "pre"
                                | "h1"
                                | "h2"
                                | "h3"
                                | "h4"
                                | "h5"
                                | "h6"
                        ) {
                            builder
                                .newline(&clipboard_block(format), !builder.boundary_just_emitted);
                            builder.boundary_just_emitted = true;
                        }
                    }
                }
                HtmlCanvasKind::Image { .. } => {} // objects never enter the plan
            }
        }
        builder.finish();
        Ok(builder.plan)
    }
}

fn clipboard_block(block: &HtmlBlock) -> HtmlBlock {
    let mut format = block.clone();
    format.elements.clear();
    format.source_range = 0..0;
    format.indent = format.indent.min(8);
    match format.kind {
        HtmlBlockKind::Header(_) => {
            format.list = None;
            format.quote = false;
            format.code = false;
        }
        HtmlBlockKind::ListItem if format.list.is_some() => {
            format.quote = false;
            format.code = false;
        }
        HtmlBlockKind::Code => {
            format.list = None;
            format.quote = false;
            format.code = true;
        }
        _ => format.kind = HtmlBlockKind::Paragraph,
    }
    format
}

fn block_attributes(format: &HtmlBlock) -> bool {
    format.kind != HtmlBlockKind::Paragraph
        || format.list.is_some()
        || format.indent != 0
        || format.quote
        || format.code
}

#[derive(Default)]
struct PlanBuilder {
    plan: HtmlClipboardPlan,
    pending_space: Option<HtmlStyle>,
    boundary_just_emitted: bool,
}
impl PlanBuilder {
    fn push(&mut self, text: &str, style: &HtmlStyle) {
        if text.is_empty() {
            return;
        }
        let start = self.plan.text.len();
        self.plan.text.push_str(text);
        self.boundary_just_emitted = false;
        if let Some(last) = self
            .plan
            .spans
            .last_mut()
            .filter(|last| last.range.end == start && last.style == *style)
        {
            last.range.end = self.plan.text.len();
        } else {
            self.plan.spans.push(HtmlClipboardSpan {
                range: start..self.plan.text.len(),
                style: style.clone(),
            });
        }
    }
    fn flush_space(&mut self) {
        if let Some(style) = self.pending_space.take() {
            if !self.plan.text.is_empty() && !self.plan.text.ends_with('\n') {
                self.push(" ", &style);
            }
        }
    }
    fn text(&mut self, text: &str, style: HtmlStyle, format: &HtmlBlock, pre: bool) {
        for character in text.chars() {
            if pre {
                self.flush_space();
                self.boundary_just_emitted = false;
                if character == '\n' {
                    self.newline(format, false);
                } else {
                    self.push(character.encode_utf8(&mut [0; 4]), &style);
                }
            } else if matches!(character, ' ' | '\t' | '\r' | '\n' | '\u{c}') {
                if !self.plan.text.ends_with('\u{a0}') {
                    self.pending_space.get_or_insert_with(|| style.clone());
                }
            } else if character == '\u{a0}' {
                // Quill's whitespace replacer preserves NBSP and drops adjacent
                // collapsible whitespace, rather than converting NBSP to space.
                self.pending_space = None;
                self.push("\u{a0}", &style);
            } else {
                self.flush_space();
                self.push(character.encode_utf8(&mut [0; 4]), &style);
            }
        }
    }
    fn newline(&mut self, format: &HtmlBlock, coalesce: bool) {
        self.pending_space = None;
        if coalesce && self.plan.text.ends_with('\n') {
            if let Some(last) = self.plan.lines.last_mut() {
                last.format = format.clone();
            }
            return;
        }
        let newline = self.plan.text.len();
        self.plan.text.push('\n');
        self.plan.lines.push(HtmlClipboardLine {
            newline,
            format: format.clone(),
        });
    }
    fn finish(&mut self) {
        self.flush_space();
        if self.plan.text.ends_with('\n')
            && self
                .plan
                .lines
                .last()
                .is_some_and(|last| !block_attributes(&last.format))
        {
            self.plan.text.pop();
            self.plan.lines.pop();
        }
    }
}

fn safe_link(value: &str) -> String {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    let blocked = value.chars().any(char::is_control)
        || lower
            .split_once(':')
            .is_some_and(|(scheme, _)| !matches!(scheme, "http" | "https" | "mailto" | "tel"));
    if blocked {
        "about:blank".into()
    } else {
        value.to_string()
    }
}

#[derive(Default)]
struct ClipboardTokens {
    state: RefCell<ClipboardTokenState>,
}
#[derive(Default)]
struct ClipboardTokenState {
    output: String,
    frames: Vec<ClipboardFrame>,
}
struct ClipboardFrame {
    input: String,
    closing: String,
    suppressed: bool,
}

impl TokenSink for ClipboardTokens {
    type Handle = ();
    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut state = self.state.borrow_mut();
        match token {
            Token::CharacterTokens(text) => {
                if !state.frames.last().is_some_and(|frame| frame.suppressed) {
                    state.output.push_str(&escape(&text));
                }
            }
            Token::TagToken(tag) => {
                let name = tag.name.to_string();
                if tag.kind == TagKind::EndTag {
                    if let Some(index) = state.frames.iter().rposition(|frame| frame.input == name)
                    {
                        let closings = state
                            .frames
                            .drain(index..)
                            .rev()
                            .map(|frame| frame.closing)
                            .collect::<String>();
                        state.output.push_str(&closings);
                    }
                    return TokenSinkResult::Continue;
                }
                let attr = |name: &str| {
                    tag.attrs
                        .iter()
                        .find(|attr| attr.name.local.as_ref() == name)
                        .map(|attr| attr.value.as_ref())
                        .unwrap_or("")
                };
                let parent_suppressed = state.frames.last().is_some_and(|frame| frame.suppressed);
                let formula = attr("class")
                    .split_ascii_whitespace()
                    .any(|class| class == "ql-formula");
                let suppressed = parent_suppressed || suppressed_element(&name) || formula;
                if formula && !parent_suppressed {
                    state.output.push_str("<img>"); // keep its line role; never emit an object in the plan
                }
                let output_name = match name.as_str() {
                    "embed" | "input" | "meta" | "link" | "source" | "track" | "hr" => None,
                    "strong" | "b" | "em" | "i" | "u" | "s" | "sub" | "sup" | "a" | "p" | "div"
                    | "br" | "img" | "ol" | "ul" | "li" | "blockquote" | "pre" | "h1" | "h2"
                    | "h3" | "h4" | "h5" | "h6" => Some(name.as_str()),
                    "address" | "article" | "aside" | "section" | "header" | "footer" | "main"
                    | "nav" | "figure" | "figcaption" | "dl" | "dt" | "dd" | "table" | "tr" => {
                        Some("div")
                    }
                    "html" | "body" => None,
                    _ => Some("span"),
                };
                let void = matches!(
                    name.as_str(),
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
                );
                let mut closing = String::new();
                if !suppressed {
                    if let Some(output_name) = output_name {
                        let heading =
                            matches!(output_name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
                        let mut css = String::new();
                        let block =
                            matches!(output_name, "p" | "div" | "li" | "blockquote" | "pre")
                                || heading;
                        let mut classes = Vec::new();
                        for class in attr("class").split_ascii_whitespace() {
                            if block
                                && class
                                    .strip_prefix("ql-indent-")
                                    .and_then(|value| value.parse::<u8>().ok())
                                    .is_some_and(|value| (1..=8).contains(&value))
                            {
                                classes.push(class.to_string());
                            } else if let Some(color) = class.strip_prefix("ql-color-") {
                                css.push_str(&format!("color:{color};"));
                            } else if let Some(color) = class.strip_prefix("ql-bg-") {
                                css.push_str(&format!("background-color:{color};"));
                            }
                        }
                        // Explicit inline styles take precedence over Quill's
                        // registered color/background class representations.
                        css.push_str(&supported_css(attr("style")));
                        // Nested Quill list items become indent attributes.
                        if output_name == "li" {
                            let depth = state
                                .frames
                                .iter()
                                .filter(|frame| matches!(frame.input.as_str(), "ol" | "ul"))
                                .count();
                            if depth > 1 {
                                classes.push(format!("ql-indent-{}", (depth - 1).min(8)));
                            }
                        }
                        state.output.push('<');
                        state.output.push_str(output_name);
                        if !classes.is_empty() {
                            let class_value = classes.join(" ");
                            state
                                .output
                                .push_str(&format!(" class=\"{}\"", escape(&class_value)));
                        }
                        if output_name == "a" {
                            state.output.push_str(&format!(
                                " href=\"{}\"",
                                escape(&safe_link(attr("href")))
                            ));
                        }
                        if !heading && !css.is_empty() {
                            state
                                .output
                                .push_str(&format!(" style=\"{}\"", escape(&css)));
                        }
                        state.output.push('>');
                        if heading {
                            state.output.push_str(&format!(
                                "<span style=\"font-weight:normal;{}\">",
                                escape(&css)
                            ));
                            closing.push_str("</span>");
                        }
                        if !void {
                            closing.push_str(&format!("</{output_name}>"));
                        }
                    }
                }
                if !void && !tag.self_closing {
                    state.frames.push(ClipboardFrame {
                        input: name.clone(),
                        closing,
                        suppressed,
                    });
                } else {
                    state.output.push_str(&closing);
                }
                return match name.as_str() {
                    "script" => TokenSinkResult::RawData(RawKind::ScriptData),
                    "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                        TokenSinkResult::RawData(RawKind::Rawtext)
                    }
                    "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
                    "plaintext" => TokenSinkResult::Plaintext,
                    _ => TokenSinkResult::Continue,
                };
            }
            _ => {} // comments, doctypes and unknown declarations never enter a plan
        }
        TokenSinkResult::Continue
    }
    fn end(&self) {
        let mut state = self.state.borrow_mut();
        let closing = state
            .frames
            .drain(..)
            .rev()
            .map(|frame| frame.closing)
            .collect::<String>();
        state.output.push_str(&closing);
    }
}

fn suppressed_element(name: &str) -> bool {
    matches!(
        name,
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
            | "noembed"
            | "noframes"
    )
}

fn supported_css(css: &str) -> String {
    css.split(';')
        .filter_map(|declaration| {
            let (name, value) = declaration.split_once(':')?;
            let name = name.trim().to_ascii_lowercase();
            let trimmed = value.trim();
            let clean = if trimmed
                .get(trimmed.len().saturating_sub(10)..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case("!important"))
            {
                trimmed[..trimmed.len() - 10].trim_end()
            } else {
                trimmed
            };
            let accepted = match name.as_str() {
                "font-weight" => {
                    clean.eq_ignore_ascii_case("bold")
                        || clean.eq_ignore_ascii_case("bolder")
                        || clean.parse::<u16>().is_ok_and(|weight| weight >= 700)
                }
                "font-style" => clean.eq_ignore_ascii_case("italic"),
                "color" | "background-color" | "background" => true,
                _ => false,
            };
            accepted.then(|| format!("{name}:{value};"))
        })
        .collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_html::HtmlListKind;
    fn style_at(plan: &HtmlClipboardPlan, byte: usize) -> &HtmlStyle {
        &plan
            .spans
            .iter()
            .find(|span| span.range.contains(&byte))
            .unwrap()
            .style
    }
    #[test]
    fn clipboard_html_converts_supported_inline_formats_and_utf8_ranges() {
        let plan = HtmlClipboardPlan::from_html("<p><strong>A😀</strong><em>B</em><u>C</u><s>D</s><sub>E</sub><sup>F</sup><span style='color:#123;background-color:rgb(1, 2, 3)'>G</span><a href='https://example.invalid/?x=1&amp;y=2'>H</a></p>").unwrap();
        assert_eq!(plan.text, "A😀BCDEFGH");
        assert!(style_at(&plan, 0).bold && style_at(&plan, 1).bold);
        assert!(style_at(&plan, 5).italic);
        assert!(style_at(&plan, 6).underline);
        assert!(style_at(&plan, 7).strike);
        assert!(style_at(&plan, 8).subscript);
        assert!(style_at(&plan, 9).superscript);
        assert_eq!(style_at(&plan, 10).color.as_deref(), Some("#123"));
        assert_eq!(
            style_at(&plan, 10).background.as_deref(),
            Some("rgb(1, 2, 3)")
        );
        assert_eq!(
            style_at(&plan, 11).link.as_deref(),
            Some("https://example.invalid/?x=1&y=2")
        );
        assert!(plan.lines.is_empty());
        for span in &plan.spans {
            assert!(plan.text.is_char_boundary(span.range.start));
            assert!(plan.text.is_char_boundary(span.range.end));
        }
    }
    #[test]
    fn clipboard_html_heading_default_does_not_become_inline_bold_or_code() {
        let plan = HtmlClipboardPlan::from_html("<h1>A<strong>B</strong><span style='font-weight:700 !IMPORTANT'>C</span></h1><h6>D</h6><pre>E</pre><p><code>F</code></p>").unwrap();
        assert_eq!(plan.text, "ABC\nD\nE\nF");
        assert!(!style_at(&plan, 0).bold);
        assert!(style_at(&plan, 1).bold && style_at(&plan, 2).bold);
        assert!(!style_at(&plan, 4).bold);
        assert!(plan.spans.iter().all(|span| !span.style.code));
        assert_eq!(plan.lines[0].format.kind, HtmlBlockKind::Header(1));
        assert_eq!(plan.lines[1].format.kind, HtmlBlockKind::Header(6));
        assert!(plan.lines[2].format.code);
    }
    #[test]
    fn clipboard_html_collapses_normal_whitespace_but_retains_nbsp_unicode_and_pre() {
        let plan = HtmlClipboardPlan::from_html(
            "<p>  A\r\n <b> B </b>  C&nbsp; \u{a0} D\u{200b}😀 </p><pre>  X\r\n\tY\n</pre>",
        )
        .unwrap();
        assert_eq!(plan.text, "A B C\u{a0}\u{a0}D\u{200b}😀\n  X\n\tY\n");
        assert_eq!(plan.lines.len(), 3);
        assert!(plan.lines[1].format.code && plan.lines[2].format.code);
        assert_eq!(HtmlClipboardPlan::from_html("A  B ").unwrap().text, "A B ");
        assert_eq!(HtmlClipboardPlan::from_html("  ").unwrap().text, "");
    }
    #[test]
    fn clipboard_html_retains_actual_newline_attributes_and_removes_only_plain_tail() {
        let plan =
            HtmlClipboardPlan::from_html("<p>A<br>B</p><p><br></p><blockquote>C</blockquote>")
                .unwrap();
        assert_eq!(plan.text, "A\nB\n\nC\n");
        assert_eq!(
            plan.lines
                .iter()
                .map(|line| line.newline)
                .collect::<Vec<_>>(),
            vec![1, 3, 4, 6]
        );
        assert!(!block_attributes(&plan.lines[0].format));
        assert!(plan.lines[3].format.quote);
        let plan = HtmlClipboardPlan::from_html("<p>A</p><p>B</p>").unwrap();
        assert_eq!(plan.text, "A\nB");
        assert_eq!(plan.lines.len(), 1);
        assert_eq!(plan.lines[0].newline, 1);
        assert_eq!(plan.lines[0].format.kind, HtmlBlockKind::Paragraph);
        assert_eq!(
            HtmlClipboardPlan::from_html("<p></p><h1></h1>")
                .unwrap()
                .text,
            ""
        );
        assert_eq!(
            HtmlClipboardPlan::from_html("<h2><br></h2>").unwrap().text,
            "\n"
        );
        assert_eq!(
            HtmlClipboardPlan::from_html("<p><br><br></p>")
                .unwrap()
                .text,
            ""
        );
    }
    #[test]
    fn clipboard_html_preserves_supported_lists_indent_and_ql_color_classes() {
        let plan = HtmlClipboardPlan::from_html("<ul><li class='ql-indent-2 vendor'>A</li><li><span class='ql-color-red ql-bg-yellow'>B</span><ol><li>C</li></ol></li></ul>").unwrap();
        assert_eq!(plan.text, "A\nB\nC\n");
        assert_eq!(plan.lines[0].format.list, Some(HtmlListKind::Bullet));
        assert_eq!(plan.lines[0].format.indent, 2);
        assert_eq!(plan.lines[1].format.list, Some(HtmlListKind::Bullet));
        assert_eq!(plan.lines[2].format.list, Some(HtmlListKind::Ordered));
        assert_eq!(plan.lines[2].format.indent, 1);
        assert_eq!(style_at(&plan, 2).color.as_deref(), Some("red"));
        assert_eq!(style_at(&plan, 2).background.as_deref(), Some("yellow"));
        let plan = HtmlClipboardPlan::from_html("<span class='ql-color-red ql-bg-yellow' style='color:blue;background-color:#123'>A</span>").unwrap();
        assert_eq!(style_at(&plan, 0).color.as_deref(), Some("blue"));
        assert_eq!(style_at(&plan, 0).background.as_deref(), Some("#123"));
    }
    #[test]
    fn clipboard_html_sanitizes_blocked_links_without_dropping_text_or_link_semantics() {
        for href in [
            "file:///C:/private/a",
            "javascript:alert(1)",
            "data:text/html,unsafe",
            "cid:private",
            "java&#10;script:alert(1)",
        ] {
            let plan = HtmlClipboardPlan::from_html(&format!("<a href='{href}'>A</a>")).unwrap();
            assert_eq!(plan.text, "A");
            assert_eq!(
                style_at(&plan, 0).link.as_deref(),
                Some("about:blank"),
                "{href}"
            );
        }
        for href in [
            "https://example.invalid",
            "http://example.invalid",
            "mailto:a@example.invalid",
            "tel:123",
            "relative/path",
            "//example.invalid/a",
        ] {
            let plan = HtmlClipboardPlan::from_html(&format!("<a href='{href}'>A</a>")).unwrap();
            assert_eq!(style_at(&plan, 0).link.as_deref(), Some(href));
        }
    }
    #[test]
    fn clipboard_html_filters_objects_active_content_and_unlisted_fonts_without_fffc() {
        let plan = HtmlClipboardPlan::from_html("<!--opaque--><p>A<img src='https://example.invalid/x'><span class='ql-formula' data-value='x'><span>formula fallback</span></span><iframe src='https://example.invalid'>hidden</iframe><video>hidden</video><script>const x='<img src=x>'; </script><style>body{color:red}</style><svg><text>hidden</text></svg><template>hidden</template><b style='font-size:40px;font-family:other;text-align:right;direction:rtl'>B</b><code>C</code><font color='red' face='other'>D</font></p>").unwrap();
        assert_eq!(plan.text, "ABCD");
        assert!(!plan.text.contains('\u{fffc}'));
        assert!(style_at(&plan, 1).bold);
        assert!(!style_at(&plan, 2).code);
        assert_eq!(style_at(&plan, 3).color, None);
        let plan = HtmlClipboardPlan::from_html("<p>A</p><p><img src=x></p><p>B</p>").unwrap();
        assert_eq!(plan.text, "A\n\nB");
        assert_eq!(
            HtmlClipboardPlan::from_html("<img src=x>").unwrap().text,
            ""
        );
        assert_eq!(
            HtmlClipboardPlan::from_html("<script>unclosed<img src=x>")
                .unwrap()
                .text,
            ""
        );
    }
    #[test]
    fn clipboard_html_unknown_structure_does_not_carry_markup_into_plan() {
        let original = "<section vendor='keep'><widget onclick='unsafe'>A&amp;B</widget></section><!--keep--><p>C</p>";
        let plan = HtmlClipboardPlan::from_html(original).unwrap();
        assert_eq!(plan.text, "A&B\nC");
        assert_eq!(plan.lines[0].format.source_range, 0..0);
        assert!(plan.lines[0].format.elements.is_empty());
        assert!(!plan.text.contains("onclick") && !plan.text.contains("widget"));
    }
}
