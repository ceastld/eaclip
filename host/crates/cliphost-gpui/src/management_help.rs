//! Offline rendering of the four shipped DocsPage articles. No Markdown/MDX
//! code is executed; the known document components use existing native APIs.

use super::ManagementPage;
use crate::smooth_wheel::SmoothWheelExt;
use crate::{preview::ThemeTokens, view::color};
use cliphost_core::i18n::tr;
use gpui::{
    canvas, div, img, point, prelude::*, px, AnyElement, ClipboardItem, Context, FocusHandle,
    FontWeight, HighlightStyle, InteractiveText, ScrollHandle, SharedString, StyledText, Task,
    Timer, Window,
};
use serde_json::Value;
use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

const USAGE: &str = include_str!("../../../../frontend/src/docs/clipboard-usage.mdx");
const COMMANDS: &str = include_str!("../../../../frontend/src/docs/commands.mdx");
const SOFTWARE: &str = include_str!("../../../../frontend/src/docs/software-info.mdx");
const ABOUT: &str = include_str!("../../../../frontend/src/docs/about.mdx");
const ABOUT_LINKS: &str = include_str!("../../../../frontend/src/docs/aboutLinks.ts");

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Inline {
    text: String,
    spans: Vec<Span>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Span {
    range: Range<usize>,
    bold: bool,
    code: bool,
    href: Option<String>,
}

fn append_inline(out: &mut Inline, input: &str, bold: bool, code: bool, href: Option<&str>) {
    let mut rest = input;
    while !rest.is_empty() {
        if !code {
            let delimiter = if rest.starts_with("**") {
                Some(("**", true))
            } else if rest.starts_with('`') {
                Some(("`", false))
            } else {
                None
            };
            if let Some((delimiter, strong)) = delimiter {
                if let Some(end) = rest[delimiter.len()..].find(delimiter) {
                    append_inline(
                        out,
                        &rest[delimiter.len()..delimiter.len() + end],
                        bold || strong,
                        !strong,
                        href,
                    );
                    rest = &rest[delimiter.len() * 2 + end..];
                    continue;
                }
            }
            if rest.starts_with('[') {
                if let Some(label_end) = rest.find("](") {
                    if let Some(url_end) = rest[label_end + 2..].find(')') {
                        let url = &rest[label_end + 2..label_end + 2 + url_end];
                        append_inline(out, &rest[1..label_end], bold, false, Some(url));
                        rest = &rest[label_end + 3 + url_end..];
                        continue;
                    }
                }
            }
        }
        if !code
            && rest.starts_with('\\')
            && rest
                .chars()
                .nth(1)
                .is_some_and(|ch| matches!(ch, '|' | '*' | '`' | '[' | ']' | '\\'))
        {
            rest = &rest[1..];
        }
        let ch = rest.chars().next().unwrap();
        let start = out.text.len();
        out.text.push(ch);
        let end = out.text.len();
        if let Some(last) = out.spans.last_mut().filter(|span| {
            span.range.end == start
                && span.bold == bold
                && span.code == code
                && span.href.as_deref() == href
        }) {
            last.range.end = end;
        } else {
            out.spans.push(Span {
                range: start..end,
                bold,
                code,
                href: href.map(str::to_string),
            });
        }
        rest = &rest[ch.len_utf8()..];
    }
}
fn inline(value: &str) -> Inline {
    let mut out = Inline::default();
    append_inline(&mut out, value, false, false, None);
    out
}

#[derive(Clone, Debug)]
enum Block {
    Heading {
        level: u8,
        id: String,
        title: Inline,
    },
    Paragraph(Inline),
    List {
        marker: String,
        text: Inline,
    },
    Table {
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
    Note {
        title: String,
        children: Vec<Block>,
    },
    Code(String),
    Commands,
    Software,
    Donate,
}
#[derive(Clone, Debug)]
struct Article {
    blocks: Vec<Block>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct TocItem {
    id: String,
    level: u8,
    title: String,
}

fn slug(doc: &str, title: &str, used: &mut BTreeSet<String>) -> String {
    let mut normalized = String::new();
    let mut whitespace = false;
    for ch in title.trim().to_lowercase().chars() {
        if ch.is_whitespace() {
            if !whitespace {
                normalized.push('-');
            }
            whitespace = true;
        } else {
            whitespace = false;
            if ch.is_alphanumeric() || ch == '-' {
                normalized.push(ch);
            }
        }
    }
    let normalized = normalized.chars().take(48).collect::<String>();
    let base = if normalized.is_empty() {
        "section"
    } else {
        &normalized
    };
    let mut result = format!("{doc}--{base}");
    let mut suffix = 2;
    while used.contains(&result) {
        result = format!("{doc}--{base}-{suffix}");
        suffix += 1;
    }
    used.insert(result.clone());
    result
}
fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    Some(tag[start..tag[start..].find('"')? + start].to_string())
}
fn constants() -> BTreeMap<String, String> {
    ABOUT_LINKS
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("export const ")?;
            let (name, value) = rest.split_once(" = ")?;
            Some((
                name.to_string(),
                value.strip_prefix('"')?.split('"').next()?.to_string(),
            ))
        })
        .collect()
}
fn expand_components(input: &str) -> String {
    let mut value = input.to_string();
    for (name, text) in constants() {
        value = value.replace(&format!("href={{{name}}}"), &format!("href=\"{text}\""));
        value = value.replace(&format!("{{{name}}}"), &text);
    }
    for name in ["ExternalLink", "DocExternalButton", "DocShellUriLink"] {
        while let Some(start) = value.find(&format!("<{name} ")) {
            let Some(open_end) = value[start..].find('>').map(|end| start + end) else {
                break;
            };
            let close = format!("</{name}>");
            let Some(close_start) = value[open_end + 1..]
                .find(&close)
                .map(|end| open_end + 1 + end)
            else {
                break;
            };
            let target = attribute(&value[start..open_end], "href").unwrap_or_default();
            let label = value[open_end + 1..close_start]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            value.replace_range(
                start..close_start + close.len(),
                &format!("[{label}]({target})"),
            );
        }
    }
    // Contact rows contain a decorative span, not an executable MDX node.
    while let Some(start) = value.find("<span") {
        let Some(end) = value[start..].find('>').map(|end| start + end + 1) else {
            break;
        };
        value.replace_range(start..end, "");
    }
    value.replace("</span>", "")
}
fn table_cells(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for ch in line
        .trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .chars()
    {
        if escaped {
            if ch != '|' {
                cell.push('\\');
            }
            cell.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(cell.trim().into());
            cell.clear();
        } else {
            cell.push(ch);
        }
    }
    if escaped {
        cell.push('\\');
    }
    cells.push(cell.trim().into());
    cells
}
fn separator(line: &str) -> bool {
    let cells = table_cells(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let cell = cell.trim_matches(':').trim();
            cell.len() >= 3 && cell.chars().all(|ch| ch == '-')
        })
}
fn list_marker(line: &str) -> Option<(String, &str)> {
    if let Some(body) = line.strip_prefix("- ") {
        return Some(("•".into(), body));
    }
    let (number, body) = line.split_once(". ")?;
    (!number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| (format!("{number}."), body))
}
fn parse_blocks(
    doc: &str,
    lines: &[&str],
    at: &mut usize,
    used: &mut BTreeSet<String>,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    while *at < lines.len() {
        let line = lines[*at].trim();
        *at += 1;
        if line == "</Note>" {
            break;
        }
        if line.is_empty() {
            continue;
        }
        if line.starts_with("import ") {
            while !lines[*at - 1].trim_end().ends_with(';') && *at < lines.len() {
                *at += 1;
            }
            continue;
        }
        if line.starts_with("<Note ") {
            blocks.push(Block::Note {
                title: attribute(line, "title").unwrap_or_else(|| "提示".into()),
                children: parse_blocks(doc, lines, at, used),
            });
            continue;
        }
        if line.starts_with("<DocNoteRow ") {
            let label = attribute(line, "label").unwrap_or_default();
            let mut body = String::new();
            while *at < lines.len() && !lines[*at].trim().starts_with("</DocNoteRow>") {
                body.push_str(lines[*at]);
                body.push(' ');
                *at += 1;
            }
            if *at < lines.len() {
                *at += 1;
            }
            blocks.push(Block::Paragraph(inline(&format!(
                "**{label}**: {}",
                expand_components(&body)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            ))));
            continue;
        }
        if line.starts_with("<ExternalLink ")
            || line.starts_with("<DocExternalButton ")
            || line.starts_with("<DocShellUriLink ")
        {
            let mut body = line.to_string();
            while !body.contains("</") && *at < lines.len() {
                body.push(' ');
                body.push_str(lines[*at]);
                *at += 1;
            }
            blocks.push(Block::Paragraph(inline(&expand_components(&body))));
            continue;
        }
        if line == "<DocCommandCatalog />" {
            blocks.push(Block::Commands);
            continue;
        }
        if line == "<SoftwareInfoPanel />" {
            blocks.push(Block::Software);
            continue;
        }
        if line == "<DonateQrGallery />" {
            blocks.push(Block::Donate);
            continue;
        }
        if line.starts_with('<') {
            continue;
        } // known layout wrappers
        if line.starts_with("```") {
            let mut code = Vec::new();
            while *at < lines.len() && !lines[*at].trim().starts_with("```") {
                code.push(lines[*at]);
                *at += 1;
            }
            if *at < lines.len() {
                *at += 1;
            }
            blocks.push(Block::Code(code.join("\n")));
            continue;
        }
        let level = line.chars().take_while(|ch| *ch == '#').count();
        if (1..=6).contains(&level) && line.as_bytes().get(level) == Some(&b' ') {
            let title = inline(line[level..].trim());
            let id = if level == 1 {
                format!("doc-{doc}")
            } else {
                slug(doc, &title.text, used)
            };
            blocks.push(Block::Heading {
                level: level as u8,
                id,
                title,
            });
            continue;
        }
        if line.starts_with('|') && *at < lines.len() && separator(lines[*at].trim()) {
            let header = table_cells(line)
                .into_iter()
                .map(|cell| inline(&cell))
                .collect::<Vec<_>>();
            *at += 1;
            let mut rows = Vec::new();
            while *at < lines.len() && lines[*at].trim().starts_with('|') {
                let mut row = table_cells(lines[*at])
                    .into_iter()
                    .map(|cell| inline(&cell))
                    .collect::<Vec<_>>();
                row.resize(header.len(), Inline::default());
                row.truncate(header.len());
                rows.push(row);
                *at += 1;
            }
            blocks.push(Block::Table { header, rows });
            continue;
        }
        if let Some((marker, text)) = list_marker(line) {
            blocks.push(Block::List {
                marker,
                text: inline(text),
            });
        } else {
            blocks.push(Block::Paragraph(inline(line)));
        }
    }
    blocks
}
fn documents() -> Vec<Article> {
    [
        ("clip-usage", USAGE),
        ("commands", COMMANDS),
        ("software-info", SOFTWARE),
        ("about", ABOUT),
    ]
    .into_iter()
    .map(|(id, source)| Article {
        blocks: parse_blocks(
            id,
            &source.lines().collect::<Vec<_>>(),
            &mut 0,
            &mut BTreeSet::new(),
        ),
    })
    .collect()
}
fn toc(articles: &[Article]) -> Vec<TocItem> {
    articles
        .iter()
        .flat_map(|article| article.blocks.iter())
        .filter_map(|block| {
            if let Block::Heading { level, id, title } = block {
                if *level <= 4 {
                    return Some(TocItem {
                        id: id.clone(),
                        level: *level,
                        title: title.text.clone(),
                    });
                }
            }
            None
        })
        .collect()
}
fn matches_command(item: &Value, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    ["command", "label", "description"].into_iter().any(|key| {
        item[key]
            .as_str()
            .is_some_and(|value| value.to_lowercase().contains(&query))
    }) || item["exampleSections"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|section| {
            section["title"]
                .as_str()
                .is_some_and(|title| title.to_lowercase().contains(&query))
                || section["rows"].as_array().into_iter().flatten().any(|row| {
                    ["line", "hint"].into_iter().any(|key| {
                        row[key]
                            .as_str()
                            .is_some_and(|value| value.to_lowercase().contains(&query))
                    })
                })
        })
}
fn decode_fragment(route: &str) -> Option<String> {
    let raw = route.split_once('#')?.1;
    let mut bytes = Vec::new();
    let mut chars = raw.bytes();
    while let Some(ch) = chars.next() {
        if ch == b'%' {
            let a = (chars.next()? as char).to_digit(16)?;
            let b = (chars.next()? as char).to_digit(16)?;
            bytes.push((a * 16 + b) as u8);
        } else {
            bytes.push(ch);
        }
    }
    String::from_utf8(bytes).ok().filter(|id| !id.is_empty())
}

#[derive(Default)]
struct Layout {
    top: f32,
    headings: BTreeMap<String, f32>,
}
fn target_offset(offset: f32, heading_top: f32, viewport_top: f32) -> f32 {
    (offset - (heading_top - viewport_top - 12.)).min(0.)
}
fn eased_offset(start: f32, target: f32, fraction: f32) -> f32 {
    let fraction = fraction.clamp(0., 1.);
    start + (target - start) * (1. - (1. - fraction).powi(3))
}
fn active_id(items: &[TocItem], layout: &Layout) -> String {
    let mut active = items
        .first()
        .map(|item| item.id.clone())
        .unwrap_or_default();
    for item in items {
        if let Some(y) = layout.headings.get(&item.id) {
            if *y <= layout.top + 72. {
                active = item.id.clone();
            } else {
                break;
            }
        }
    }
    active
}
pub(super) struct HelpState {
    articles: Vec<Article>,
    items: Vec<TocItem>,
    scroll: ScrollHandle,
    layout: Rc<RefCell<Layout>>,
    active: String,
    pending: Option<String>,
    locked_until: Option<Instant>,
    selected_command: String,
    tab_focus: FocusHandle,
    copied: Option<String>,
    copy_generation: u64,
    last_offset: Option<f32>,
    sync_generation: u64,
    search_placeholder: Option<String>,
    scroll_generation: u64,
    scroll_task: Option<Task<()>>,
}
impl HelpState {
    pub(super) fn new(cx: &mut Context<ManagementPage>) -> Self {
        let articles = documents();
        let items = toc(&articles);
        Self {
            articles,
            items,
            scroll: ScrollHandle::new(),
            layout: Rc::default(),
            active: "doc-clip-usage".into(),
            pending: None,
            locked_until: None,
            selected_command: "paste".into(),
            tab_focus: cx.focus_handle(),
            copied: None,
            copy_generation: 0,
            last_offset: None,
            sync_generation: 0,
            search_placeholder: None,
            scroll_generation: 0,
            scroll_task: None,
        }
    }
    fn jump(&mut self, id: String) {
        self.active = id.clone();
        self.pending = Some(id);
        self.locked_until = Some(Instant::now() + Duration::from_millis(120));
    }
    fn take_jump(&mut self) -> Option<f32> {
        let layout = self.layout.borrow();
        if let Some(id) = self.pending.clone() {
            if let Some(y) = layout.headings.get(&id) {
                let target = target_offset(f32::from(self.scroll.offset().y), *y, layout.top);
                self.pending = None;
                self.active = id;
                self.locked_until = Some(Instant::now() + Duration::from_millis(320));
                return Some(target);
            }
        }
        None
    }
    fn sync_layout(&mut self) -> bool {
        let layout = self.layout.borrow();
        if self
            .locked_until
            .is_some_and(|until| Instant::now() < until)
        {
            return false;
        }
        let active = active_id(&self.items, &layout);
        if active != self.active {
            self.active = active;
            true
        } else {
            false
        }
    }
}

// Native rendering is below the pure document/anchor model.
fn styled(text: &Inline, t: &ThemeTokens) -> StyledText {
    StyledText::new(text.text.clone()).with_highlights(text.spans.iter().map(|span| {
        (
            span.range.clone(),
            HighlightStyle {
                font_weight: span.bold.then_some(FontWeight::SEMIBOLD),
                color: span.href.as_ref().map(|_| color(&t.accent)),
                background_color: span.code.then(|| color(&t.secondary)),
                ..Default::default()
            },
        )
    }))
}
fn text_view(
    page: &ManagementPage,
    text: &Inline,
    id: &str,
    t: &ThemeTokens,
    cx: &Context<ManagementPage>,
) -> AnyElement {
    let links = text
        .spans
        .iter()
        .filter_map(|span| span.href.clone().map(|href| (span.range.clone(), href)))
        .collect::<Vec<_>>();
    if links.is_empty() {
        return styled(text, t).into_any_element();
    }
    let weak = cx.weak_entity();
    let ranges = links.iter().map(|(range, _)| range.clone()).collect();
    let _ = page;
    InteractiveText::new(SharedString::from(id.to_string()), styled(text, t))
        .on_click(ranges, move |ix, window, cx| {
            if let Some((_, href)) = links.get(ix) {
                let _ = weak.update(cx, |page, cx| {
                    if href.starts_with('#') {
                        if let Some(id) = decode_fragment(href)
                            .filter(|id| page.help.items.iter().any(|item| &item.id == id))
                        {
                            page.help.jump(id);
                            cx.notify();
                        }
                    } else if href.starts_with("https://")
                        || href.starts_with("http://")
                        || href.starts_with("mailto:")
                    {
                        page.open_target(href.clone(), true, window, cx);
                    }
                });
            }
        })
        .into_any_element()
}
fn copy(
    page: &mut ManagementPage,
    id: String,
    text: String,
    duration: Duration,
    cx: &mut Context<ManagementPage>,
) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    page.help.copy_generation = page.help.copy_generation.wrapping_add(1);
    let generation = page.help.copy_generation;
    let lifetime = page.generation;
    page.help.copied = Some(id);
    cx.spawn(async move |this, cx| {
        Timer::after(duration).await;
        let _ = this.update(cx, |page, cx| {
            if page.page == "docs"
                && page.generation == lifetime
                && page.help.copy_generation == generation
            {
                page.help.copied = None;
                cx.notify();
            }
        });
    })
    .detach();
    cx.notify();
}
fn schedule_active(page: &mut ManagementPage, cx: &mut Context<ManagementPage>) {
    page.help.sync_generation = page.help.sync_generation.wrapping_add(1);
    let generation = page.help.sync_generation;
    let lifetime = page.generation;
    cx.spawn(async move |this, cx| {
        Timer::after(Duration::from_millis(120)).await;
        let _ = this.update(cx, |page, cx| {
            if page.page == "docs"
                && page.generation == lifetime
                && page.help.sync_generation == generation
            {
                page.help.locked_until = None;
                if page.help.sync_layout() {
                    cx.notify();
                }
            }
        });
    })
    .detach();
}
fn start_jump(page: &mut ManagementPage, target: f32, cx: &mut Context<ManagementPage>) {
    page.help.scroll_generation = page.help.scroll_generation.wrapping_add(1);
    let generation = page.help.scroll_generation;
    let lifetime = page.generation;
    let start = f32::from(page.help.scroll.offset().y);
    page.help.scroll_task = Some(cx.spawn(async move |this, cx| {
        let began = Instant::now();
        loop {
            Timer::after(Duration::from_millis(16)).await;
            let fraction = (began.elapsed().as_secs_f32() / 0.2).min(1.);
            let valid = this
                .update(cx, |page, cx| {
                    if page.page != "docs"
                        || page.generation != lifetime
                        || page.help.scroll_generation != generation
                    {
                        return false;
                    }
                    let offset = page.help.scroll.offset();
                    page.help
                        .scroll
                        .set_offset(point(offset.x, px(eased_offset(start, target, fraction))));
                    if fraction >= 1. {
                        page.help.locked_until = Some(Instant::now() + Duration::from_millis(120));
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !valid || fraction >= 1. {
                break;
            }
        }
    }));
}
fn command_view(
    page: &ManagementPage,
    t: &ThemeTokens,
    cx: &mut Context<ManagementPage>,
) -> AnyElement {
    let mut root = rsx! {
        <div flex flex_col gap={px(10.)}>
            {page.command_search.clone()}
        </div>
    };
    let items = match &page.catalogue {
        Ok(items) => items
            .iter()
            .filter(|item| matches_command(item, &page.query))
            .cloned()
            .collect::<Vec<_>>(),
        Err(error) => {
            return root
                .child(div().text_color(color(&t.error)).child(error.clone()))
                .into_any_element()
        }
    };
    let Some(selected) = items
        .iter()
        .find(|item| item["command"] == page.help.selected_command)
        .or_else(|| items.first())
    else {
        return root
            .child(tr("无匹配命令。", "No matching commands."))
            .into_any_element();
    };
    let selected_id = super::string(selected, "command");
    let keyboard_items = items
        .iter()
        .map(|item| super::string(item, "command"))
        .collect::<Vec<_>>();
    let tabs = div()
        .track_focus(&page.help.tab_focus)
        .flex()
        .flex_wrap()
        .gap(px(5.))
        .on_key_down(cx.listener(move |page, event: &gpui::KeyDownEvent, _, cx| {
            let step = match event.keystroke.key.as_str() {
                "right" => 1,
                "left" => keyboard_items.len().saturating_sub(1),
                _ => return,
            };
            let at = keyboard_items
                .iter()
                .position(|id| id == &page.help.selected_command)
                .unwrap_or(0);
            page.help.selected_command = keyboard_items[(at + step) % keyboard_items.len()].clone();
            cx.stop_propagation();
            cx.notify();
        }))
        .children(items.iter().map(|item| {
            let id = super::string(item, "command");
            page.button(
                format!("doc-cmd-tab-{id}"),
                super::string(item, "label"),
                true,
                t,
            )
            .when(id == selected_id, |tab| tab.bg(color(&t.tab_active)))
            .on_click(cx.listener(move |page, _, window, cx| {
                page.help.selected_command = id.clone();
                page.help.tab_focus.focus(window);
                cx.notify();
            }))
        }));
    root = root
        .child(tabs)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
            "{} · {}",
            super::string(selected, "label"),
            selected_id
        )))
        .child(super::string(selected, "description"));
    for (si, section) in selected["exampleSections"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let title = super::string(section, "title");
        if !title.is_empty() {
            root = root.child(div().font_weight(FontWeight::SEMIBOLD).child(title));
        }
        for (ri, row) in section["rows"].as_array().into_iter().flatten().enumerate() {
            let line = super::string(row, "line");
            let text = line.clone();
            let key = format!("command:{selected_id}:{si}:{ri}");
            let copy_key = key.clone();
            root = root.child(
                rsx! {
                    <div flex items_start gap={px(8.)} border_1
                        border_color={color(&t.border_secondary)} rounded={px(6.)} p={px(9.)}>
                        <div flex_1 min_w_0 flex flex_col gap={px(4.)}>
                            <div font_family={"Consolas"}>{line}</div>
                            <div text_color={color(&t.text_secondary)}>
                                {super::string(row, "hint")}
                            </div>
                        </div>
                        {page.button(
                            format!("copy:{key}"),
                            if page.help.copied.as_ref() == Some(&key) {
                                tr("已复制", "Copied")
                            } else {
                                tr("复制", "Copy")
                            },
                            true,
                            t,
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            copy(
                                page,
                                copy_key.clone(),
                                text.clone(),
                                Duration::from_millis(2000),
                                cx,
                            )
                        }))}
                    </div>
                },
            );
        }
    }
    root.into_any_element()
}
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::String(value) if value.is_empty() => "—".into(),
        Value::String(value) => value.clone(),
        Value::Bool(true) => tr("是", "Yes").into(),
        Value::Bool(false) => tr("否", "No").into(),
        value => value.to_string(),
    }
}
fn software_view(page: &ManagementPage, t: &ThemeTokens) -> AnyElement {
    if page.loading {
        return div()
            .text_color(color(&t.text_muted))
            .child(tr("正在读取版本信息…", "Reading version information…"))
            .into_any_element();
    }
    if !page.info.is_object() {
        return div()
            .child(tr("暂无软件信息。", "No software information."))
            .into_any_element();
    }
    let host = &page.info["clipHost"];
    let quicker = &page.info["quicker"];
    let version = if host["fileVersion"].is_null() {
        &host["assemblyVersion"]
    } else {
        &host["fileVersion"]
    };
    let mut root = rsx! {
        <div flex flex_col gap={px(7.)}>
            <div font_weight={FontWeight::SEMIBOLD}>{"ClipHost"}</div>
        </div>
    };
    let rows = [
        (tr("版本", "Version"), format_value(version)),
        (
            tr("程序集版本", "Assembly version"),
            format_value(&host["assemblyVersion"]),
        ),
        (
            tr("构建配置", "Build configuration"),
            if host["isDebugConfiguration"] == true {
                "Debug".into()
            } else {
                "Release".into()
            },
        ),
        (tr("运行时", "Runtime"), format_value(&host["runtime"])),
    ];
    for (label, value) in rows {
        root = root.child(
            rsx! {
                <div flex gap={px(12.)}>
                    <div w={px(130.)} flex_none text_color={color(&t.text_muted)}>
                        {label}
                    </div>
                    <div flex_1 min_w_0>{value}</div>
                </div>
            },
        );
    }
    root = root.child(
        rsx! {
            <div pt={px(9.)} font_weight={FontWeight::SEMIBOLD}>
                {tr("Quicker 插件", "Quicker plugin")}
            </div>
        },
    );
    if quicker["connected"] == true {
        for (label, value) in [
            (
                tr("连接状态", "Connection"),
                tr("已连接", "Connected").to_string(),
            ),
            (
                tr("插件版本", "Plugin version"),
                format_value(&quicker["pluginVersion"]),
            ),
            (
                tr("Quicker 版本", "Quicker version"),
                format_value(&quicker["quickerVersion"]),
            ),
            (
                tr("RPC 协议版本", "RPC protocol version"),
                format_value(&quicker["protocolVersion"]),
            ),
        ] {
            root = root.child(
                rsx! {
                    <div flex gap={px(12.)}>
                        <div w={px(130.)} flex_none text_color={color(&t.text_muted)}>
                            {label}
                        </div>
                        <div flex_1 min_w_0>{value}</div>
                    </div>
                },
            );
        }
    } else {
        let hint = quicker["error"]
            .as_str()
            .or_else(|| quicker["hint"].as_str())
            .unwrap_or(tr(
                "未连接到 Quicker 插件。",
                "Not connected to the Quicker plugin.",
            ));
        root = root.child(
            div()
                .text_color(color(&t.text_muted))
                .child(hint.to_string()),
        );
    }
    root.into_any_element()
}
fn table_view(
    page: &ManagementPage,
    header: &[Inline],
    rows: &[Vec<Inline>],
    id: &str,
    width: f32,
    t: &ThemeTokens,
    cx: &Context<ManagementPage>,
) -> AnyElement {
    let count = header.len().max(1);
    let width = width.max(if count >= 3 { 620. } else { 460. });
    let mut table = rsx! { <div w={px(width)} flex flex_col /> };
    for (ri, row) in std::iter::once(header)
        .chain(rows.iter().map(Vec::as_slice))
        .enumerate()
    {
        let mut cells = rsx! { <div flex /> }
            .when(ri == 0, |row| {
                row.bg(color(&t.secondary))
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .when(ri > 0 && ri % 2 == 0, |row| row.bg(color(&t.surface)));
        for (ci, cell) in row.iter().enumerate() {
            let weight = if count == 3 {
                if ci == 2 {
                    0.5
                } else {
                    0.25
                }
            } else {
                1. / count as f32
            };
            cells = cells.child(rsx! {
                <div w={px(width * weight)} flex_none border_r_1 border_b_1
                    border_color={color(&t.border_secondary)} px={px(8.)} py={px(6.)}
                    text_size={px(12.)}>
                    {text_view(page, cell, &format!("{id}:{ri}:{ci}"), t, cx)}
                </div>
            });
        }
        table = table.child(cells);
    }
    rsx! {
        <div id={SharedString::from(id.to_string())} w_full min_w_0 overflow_x_scroll
            rounded={px(6.)} border_1 border_color={color(&t.border_secondary)}>
            {table}
        </div>
    }
    .into_any_element()
}
fn block_view(
    page: &ManagementPage,
    block: &Block,
    key: &str,
    width: f32,
    t: &ThemeTokens,
    cx: &mut Context<ManagementPage>,
) -> AnyElement {
    match block {
        Block::Heading { level, id, title } => {
            let layout = page.help.layout.clone();
            let anchor = id.clone();
            div()
                .relative()
                .pt(px(if *level == 1 { 0. } else { 10. }))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(match *level {
                    1 => 20.,
                    2 => 17.,
                    _ => 14.,
                }))
                .child(
                    canvas(
                        move |bounds, _, _| {
                            layout
                                .borrow_mut()
                                .headings
                                .insert(anchor.clone(), f32::from(bounds.top()));
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(styled(title, t))
                .into_any_element()
        }
        Block::Paragraph(text) => div()
            .child(text_view(page, text, key, t, cx))
            .into_any_element(),
        Block::List { marker, text } => rsx! {
            <div flex items_start gap={px(8.)}>
                <div w={px(24.)} flex_none>{marker.clone()}</div>
                <div flex_1 min_w_0>{text_view(page, text, key, t, cx)}</div>
            </div>
        }
            .into_any_element(),
        Block::Table { header, rows } => table_view(page, header, rows, key, width, t, cx),
        Block::Note { title, children } => rsx! {
            <div flex flex_col gap={px(6.)} p={px(12.)} rounded={px(6.)} border_1
                border_color={color(&t.border_secondary)} bg={color(&t.secondary)}>
                <div font_weight={FontWeight::SEMIBOLD}>{title.clone()}</div>
            </div>
        }
            .children(children.iter().enumerate().map(|(i, block)| {
                block_view(page, block, &format!("{key}:{i}"), width - 24., t, cx)
            }))
            .into_any_element(),
        Block::Code(text) => {
            let value = text.replace("\r\n", "\n").trim().to_string();
            let key = key.to_string();
            let copy_key = key.clone();
            rsx! {
                <div flex items_start gap={px(8.)} p={px(10.)} rounded={px(6.)}
                    bg={color(&t.secondary)}>
                    <div flex_1 min_w_0 font_family={"Consolas"}>{text.clone()}</div>
                    {page.button(
                        format!("copy-code:{key}"),
                        if page.help.copied.as_ref() == Some(&key) {
                            tr("已复制", "Copied")
                        } else {
                            tr("复制", "Copy")
                        },
                        true,
                        t,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        copy(
                            page,
                            copy_key.clone(),
                            value.clone(),
                            Duration::from_millis(1600),
                            cx,
                        )
                    }))}
                </div>
            }
                .into_any_element()
        }
        Block::Commands => command_view(page, t, cx),
        Block::Software => software_view(page, t),
        Block::Donate => div()
            .flex()
            .flex_wrap()
            .gap(px(12.))
            .children(
                [
                    (page.alipay.clone(), tr("支付宝", "Alipay")),
                    (page.wepay.clone(), tr("微信", "WeChat")),
                ]
                .into_iter()
                .map(|(image, label)| {
                    rsx! {
                        <div flex flex_col gap={px(5.)}>
                            {img(image).w(px(180.)).h(px(180.))}
                            {label}
                        </div>
                    }
                }),
            )
            .into_any_element(),
    }
}
pub(super) fn render(
    page: &mut ManagementPage,
    window: &mut Window,
    cx: &mut Context<ManagementPage>,
) -> AnyElement {
    let placeholder = tr(
        "搜索命令、名称或说明…",
        "Search commands, names or descriptions…",
    )
    .to_string();
    if page.help.search_placeholder.as_ref() != Some(&placeholder) {
        page.help.search_placeholder = Some(placeholder.clone());
        let lifetime = page.generation;
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |page, cx| {
                if page.page == "docs"
                    && page.generation == lifetime
                    && page.help.search_placeholder.as_ref() == Some(&placeholder)
                {
                    page.command_search
                        .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
                }
            });
        });
    }
    let t = page.tokens.theme(page.dark).clone();
    let viewport_width = f32::from(window.viewport_size().width);
    let narrow = viewport_width < 720.;
    let toc_width = if narrow { 140. } else { 180. };
    let content_width = (viewport_width - toc_width - 60.).max(260.);
    let mut directory = rsx! {
        <div id={"help-directory"} w={px(toc_width)} flex_none min_h_0 />
    }
        .overflow_y_scroll()
        .smooth_scroll("help-directory", cx)
        .p(px(10.))
        .flex()
        .flex_col()
        .gap(px(2.))
        .bg(color(&t.secondary))
        .child(
            rsx! {
                <div text_size={px(11.)} font_weight={FontWeight::SEMIBOLD}
                    text_color={color(&t.text_muted)} pb={px(7.)}>
                    {tr("目录", "Contents")}
                </div>
            },
        );
    for item in page.help.items.clone() {
        let id = item.id.clone();
        let hover = color(&t.hover);
        directory = directory.child(
            rsx! {
                <div id={SharedString::from(format!("help-toc:{id}"))}
                    pl={px(6. + (item.level.saturating_sub(1)) as f32 * 9.)} pr={px(6.)}
                    py={px(5.)} text_size={px(12.)} rounded={px(5.)} cursor_pointer />
            }
                .hover(move |style| style.bg(hover))
                .when(page.help.active == id, |link| {
                    link.bg(color(&t.tab_active)).text_color(color(&t.accent))
                })
                .child(item.title)
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.help.jump(id.clone());
                    cx.notify();
                })),
        );
    }
    let layout = page.help.layout.clone();
    let weak = cx.weak_entity();
    let viewport_scroll = page.help.scroll.clone();
    let lifetime = page.generation;
    let mut content = rsx! {
        <div id={"help-documents"} relative flex_1 min_w_0 min_h_0 />
    }
        .overflow_y_scroll()
        .track_scroll(&page.help.scroll)
        .smooth_wheel("help-documents", &page.help.scroll, cx)
        .on_scroll_wheel(cx.listener(|page, _, _, cx| {
            page.help.scroll_generation = page.help.scroll_generation.wrapping_add(1);
            page.help.scroll_task = None;
            page.help.pending = None;
            page.help.locked_until = None;
            schedule_active(page, cx);
        }))
        .p(px(14.))
        .flex()
        .flex_col()
        .gap(px(16.))
        .child(
            canvas(
                move |_, _, cx| {
                    let mut current = layout.borrow_mut();
                    current.top = f32::from(viewport_scroll.bounds().top());
                    current.headings.clear();
                    drop(current);
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |page, cx| {
                            if page.page == "docs" && page.generation == lifetime {
                                let offset = f32::from(page.help.scroll.offset().y);
                                let changed = page.help.last_offset != Some(offset)
                                    || page.help.pending.is_some();
                                page.help.last_offset = Some(offset);
                                if let Some(target) = page.help.take_jump() {
                                    start_jump(page, target, cx);
                                }
                                if page.help.sync_layout() {
                                    cx.notify();
                                }
                                if changed {
                                    schedule_active(page, cx);
                                }
                            }
                        });
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
    for (ai, article) in page.help.articles.clone().into_iter().enumerate() {
        content = content.child(
            rsx! {
                <div flex_none min_w_0 p={px(14.)} rounded={px(9.)} border_1
                    border_color={color(&t.border_secondary)} bg={color(&t.surface)}
                    flex flex_col gap={px(9.)} />
            }
                .children(article.blocks.iter().enumerate().map(|(bi, block)| {
                    block_view(
                        page,
                        block,
                        &format!("help-block:{ai}:{bi}"),
                        content_width - 28.,
                        &t,
                        cx,
                    )
                })),
        );
    }
    rsx! {
        <div size_full flex min_h_0>
            {directory}
            {content}
        </div>
    }
        .into_any_element()
}

#[cfg(test)]
mod model_tests {
    use super::*;
    fn walk<'a>(blocks: &'a [Block], out: &mut Vec<&'a Block>) {
        for block in blocks {
            out.push(block);
            if let Block::Note { children, .. } = block {
                walk(children, out);
            }
        }
    }
    #[test]
    fn shipped_docs_keep_all_articles_heading_ids_tables_and_rows() {
        let docs = documents();
        let ids = toc(&docs)
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "doc-clip-usage",
                "clip-usage--全局快捷键",
                "clip-usage--窗口内快捷键",
                "clip-usage--粘贴",
                "clip-usage--扩展热键capslock-引导",
                "clip-usage--搜索",
                "clip-usage--列表导航与选择",
                "clip-usage--鼠标操作",
                "clip-usage--粘贴触发方式",
                "clip-usage--界面与常用功能",
                "clip-usage--超级菜单与贴顶晾衣绳",
                "clip-usage--外观主题",
                "doc-commands",
                "commands--常用命令一览",
                "doc-software-info",
                "doc-about",
                "about--支持作者"
            ]
        );
        let tables = docs[0]
            .blocks
            .iter()
            .filter_map(|block| {
                if let Block::Table { header, rows } = block {
                    Some((header, rows))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 11);
        // The native list-shortcut documentation adds the Ctrl+C / Ctrl+P
        // row; it must render as one two-cell row rather than disappearing
        // or splitting on the command separator.
        // The file-line keyboard section adds one table with nine shortcut rows.
        assert_eq!(tables.iter().map(|(_, rows)| rows.len()).sum::<usize>(), 66);
        assert_eq!(
            tables[0]
                .0
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<Vec<_>>(),
            ["快捷键", "作用", "说明"]
        );
        let win_v = tables[0].1.iter()
            .find(|row| row.first().is_some_and(|cell| cell.text == "Win+V"))
            .expect("the global shortcut table must preserve its Win+V row");
        assert_eq!(win_v.len(), 3);
        assert_eq!(win_v[1].text, "打开剪贴板");
        assert!(win_v[2].text.contains("使用 Win+V 打开剪贴板"));
        assert!(win_v[2].text.contains("重启资源管理器"));
        let list_shortcuts = tables[5].1;
        let select_all = list_shortcuts.iter()
            .find(|row| row.first().is_some_and(|cell| cell.text == "Ctrl + A"))
            .expect("history select-all scope must remain visible in the rendered help");
        assert_eq!(select_all.len(), 2);
        assert!(select_all[1].text.contains("已经加载的条目（含屏外行）"));
        assert!(select_all[1].text.contains("不主动加载后续分页"));
        assert!(select_all[1].text.contains("输入框内只全选文本"));
        let copy_pin = list_shortcuts.iter()
            .find(|row| row.first().is_some_and(|cell| cell.text == "Ctrl + C / Ctrl + P"))
            .expect("the native copy/pin shortcut row must not be dropped");
        assert_eq!(copy_pin.len(), 2);
        assert!(copy_pin[1].text.contains("复制选中项 / 切换置顶"));
        assert!(copy_pin[1].text.contains("普通列表焦点与无焦点完整接管均可用"));
        assert!(copy_pin[1].text.contains("搜索框不触发列表操作"));
        assert!(matches!(docs[1].blocks.last(), Some(Block::Commands)));
        assert!(matches!(docs[2].blocks.last(), Some(Block::Software)));
        assert!(matches!(docs[3].blocks.last(), Some(Block::Donate)));
    }
    #[test]
    fn shipped_contacts_are_clickable_without_rendering_imports_or_executing_mdx() {
        let docs = documents();
        let mut blocks = Vec::new();
        for article in &docs {
            walk(&article.blocks, &mut blocks);
        }
        let mut targets = Vec::new();
        let mut text = String::new();
        for block in blocks {
            match block {
                Block::Heading { title, .. }
                | Block::Paragraph(title)
                | Block::List { text: title, .. } => {
                    text.push_str(&title.text);
                    for span in &title.spans {
                        if let Some(href) = &span.href {
                            targets.push(href.clone());
                        }
                    }
                }
                Block::Code(code) => assert_eq!(code, "113342-5249"),
                _ => {}
            }
        }
        assert_eq!(targets,["https://www.wolai.com/rU43LqAXtZ8xDBNHxtKGni","https://getquicker.net/User/Actions/113342-Cea","https://getquicker.net/Common/Topics/New?objectType=SharedAction&objectId=9ec53d43-5539-4571-6886-08d8c752bfcb","mailto:1950946176@qq.com","https://qm.qq.com/q/krMDRGFtLO"]);
        assert!(text.contains("824753837"));
        assert!(text.contains("点击加入 QQ 群"));
        assert!(!text.contains("import "));
        assert!(!text.contains("<Note"));
        assert!(!text.contains("CLIPBOARD_QQ"));
    }
    #[test]
    fn table_escapes_and_inline_styles_keep_unicode_and_windows_paths() {
        let cells = table_cells(r"| `A\|B` | **中文😀** | `C:\a\[x].txt` |");
        assert_eq!(cells.len(), 3);
        let escaped = inline(&cells[0]);
        assert_eq!(escaped.text, "A|B"); // GFM consumes the table delimiter escape
        let prose = inline(r"A\|B **中文😀** [邮箱](mailto:a@example.com)");
        assert_eq!(prose.text, "A|B 中文😀 邮箱");
        assert!(prose
            .spans
            .iter()
            .all(|span| prose.text.is_char_boundary(span.range.start)
                && prose.text.is_char_boundary(span.range.end)));
        assert!(prose.spans.iter().any(|span| span.bold));
        assert_eq!(inline(&cells[2]).text, r"C:\a\[x].txt");
    }
    #[test]
    fn command_filter_matches_only_the_web_fields_and_keeps_original_examples() {
        let catalog = super::super::command_catalog().unwrap();
        assert_eq!(
            catalog
                .iter()
                .map(|item| item["command"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["paste", "clip", "favor", "collect", "setting", "menu", "panel", "winp"]
        );
        assert_eq!(
            catalog
                .iter()
                .flat_map(|item| item["exampleSections"].as_array().into_iter().flatten())
                .flat_map(|section| section["rows"].as_array().into_iter().flatten())
                .count(),
            88
        );
        let item = serde_json::json!({"command":"menu","label":"菜单","description":"打开菜单","hiddenKey":"must-not-match","exampleSections":[{"title":"文件","rows":[{"line":r"menu file -f C:\a.txt;C:\b.txt","hint":"打开文件菜单"}]}]});
        assert!(matches_command(&item, " MENU "));
        assert!(matches_command(&item, "打开文件"));
        assert!(matches_command(&item, "c:\\a"));
        assert!(matches_command(&item, "文件"));
        assert!(!matches_command(&item, "must-not-match"));
        assert!(!matches_command(&item, "exampleSections"));
        assert_eq!(
            item["exampleSections"][0]["rows"][0]["line"],
            r"menu file -f C:\a.txt;C:\b.txt"
        );
    }
    #[test]
    fn duplicate_heading_suffixes_and_encoded_fragment_use_stable_document_scope() {
        let mut used = BTreeSet::new();
        assert_eq!(
            slug("clip-usage", " **不可见？** ", &mut used),
            "clip-usage--不可见"
        );
        assert_eq!(
            slug("clip-usage", "不可见", &mut used),
            "clip-usage--不可见-2"
        );
        assert_eq!(
            decode_fragment("#clip-usage--%E7%B2%98%E8%B4%B4"),
            Some("clip-usage--粘贴".into())
        );
        assert_eq!(decode_fragment("#%broken"), None);
        assert_eq!(decode_fragment("#%FF"), None);
    }
    #[test]
    fn anchor_destination_and_directory_marker_use_the_scroll_viewport_not_scrolled_canvas_origin()
    {
        assert_eq!(eased_offset(-100., -428., 0.), -100.);
        assert_eq!(eased_offset(-100., -428., 1.), -428.);
        assert!(eased_offset(-100., -428., 0.5) > -428. && eased_offset(-100., -428., 0.5) < -100.);
        assert_eq!(target_offset(-100., 420., 80.), -428.);
        assert_eq!(target_offset(-100., 50., 80.), -58.);
        assert_eq!(target_offset(0., 20., 80.), 0.);
        let items = toc(&documents());
        let mut layout = Layout {
            top: 80.,
            ..Default::default()
        };
        layout.headings.insert(items[0].id.clone(), 20.);
        layout.headings.insert(items[1].id.clone(), 152.);
        layout.headings.insert(items[2].id.clone(), 153.);
        assert_eq!(active_id(&items, &layout), items[1].id);
        layout.headings.insert(items[2].id.clone(), 151.);
        assert_eq!(active_id(&items, &layout), items[2].id);
        assert_eq!(active_id(&[], &Layout::default()), "");
    }
}
