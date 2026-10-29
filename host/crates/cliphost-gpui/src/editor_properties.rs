//! Read-only editor metadata. The draft is only projected into display facts;
//! these controls never emit an editor change or access a backing file/store.

use std::{collections::HashMap, time::Instant};

use chrono::{DateTime, Local};
use gpui::{
    div, font, prelude::*, px, relative, Context, Entity, FocusHandle, FontWeight, KeyDownEvent,
    Render, ScrollHandle, SharedString, TextRun, Window,
};
use gpui_rsx::rsx;
use serde_json::Value;

use super::{style, DraftSnapshot};
use crate::{
    crisp_svg::svg,
    input::InputColors,
    motion::{self, Tweens},
    multiline::MultilineInput,
    preview::DesignTokens,
    smooth_wheel::SmoothWheelExt,
    view::color,
};
use cliphost_core::i18n::tr;

pub(super) fn kind_label(kind: &str) -> &'static str {
    match kind {
        "TextClipItem" => tr("纯文本", "Plain text"),
        "HtmlClipItem" => tr("HTML 富文本", "HTML rich text"),
        "QQRichTextItem" => tr("即时通讯图文", "IM rich content"),
        "ImageClipItem" => tr("图片", "Image"),
        "FileClipItem" => tr("文件", "Files"),
        "LinkClipItem" => tr("链接", "Link"),
        "CmdClipItem" => tr("命令", "Command"),
        "ActionClipItem" => tr("Quicker 动作", "Quicker action"),
        "StepClipItem" => tr("Quicker 步骤", "Quicker steps"),
        "FavoriteClipItem" => tr("收藏夹", "Folder"),
        _ => tr("其他条目", "Other item"),
    }
}

/// Paths use the same embedded UI-design asset namespace as DesignTokens.icons.
pub(super) fn kind_icon_asset(kind: &str) -> &'static str {
    match kind {
        "TextClipItem" => "text.svg",
        "HtmlClipItem" | "CmdClipItem" => "code.svg",
        "ImageClipItem" => "image.svg",
        "FileClipItem" => "file.svg",
        "LinkClipItem" => "link.svg",
        "ActionClipItem" => "play.svg",
        "StepClipItem" => "list.svg",
        "FavoriteClipItem" => "folder-closed.svg",
        _ => "clipboard.svg",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PropertyRow {
    key: &'static str,
    label: String,
    value: String,
    mono: bool,
}

impl PropertyRow {
    fn new(key: &'static str, label: &str, value: impl Into<String>) -> Self {
        Self {
            key,
            label: label.into(),
            value: value.into(),
            mono: false,
        }
    }
    fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PropertyGroup {
    key: &'static str,
    label: String,
    rows: Vec<PropertyRow>,
    disclosure: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PropertyPage {
    identity: (bool, String),
    kind: String,
    label: String,
    summary: String,
    groups: Vec<PropertyGroup>,
}

fn string(detail: &Value, key: &str) -> String {
    detail[key].as_str().unwrap_or_default().to_owned()
}

fn format_time(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|instant| {
            instant
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S (UTC%:z)")
                .to_string()
        })
        .unwrap_or_else(|_| raw.to_owned())
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    }
}

fn text_stats(text: &str) -> (usize, usize) {
    let characters = text.chars().count();
    if text.is_empty() {
        return (0, 0);
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    (characters, normalized.split('\n').count())
}

fn page(
    detail: &Value,
    favorite: bool,
    draft: &DraftSnapshot,
    image_size: Option<(u32, u32)>,
) -> PropertyPage {
    let kind = string(detail, "kind");
    let folder = favorite && (detail["isFolder"] == true || kind == "FavoriteClipItem");
    let display_kind = if folder { "FavoriteClipItem" } else { &kind };
    let mut info = vec![
        PropertyRow::new("kind", tr("类型", "Type"), kind_label(display_kind)),
        PropertyRow::new(
            "location",
            tr("位置", "Location"),
            if favorite {
                tr("收藏", "Favorites")
            } else {
                tr("剪贴板历史", "Clipboard history")
            },
        ),
    ];
    // Rich-source views can contain an uncommitted derived body; only plain text
    // displays draft text statistics, exactly as the confirmed browser preview.
    if kind == "TextClipItem" && !folder {
        let (characters, lines) = text_stats(&draft.body);
        info.push(PropertyRow::new(
            "textStats",
            tr("文本统计", "Text statistics"),
            format!(
                "{characters} {} · {lines} {}",
                tr("字符", "characters"),
                tr("行", "lines")
            ),
        ));
    }
    if kind == "FileClipItem" && !folder {
        let count = draft
            .file_paths
            .iter()
            .filter(|path| !path.trim().is_empty())
            .count();
        info.push(PropertyRow::new(
            "fileCount",
            tr("文件数量", "File count"),
            format!("{count} {}", tr("个", "files")),
        ));
    }
    if kind == "ImageClipItem" && !folder {
        if let Some((width, height)) =
            image_size.filter(|(width, height)| *width > 0 && *height > 0)
        {
            info.push(PropertyRow::new(
                "dimensions",
                tr("尺寸", "Dimensions"),
                format!("{width} × {height} px"),
            ));
        }
        // A detail DTO does not currently carry these fields. If a caller has
        // an already-known value, project it; never stat/read/decode a file here.
        if let Some(format) = detail["imageFormat"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            info.push(PropertyRow::new(
                "imageFormat",
                tr("格式", "Format"),
                format,
            ));
        }
        if let Some(bytes) = detail["imageSizeBytes"].as_u64() {
            info.push(PropertyRow::new(
                "imageBytes",
                tr("大小", "Size"),
                format_bytes(bytes),
            ));
        }
    }
    if folder {
        if let Some(children) = detail["children"].as_array() {
            info.push(PropertyRow::new(
                "children",
                tr("子项", "Children"),
                format!("{} {}", children.len(), tr("项", "items")),
            ));
        }
    }
    if !favorite {
        if let Some(top) = detail["isTop"].as_bool() {
            info.push(PropertyRow::new(
                "isTop",
                tr("置顶", "Pinned"),
                if top {
                    tr("是", "Yes")
                } else {
                    tr("否", "No")
                },
            ));
        }
    }
    if let Some(name) = detail["actionTitle"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
    {
        info.push(PropertyRow::new("actionTitle", tr("动作", "Action"), name));
    }
    if let Some(origin) = detail["imRichOrigin"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
    {
        let label = match origin {
            "qq" => "QQ",
            "wechat" => tr("微信", "WeChat"),
            "wework" => tr("企业微信", "WeCom"),
            _ => origin,
        };
        info.push(PropertyRow::new(
            "imOrigin",
            tr("图文格式", "Rich format"),
            label,
        ));
    }
    let mut source = Vec::new();
    if !favorite || detail.get("sourceProcessName").is_some() {
        source.push(PropertyRow::new(
            "sourceProcess",
            tr("来源应用", "Source application"),
            string(detail, "sourceProcessName"),
        ));
    }
    source.push(PropertyRow::new(
        "created",
        tr("创建时间", "Created"),
        format_time(&string(detail, "createTime")),
    ));
    if let Some(updated) = detail["updateTime"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
    {
        source.push(PropertyRow::new(
            "updated",
            tr("更新时间", "Updated"),
            format_time(updated),
        ));
    }
    // Explicit text conversion clears HTML and its source URL. A tab switch or
    // title-only edit does not, so it continues to show the original URL.
    let original_html = string(detail, "htmlData");
    let text_conversion = kind == "HtmlClipItem"
        && !original_html.is_empty()
        && (draft.html.is_empty()
            || (draft.html == original_html && draft.body != string(detail, "bodyText")));
    if !text_conversion {
        if let Some(url) = detail["htmlSourceUrl"]
            .as_str()
            .filter(|url| !url.trim().is_empty())
        {
            source.push(PropertyRow::new(
                "sourceUrl",
                tr("来源页面", "Source page"),
                url,
            ));
        }
    }
    let mut identifiers = vec![
        PropertyRow::new("id", tr("条目 ID", "Item ID"), string(detail, "id")).mono(),
        PropertyRow::new("rawKind", tr("原始类型", "Raw type"), kind.clone()).mono(),
    ];
    for (key, label) in [
        ("parentId", tr("父级 ID", "Parent ID")),
        ("actionId", tr("动作 ID", "Action ID")),
    ] {
        if let Some(value) = detail[key]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            identifiers.push(PropertyRow::new(key, label, value).mono());
        }
    }
    PropertyPage {
        identity: (favorite, string(detail, "id")),
        kind: display_kind.into(),
        label: kind_label(display_kind).into(),
        summary: if draft.title.trim().is_empty() {
            tr("未命名条目", "Untitled item").into()
        } else {
            draft.title.clone()
        },
        groups: vec![
            PropertyGroup {
                key: "item",
                label: tr("条目信息", "Item information").into(),
                rows: info,
                disclosure: false,
            },
            PropertyGroup {
                key: "source",
                label: tr("来源与时间", "Source and time").into(),
                rows: source,
                disclosure: false,
            },
            PropertyGroup {
                key: "identifiers",
                label: tr("标识", "Identifiers").into(),
                rows: identifiers,
                disclosure: true,
            },
        ],
    }
}

struct ReadOnlyValue {
    input: Entity<MultilineInput>,
    text: String,
    height: f32,
    // Width changes need a real text measurement, even for an unchanged value.
    metrics: Option<(f32, f32)>,
}

pub(super) struct EditorProperties {
    tokens: DesignTokens,
    dark: bool,
    page: PropertyPage,
    values: HashMap<&'static str, ReadOnlyValue>,
    scroll: ScrollHandle,
    identifiers_open: bool,
    identifiers_hovered: bool,
    disclosure_focus: FocusHandle,
    motion: Tweens<&'static str>,
}

impl EditorProperties {
    pub(super) fn new(tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) -> Self {
        Self {
            tokens,
            dark,
            page: PropertyPage::default(),
            values: HashMap::new(),
            scroll: ScrollHandle::new(),
            identifiers_open: false,
            identifiers_hovered: false,
            disclosure_focus: cx.focus_handle(),
            motion: Tweens::default(),
        }
    }

    pub(super) fn sync(
        &mut self,
        detail: Option<&Value>,
        favorite: bool,
        draft: &DraftSnapshot,
        image_size: Option<(u32, u32)>,
        tokens: &DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = detail
            .map(|detail| page(detail, favorite, draft, image_size))
            .unwrap_or_default();
        let theme_changed = self.dark != dark
            || self.tokens.theme(self.dark) != tokens.theme(dark)
            || self.tokens.typography.font_family != tokens.typography.font_family
            || self.tokens.typography.small_font_size != tokens.typography.small_font_size;
        if next == self.page && !theme_changed {
            return;
        }
        if next.identity != self.page.identity {
            self.identifiers_open = false;
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
            self.values.clear();
        }
        self.tokens = tokens.clone();
        self.dark = dark;
        let theme = tokens.theme(dark);
        let colors = InputColors {
            background: color(&theme.surface),
            foreground: color(&theme.text),
            placeholder: color(&theme.text_muted),
            border: color(&theme.border),
            selection: color(&theme.selected),
            caret: gpui::transparent_black(),
        };
        for row in next.groups.iter().flat_map(|group| &group.rows) {
            let display = if row.value.trim().is_empty() {
                tr("未提供", "Not provided").to_owned()
            } else {
                row.value.clone()
            };
            let entry = self.values.entry(row.key).or_insert_with(|| ReadOnlyValue {
                input: cx.new(|cx| {
                    let mut input = MultilineInput::new(window, cx);
                    input.set_read_only(true, cx);
                    input
                }),
                text: String::new(),
                height: 0.,
                metrics: None,
            });
            let value_colors = InputColors {
                foreground: if row.value.trim().is_empty() {
                    color(&theme.text_muted)
                } else {
                    colors.foreground
                },
                ..colors
            };
            entry.input.update(cx, |input, cx| {
                if entry.text != display {
                    input.set_text(display.clone(), window, cx);
                }
                if theme_changed || entry.text != display || entry.metrics.is_none() {
                    input.set_colors(value_colors, cx);
                }
            });
            if entry.text != display || theme_changed {
                entry.metrics = None;
            }
            entry.text = display;
        }
        self.page = next;
        cx.notify();
    }

    fn toggle_identifiers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.identifiers_open = !self.identifiers_open;
        self.disclosure_focus.focus(window);
        cx.notify();
    }

    fn rows(
        &mut self,
        group: &PropertyGroup,
        content_width: f32,
        stacked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = self.tokens.theme(self.dark);
        let padding_x = style::metric("propRowPaddingX", 12.);
        let padding_y = style::metric("propRowPaddingY", 10.);
        let label_width = style::metric("propLabelWidth", 72.);
        let gap = style::metric("propRowGap", 12.);
        let font_size = self.tokens.typography.small_font_size;
        let value_width =
            (content_width - 2. - 2. * padding_x - if stacked { 0. } else { label_width + gap })
                .max(1.);
        let mut rows = rsx! {
            <div w_full min_w={px(0.)} flex_none flex flex_col />
        };
        for (index, row) in group.rows.iter().enumerate() {
            let Some(value) = self.values.get_mut(row.key) else {
                continue;
            };
            let family = if row.mono && !row.value.trim().is_empty() {
                "Consolas".to_string()
            } else {
                self.tokens.typography.font_family.clone()
            };
            if value.metrics != Some((value_width, font_size)) {
                let height = measured_height(&value.text, value_width, font_size, &family, window);
                value.input.update(cx, |input, cx| {
                    input.set_metrics(height + 12., font_size, 0., cx)
                });
                value.height = height + 12.;
                value.metrics = Some((value_width, font_size));
            }
            let label_height = measured_height(
                &row.label,
                if stacked { value_width } else { label_width },
                font_size,
                &self.tokens.typography.font_family,
                window,
            );
            let row_gap = if stacked {
                style::metric("propTextGap", 4.)
            } else {
                gap
            };
            let row_height = padding_y * 2.
                + if stacked {
                    label_height + row_gap + value.height - 12.
                } else {
                    label_height.max(value.height - 12.)
                }
                + if index > 0 { 1. } else { 0. };
            let label = rsx! {
                <div flex_none min_h={px(label_height)} text_size={px(font_size)}
                    line_height={relative(self.tokens.typography.line_height)}
                    text_color={color(&theme.text_muted)} />
            }
                .when(!stacked, |label| label.w(px(label_width)))
                .child(row.label.clone());
            let readonly = div()
                // In the stacked row the main flex axis is vertical. Growing
                // there gives the percentage-width input no horizontal size;
                // let its measured height size the row and fill the cross axis.
                .when(stacked, |value| value.flex_none().w_full())
                .when(!stacked, |value| value.flex_1())
                .h(px(value.height))
                .min_w(px(0.))
                .my(px(-6.))
                .font_family(family)
                .child(value.input.clone());
            rows = rows.child(
                rsx! {
                    <div flex flex_none h={px(row_height)} min_h={px(row_height)} />
                }
                    .when(stacked, |row| row.flex_col())
                    .items_start()
                    .w_full()
                    .min_w(px(0.))
                    .px(px(padding_x))
                    .py(px(padding_y))
                    .gap(px(row_gap))
                    .when(index > 0, |row| {
                        row.border_t_1()
                            .border_color(color(&theme.border_secondary))
                    })
                    .child(label)
                    .child(readonly),
            );
        }
        rows.into_any_element()
    }
}

/// Match MultilineInput's real line shaping rather than truncating an ID/URL or
/// guessing a character width. Its six-pixel padding is cancelled by the row.
fn measured_height(
    text: &str,
    width: f32,
    font_size: f32,
    family: &str,
    window: &mut Window,
) -> f32 {
    let mut height = 0.;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let value: SharedString = raw.to_owned().into();
        let run = TextRun {
            len: value.len(),
            font: font(family.to_owned()),
            color: gpui::rgb(0).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let measured = window
            .text_system()
            .shape_text(value, px(font_size), &[run], Some(px(width)), None)
            .ok()
            .and_then(|lines| lines.into_iter().next())
            .map(|line| f32::from(line.size(px(font_size + 6.)).height))
            .unwrap_or(font_size + 6.);
        height += measured.max(font_size + 6.);
    }
    height
}

impl Render for EditorProperties {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.tokens.theme(self.dark).clone();
        let padding_x = style::metric("propPaddingX", 16.);
        let content_width = (f32::from(window.viewport_size().width) - padding_x * 2. - 2.).max(1.);
        // Browser container queries use the page's content box (after padding).
        let stacked = content_width <= 340.;
        let now = Instant::now();
        let hover = self.motion.value(
            "identifiers-hover",
            if self.identifiers_hovered { 1. } else { 0. },
            motion::HOVER,
            motion::EASE,
            now,
        );
        let focus = self.motion.value(
            "identifiers-focus",
            if self.disclosure_focus.is_focused(window) {
                1.
            } else {
                0.
            },
            motion::FOCUS,
            motion::EASE,
            now,
        );
        if self.motion.any_active(now) {
            window.request_animation_frame();
        }
        let mut page = rsx! {
            <div flex flex_col w_full min_w={px(0.)} gap={px(style::metric("propGap", 14.))}>
                <div flex items_start min_w={px(0.)}
                    gap={px(style::metric("propOverviewGap", 12.))}
                    pb={px(style::metric("propOverviewGap", 12.))}>
                    <div flex_none size={px(style::metric("propBadgeSize", 36.))}
                        flex items_center justify_center border_1 border_color={color(&theme.border)}
                        rounded={px(style::metric("propCardRadius", 8.))} bg={color(&theme.secondary)}>
                        <svg path={kind_icon_asset(&self.page.kind)}
                            size={px(self.tokens.layout.icon_size)}
                            text_color={color(&theme.text_secondary)} />
                    </div>
                    <div flex_1 min_w={px(0.)} flex flex_col
                        gap={px(style::metric("propTextGap", 4.))}>
                        <div font_weight={FontWeight::SEMIBOLD}
                            text_size={px(self.tokens.typography.font_size)}>
                            {self.page.label.clone()}
                        </div>
                        <div text_size={px(self.tokens.typography.small_font_size)}
                            text_color={color(&theme.text_muted)}>
                            {self.page.summary.clone()}
                        </div>
                    </div>
                </div>
            </div>
        };
        for group in self.page.groups.clone() {
            let mut card = rsx! {
                <div flex_none w_full min_w={px(0.)} overflow_hidden
                    rounded={px(style::metric("propCardRadius", 8.))} border_1
                    border_color={color(&theme.border)} bg={color(&theme.surface)} />
            };
            let heading = rsx! {
                <div text_size={px(self.tokens.typography.small_font_size)}
                    font_weight={FontWeight::SEMIBOLD} text_color={color(&theme.text_secondary)}
                    px={px(style::metric("propRowPaddingX", 12.))}
                    py={px(style::metric("propRowPaddingY", 10.))} />
            };
            if group.disclosure {
                card = card.child(
                    heading
                        .id("editor-properties-identifiers")
                        .track_focus(&self.disclosure_focus)
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .justify_between()
                        .bg(motion::mix_hsla(
                            color(&theme.surface),
                            color(&theme.hover),
                            hover.max(focus),
                        ))
                        .when(self.identifiers_open, |row| {
                            row.border_b_1().border_color(color(&theme.border))
                        })
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            this.identifiers_hovered = *hovered;
                            cx.notify();
                        }))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_identifiers(window, cx)),
                        )
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                                && !event.is_held
                            {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.toggle_identifiers(window, cx);
                            }
                        }))
                        .child(group.label.clone())
                        .child(
                            svg()
                                .path(if self.identifiers_open {
                                    "collapse.svg"
                                } else {
                                    "chevron-right.svg"
                                })
                                .size(px(self.tokens.layout.icon_size))
                                .text_color(color(&theme.text_muted)),
                        ),
                );
            } else {
                card = card.child(
                    heading
                        .border_b_1()
                        .border_color(color(&theme.border))
                        .child(group.label.clone()),
                );
            }
            if !group.disclosure || self.identifiers_open {
                card = card.child(self.rows(&group, content_width, stacked, window, cx));
            }
            page = page.child(card);
        }
        div()
            .id("editor-properties-scroll")
            .size_full()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .smooth_wheel("editor-properties-scroll", &self.scroll, cx)
            .px(px(padding_x))
            .py(px(style::metric("propPaddingY", 16.)))
            .text_color(color(&theme.text))
            .font_family(self.tokens.typography.font_family.clone())
            .line_height(relative(self.tokens.typography.line_height))
            .child(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn value<'a>(page: &'a PropertyPage, key: &str) -> Option<&'a str> {
        page.groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.key == key)
            .map(|row| row.value.as_str())
    }

    #[test]
    fn text_statistics_count_scalars_and_all_newline_spellings() {
        assert_eq!(text_stats("A😀\r\n中\rB\n"), (8, 4));
        assert_eq!(text_stats(""), (0, 0));
    }

    #[test]
    fn draft_file_count_and_folder_relationships_are_read_only_projections() {
        let detail = json!({"id":"x","kind":"FileClipItem","filePaths":["old"]});
        let draft = DraftSnapshot {
            file_paths: vec!["one".into(), "   ".into(), "two".into()],
            ..Default::default()
        };
        let projection = page(&detail, false, &draft, None);
        assert!(value(&projection, "fileCount").unwrap().starts_with("2 "));
        assert_eq!(detail["filePaths"], json!(["old"]));
        let folder = page(
            &json!({"id":"f","kind":"FavoriteClipItem","isFolder":true,"children":["a","b"],"parentId":"parent"}),
            true,
            &draft,
            None,
        );
        assert!(value(&folder, "children").unwrap().starts_with("2 "));
        assert_eq!(value(&folder, "parentId"), Some("parent"));
        assert!(folder.groups.last().unwrap().disclosure);
    }

    #[test]
    fn unknown_kind_and_long_identifiers_preserve_the_complete_value() {
        let id = "长id/😀".repeat(400);
        let detail = json!({"id":id,"kind":"VendorCustomClipItem","sourceProcessName":""});
        let projection = page(&detail, false, &DraftSnapshot::default(), None);
        assert_eq!(value(&projection, "id"), detail["id"].as_str());
        assert_eq!(value(&projection, "rawKind"), Some("VendorCustomClipItem"));
        assert_eq!(value(&projection, "sourceProcess"), Some(""));
    }

    #[test]
    fn image_metadata_uses_only_supplied_dimensions_and_known_size() {
        let detail = json!({"id":"i","kind":"ImageClipItem","rawJson":"{\"ImagePath\":\"C:\\\\private.png\"}"});
        let projection = page(
            &detail,
            false,
            &DraftSnapshot::default(),
            Some((1920, 1080)),
        );
        assert_eq!(value(&projection, "dimensions"), Some("1920 × 1080 px"));
        assert_eq!(value(&projection, "imageBytes"), None);
        assert_eq!(value(&projection, "imageFormat"), None);
        assert_eq!(format_bytes(1024), "1.0 KB");
    }

    #[test]
    fn source_url_survives_title_edits_and_disappears_after_text_conversion() {
        let detail = json!({"id":"html","kind":"HtmlClipItem","htmlData":"<b>old</b>","bodyText":"old","htmlSourceUrl":"https://example.com/a"});
        let mut draft = DraftSnapshot::from_detail(&detail, false);
        draft.title = "new title".into();
        assert_eq!(
            value(&page(&detail, false, &draft, None), "sourceUrl"),
            Some("https://example.com/a")
        );
        draft.body = "plain text edit".into();
        assert_eq!(
            value(&page(&detail, false, &draft, None), "sourceUrl"),
            None
        );
        draft.html.clear();
        assert_eq!(
            value(&page(&detail, false, &draft, None), "sourceUrl"),
            None
        );
    }

    #[test]
    fn malformed_time_is_preserved_instead_of_fabricated() {
        assert_eq!(format_time("vendor timestamp"), "vendor timestamp");
        assert_eq!(format_time(""), "");
    }
}
