//! Inert HTML display shared by QP and copy-card previews. Payload stays in core.
use gpui_rsx::rsx;
use super::html::{
    self, HtmlBlock, HtmlColor, HtmlImage, HtmlImageSource, HtmlInline, HtmlListMarker,
    HtmlParagraph, HtmlPreview,
};
use crate::native_content::{ContentResources, CONTENT_RESOURCE_BUDGET_BYTES};
use crate::preview::DesignTokens;
use crate::view::color;
use cliphost_core::{host::HostState, i18n::tr};
use gpui::{
    div, img, prelude::*, px, rgba, AnyElement, FontStyle, FontWeight, HighlightStyle, Image,
    StrikethroughStyle, StyledText, UnderlineStyle,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub struct LoadedHtml {
    pub projection: HtmlPreview,
    images: BTreeMap<String, Arc<Image>>,
    image_sizes: BTreeMap<String, (u32, u32)>,
    _image_holder: crate::native_content::ContentImageHolderLease,
}

fn source_key(source: &HtmlImageSource) -> Option<String> {
    match source {
        HtmlImageSource::LocalPath(path) => Some(path.clone()),
        HtmlImageSource::EmbeddedData { uri, .. } => Some(uri.clone()),
        _ => None,
    }
}

pub fn load_html_preview(host: &HostState, value: &str) -> LoadedHtml {
    let projection = html::parse_html_preview(value);
    let mut loaded = LoadedHtml {
        projection,
        images: BTreeMap::new(),
        image_sizes: BTreeMap::new(),
        _image_holder: crate::native_content::ContentImageHolderLease::new(Vec::new()),
    };
    let mut sources = Vec::new();
    collect_block_images(&loaded.projection.blocks, &mut sources);
    let resources = load_html_image_resources(sources, |source| match source {
        // Retain the existing host authorization; no direct path reads here.
        HtmlImageSource::LocalPath(path) => host.local_image_bytes(path),
        HtmlImageSource::EmbeddedData { .. } => html::decode_embedded_image(source),
        _ => None,
    });
    for (key, image) in resources.images {
        if let Some(image) = image {
            loaded
                .image_sizes
                .insert(key.clone(), (image.width as u32, image.height as u32));
            loaded.images.insert(key, image.image);
        }
    }
    loaded._image_holder = crate::native_content::ContentImageHolderLease::new(
        loaded.images.values().cloned().collect(),
    );
    loaded
}

// Each resource is decoded/admitted before the next source is read. Duplicate
// failures also count as attempted, so repeated missing/oversized references do
// not repeat I/O. This budget bounds retained resources, not the existing Core
// local_image_bytes call's temporary whole-file read.
fn load_html_image_resources(
    sources: Vec<HtmlImageSource>,
    mut read: impl FnMut(&HtmlImageSource) -> Option<(Vec<u8>, String)>,
) -> ContentResources {
    let mut resources = ContentResources::default();
    let mut seen = BTreeSet::new();
    for source in sources {
        let Some(key) = source_key(&source) else {
            continue;
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        let Some((bytes, mime)) = read(&source) else {
            continue;
        };
        // Full preview keeps original raster bytes when they fit, otherwise the
        // shared decoder uses its detail-sized fallback, never the list's 56px
        // thumbnail. Unsupported SVG stays an unavailable image placeholder.
        let Some(image) = super::raster_from_bytes(bytes, &mime, false) else {
            continue;
        };
        let remaining = CONTENT_RESOURCE_BUDGET_BYTES.saturating_sub(resources.retained_bytes());
        let mut candidate = ContentResources::default();
        candidate.images.insert(key.clone(), Some(image));
        candidate.limit_retained_bytes(remaining);
        if let Some(image) = candidate.images.remove(&key).flatten() {
            resources.images.insert(key, Some(image));
        }
    }
    resources
}

fn collect_images(inlines: &[HtmlInline], sources: &mut Vec<HtmlImageSource>) {
    for inline in inlines {
        if let HtmlInline::Image(image) = inline {
            sources.push(image.source.clone());
        }
    }
}

fn collect_block_images(blocks: &[HtmlBlock], sources: &mut Vec<HtmlImageSource>) {
    for block in blocks {
        match block {
            HtmlBlock::Paragraph(paragraph) | HtmlBlock::Preformatted(paragraph) => {
                collect_images(&paragraph.inlines, sources)
            }
            HtmlBlock::Table(table) => {
                for row in &table.rows {
                    for cell in row {
                        collect_block_images(&cell.blocks, sources);
                    }
                }
            }
            HtmlBlock::Rule => {}
        }
    }
}

fn html_color(value: HtmlColor) -> gpui::Hsla {
    rgba(
        (value.red as u32) << 24
            | (value.green as u32) << 16
            | (value.blue as u32) << 8
            | value.alpha as u32,
    )
    .into()
}

fn inline_highlight(style: &html::HtmlTextStyle, compact: bool, dark: bool) -> HighlightStyle {
    // Dark detail inherits the readable theme foreground/background, as the
    // Web QP text preview does. Source formatting remains in the projection.
    let source_colors = !compact && !dark;
    HighlightStyle {
        color: source_colors.then(|| style.color.map(html_color)).flatten(),
        background_color: source_colors
            .then(|| style.background.map(html_color))
            .flatten(),
        font_weight: Some(FontWeight(style.font_weight as f32)),
        font_style: Some(if style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        }),
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.0),
            ..Default::default()
        }),
        strikethrough: style.strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.0),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn inline_text(inlines: &[HtmlInline], compact: bool, dark: bool) -> AnyElement {
    let mut text = String::new();
    let mut highlights = Vec::new();
    for inline in inlines {
        match inline {
            HtmlInline::Text(run) => {
                let start = text.len();
                text.push_str(&run.text);
                highlights.push((
                    start..text.len(),
                    inline_highlight(&run.style, compact, dark),
                ));
            }
            HtmlInline::LineBreak => text.push('\n'),
            HtmlInline::Image(_) => {}
        }
    }
    StyledText::new(text)
        .with_highlights(highlights)
        .into_any_element()
}

// Copy cards are excerpts, so source whitespace must not reserve empty rows.
// Keep run boundaries and emphasis while collapsing repeated blank lines/spaces.
fn compact_inlines(inlines: &[HtmlInline]) -> Vec<HtmlInline> {
    let mut result = Vec::new();
    let mut separator = None;
    let mut has_text = false;
    for inline in inlines {
        match inline {
            HtmlInline::Text(run) => {
                let mut text = String::new();
                for ch in run.text.chars() {
                    if ch.is_whitespace() {
                        separator.get_or_insert(' ');
                    } else {
                        if has_text {
                            if let Some(ch) = separator.take() {
                                text.push(ch);
                            }
                        } else {
                            separator = None;
                        }
                        text.push(ch);
                        has_text = true;
                    }
                }
                if !text.is_empty() {
                    let mut run = run.clone();
                    run.text = text;
                    result.push(HtmlInline::Text(run));
                }
            }
            HtmlInline::LineBreak => {
                if has_text {
                    separator = Some('\n');
                }
            }
            HtmlInline::Image(_) => {
                result.push(inline.clone());
                separator = None;
                has_text = false;
            }
        }
    }
    result
}

fn compact_image_size(
    image: &HtmlImage,
    intrinsic: Option<(u32, u32)>,
    width: f32,
    height: f32,
) -> (f32, f32) {
    let valid = |value: f32| value.is_finite() && value > 0.0;
    let declared = image.width_px.zip(image.height_px);
    let (source_width, source_height) = intrinsic
        .map(|(width, height)| (width as f32, height as f32))
        .or(declared)
        .filter(|(width, height)| valid(*width) && valid(*height))
        .unwrap_or((64.0, 64.0));
    let scale = (width.max(1.0) / source_width)
        .min(height.max(1.0) / source_height)
        .min(1.0);
    (source_width * scale, source_height * scale)
}

fn paragraph_content(
    loaded: &LoadedHtml,
    paragraph: &HtmlParagraph,
    width: f32,
    height: f32,
    compact: bool,
    dark: bool,
) -> AnyElement {
    let mut content = div()
        .flex()
        .flex_col()
        .gap(px(if compact { 1.0 } else { 3.0 }));
    // Keep a contiguous inline text span as one shaped element so whitespace,
    // CJK/Latin wrapping and mixed emphasis retain their reading order.
    let mut pending = Vec::new();
    let compact_inlines = compact.then(|| compact_inlines(&paragraph.inlines));
    let inlines = compact_inlines.as_deref().unwrap_or(&paragraph.inlines);
    for inline in inlines {
        match inline {
            HtmlInline::Image(image) => {
                if !pending.is_empty() {
                    content = content.child(inline_text(&pending, compact, dark));
                    pending.clear();
                }
                let bitmap = source_key(&image.source)
                    .and_then(|key| loaded.images.get(&key))
                    .cloned();
                content = if let Some(bitmap) = bitmap {
                    let size = compact_image_size(
                        image,
                        source_key(&image.source)
                            .and_then(|key| loaded.image_sizes.get(&key))
                            .copied(),
                        width,
                        height,
                    );
                    content.child(crate::native_content::render_resource_image(
                        bitmap.clone(),
                        img(bitmap)
                            .when(compact, |image| image.w(px(size.0)).h(px(size.1)))
                            .max_w(px(width.max(1.0)))
                            .max_h(px(height.max(1.0)))
                            .object_fit(gpui::ObjectFit::Contain)
                            .with_fallback(|| {
                                div()
                                    .child(tr("[图片加载失败]", "[Image failed to load]"))
                                    .into_any_element()
                            })
                            .into_any_element(),
                    ))
                } else {
                    content.child(if image.alt.is_empty() {
                        tr("[图片不可用]", "[Image unavailable]").to_string()
                    } else {
                        format!("[{}]", image.alt)
                    })
                };
            }
            _ => pending.push(inline.clone()),
        }
    }
    if !pending.is_empty() {
        content = content.child(inline_text(&pending, compact, dark));
    }
    if paragraph.preserve_whitespace && !compact {
        content = content.font_family("Consolas");
    }
    let first = paragraph.inlines.iter().find_map(|inline| match inline {
        HtmlInline::Text(run) => Some(&run.style),
        _ => None,
    });
    if let Some(style) = first.filter(|_| !compact) {
        if let Some(size) = style.font_size_px {
            content = content.text_size(px(size.clamp(8.0, 48.0)));
        }
        if let Some(family) = &style.font_family {
            content = content.font_family(family.clone());
        }
    }
    content = match (compact, paragraph.alignment) {
        (false, html::HtmlAlignment::Center) => content.text_center(),
        (false, html::HtmlAlignment::Right) => content.text_right(),
        _ => content.text_left(),
    };
    let marker = match &paragraph.marker {
        Some(HtmlListMarker::Unordered) => Some("•".to_string()),
        Some(HtmlListMarker::Ordered(index)) => Some(format!("{index}.")),
        _ => None,
    };
    div()
        .flex()
        .gap(px(if compact { 4.0 } else { 6.0 }))
        .pl(px(if compact {
            (paragraph.indent.min(2) * 6 + paragraph.quote_depth.min(2) * 4) as f32
        } else {
            (paragraph.indent.min(8) * 14 + paragraph.quote_depth.min(8) * 8) as f32
        }))
        .when(compact, |row| row.flex_shrink_0())
        .when_some(marker, |row, marker| {
            row.child(div().flex_shrink_0().child(marker))
        })
        .child(content.flex_1().min_w_0())
        .into_any_element()
}

pub fn render_html_preview(
    loaded: &LoadedHtml,
    width: f32,
    height: f32,
    tokens: &DesignTokens,
    dark: bool,
) -> AnyElement {
    let content = render_blocks(
        loaded,
        &loaded.projection.blocks,
        width,
        height,
        tokens,
        dark,
        false,
    );
    let theme = tokens.theme(dark);
    let muted = color(&theme.text_muted);
    rsx! {
        <div flex flex_col gap={px(6.0)}>
            {content}
        </div>
    }
        .when(loaded.projection.truncated, |content| {
            content.child(rsx! {
                <div text_size={px(11.0)} text_color={muted}>
                    {tr(
                "预览内容过长，已截取显示；原内容完整保留。",
                "Preview truncated; the original content is preserved.",
            )}
                </div>
            })
        })
        .into_any_element()
}

/// Theme-readable, bounded excerpts for copy notifications only. The full HTML
/// renderer and the original clipboard payload keep their source formatting.
pub fn render_compact_html_preview(
    loaded: &LoadedHtml,
    width: f32,
    height: f32,
    tokens: &DesignTokens,
    dark: bool,
) -> AnyElement {
    let theme = tokens.theme(dark);
    let text = color(&theme.text);
    rsx! {
        <div w={px(width.max(1.0))} max_h={px(height.max(1.0))} overflow_hidden
            font_family={tokens.typography.font_family.clone()} text_size={px(13.0)}
            line_height={px(18.0)} text_color={text}>
            {render_blocks(
            loaded,
            &loaded.projection.blocks,
            width,
            height,
            tokens,
            dark,
            true,
        )}
        </div>
    }
        .into_any_element()
}

fn compact_block_visible(block: &HtmlBlock) -> bool {
    match block {
        HtmlBlock::Paragraph(paragraph) | HtmlBlock::Preformatted(paragraph) => {
            paragraph.inlines.iter().any(|inline| match inline {
                HtmlInline::Text(run) => !run.text.trim().is_empty(),
                HtmlInline::Image(_) => true,
                HtmlInline::LineBreak => false,
            })
        }
        HtmlBlock::Table(table) => table
            .rows
            .iter()
            .flatten()
            .any(|cell| cell.blocks.iter().any(compact_block_visible)),
        HtmlBlock::Rule => false,
    }
}

fn render_blocks(
    loaded: &LoadedHtml,
    blocks: &[HtmlBlock],
    width: f32,
    height: f32,
    tokens: &DesignTokens,
    dark: bool,
    compact: bool,
) -> AnyElement {
    let theme = tokens.theme(dark);
    let mut content = div()
        .flex()
        .flex_col()
        .gap(px(if compact { 2.0 } else { 7.0 }))
        .w_full()
        .min_w_0();
    for block in blocks
        .iter()
        .filter(|block| !compact || compact_block_visible(block))
        .take(if compact { 4 } else { usize::MAX })
    {
        match block {
            HtmlBlock::Paragraph(paragraph) | HtmlBlock::Preformatted(paragraph) => {
                content = content.child(paragraph_content(
                    loaded, paragraph, width, height, compact, dark,
                ))
            }
            HtmlBlock::Rule => {
                content = content.child(div().h(px(1.0)).w_full().bg(color(&theme.border)))
            }
            HtmlBlock::Table(table) => {
                let columns = table.rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                let columns = if compact { columns.min(3) } else { columns };
                let mut grid = rsx! {
                    <div flex flex_col border_1 border_color={color(&theme.border)} />
                };
                for row in table.rows.iter().take(if compact { 2 } else { usize::MAX }) {
                    let mut cells = div().flex().w_full();
                    for cell in row.iter().take(columns) {
                        cells = cells.child(
                            div()
                                .w(px((width / columns as f32).max(1.0)))
                                .min_w_0()
                                .when(compact, |cell| cell.p(px(2.0)))
                                .when(!compact, |cell| cell.p_1())
                                .border_r_1()
                                .border_b_1()
                                .border_color(color(&theme.border))
                                .when(cell.header, |cell| cell.font_weight(FontWeight::BOLD))
                                .child(render_blocks(
                                    loaded,
                                    &cell.blocks,
                                    width / columns as f32 - if compact { 4.0 } else { 8.0 },
                                    height,
                                    tokens,
                                    dark,
                                    compact,
                                )),
                        );
                    }
                    grid = grid.child(cells);
                }
                content = content.child(grid);
            }
        }
    }
    content.into_any_element()
}

#[cfg(test)]
mod html_style_tests {
    use super::*;

    fn source_style() -> html::HtmlTextStyle {
        let projection = html::parse_html_preview(
            "<p><strong><em><u><s><span style='color:#000;background-color:#fff'>readable</span></s></u></em></strong></p>",
        );
        projection
            .blocks
            .iter()
            .find_map(|block| match block {
                HtmlBlock::Paragraph(paragraph) => paragraph.inlines.iter().find_map(|inline| {
                    if let HtmlInline::Text(run) = inline {
                        Some(run.style.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .unwrap()
    }

    fn assert_emphasis(highlight: &HighlightStyle) {
        assert_eq!(highlight.font_weight, Some(FontWeight::BOLD));
        assert_eq!(highlight.font_style, Some(FontStyle::Italic));
        assert_eq!(highlight.underline.unwrap().thickness, px(1.0));
        assert_eq!(highlight.strikethrough.unwrap().thickness, px(1.0));
    }

    #[test]
    fn dark_detail_inherits_theme_colors_and_keeps_source_emphasis_unchanged() {
        let style = source_style();
        let original = style.clone();
        assert_eq!(style.color, html::parse_html_color("#000"));
        assert_eq!(style.background, html::parse_html_color("#fff"));
        let highlight = inline_highlight(&style, false, true);
        assert!(highlight.color.is_none());
        assert!(highlight.background_color.is_none());
        assert_emphasis(&highlight);
        assert_eq!(style, original);
    }

    #[test]
    fn light_detail_preserves_source_colors_and_compact_still_inherits_both_themes() {
        let style = source_style();
        let highlight = inline_highlight(&style, false, false);
        assert_eq!(highlight.color, style.color.map(html_color));
        assert_eq!(highlight.background_color, style.background.map(html_color));
        assert_emphasis(&highlight);
        for dark in [false, true] {
            let highlight = inline_highlight(&style, true, dark);
            assert!(highlight.color.is_none());
            assert!(highlight.background_color.is_none());
            assert_emphasis(&highlight);
        }
    }
}

#[cfg(test)]
mod html_resource_tests {
    use super::*;
    use std::io::Cursor;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let pixels = image::RgbaImage::from_pixel(width, height, image::Rgba([20, 60, 120, 255]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn detail_keeps_full_small_raster_bytes_and_natural_dimensions() {
        let bytes = png(480, 320);
        let expected = bytes.clone();
        let resources =
            load_html_image_resources(vec![HtmlImageSource::LocalPath("full.png".into())], |_| {
                Some((bytes.clone(), "image/png".into()))
            });
        let image = resources.images["full.png"].as_ref().unwrap();
        assert_eq!((image.width, image.height), (480.0, 320.0));
        assert_eq!(image.image.bytes(), expected.as_slice());
        assert!(resources.retained_bytes() <= CONTENT_RESOURCE_BUDGET_BYTES);
    }

    #[test]
    fn incremental_budget_keeps_first_image_and_allows_later_small_image() {
        let large = png(1600, 1600);
        let small = png(24, 16);
        let resources = load_html_image_resources(
            vec![
                HtmlImageSource::LocalPath("z-first.png".into()),
                HtmlImageSource::LocalPath("a-over-budget.png".into()),
                HtmlImageSource::LocalPath("m-small.png".into()),
            ],
            |source| {
                let bytes = if source_key(source).as_deref() == Some("m-small.png") {
                    &small
                } else {
                    &large
                };
                Some((bytes.clone(), "image/png".into()))
            },
        );
        assert_eq!(resources.images.len(), 2);
        assert!(resources.images.contains_key("z-first.png"));
        assert!(!resources.images.contains_key("a-over-budget.png"));
        assert!(resources.images.contains_key("m-small.png"));
        assert_eq!(
            resources.images["z-first.png"].as_ref().unwrap().width,
            1600.0
        );
        assert!(resources.retained_bytes() <= CONTENT_RESOURCE_BUDGET_BYTES);
    }

    #[test]
    fn repeated_failures_are_read_once_and_unresolved_sources_are_never_read() {
        let path = HtmlImageSource::LocalPath("missing.png".into());
        let mut calls = Vec::new();
        let resources = load_html_image_resources(
            vec![
                path.clone(),
                path,
                HtmlImageSource::RelativePath("Image/a.png".into()),
                HtmlImageSource::Unavailable {
                    original: "https://example.invalid/a.png".into(),
                    reason: html::HtmlImageUnavailable::Remote,
                },
            ],
            |source| {
                calls.push(source_key(source).unwrap());
                None
            },
        );
        assert_eq!(calls, ["missing.png"]);
        assert!(resources.images.is_empty());
    }

    #[test]
    fn supported_data_resources_share_budget_and_svg_stays_unavailable() {
        let data = HtmlImageSource::EmbeddedData {
            mime: "image/png".into(),
            uri: "data:image/png;base64,test".into(),
        };
        let resources = load_html_image_resources(
            vec![
                data.clone(),
                data,
                HtmlImageSource::LocalPath("local.svg".into()),
            ],
            |source| match source {
                HtmlImageSource::EmbeddedData { .. } => Some((png(20, 12), "image/png".into())),
                _ => Some((
                    b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
                    "image/svg+xml".into(),
                )),
            },
        );
        assert_eq!(resources.images.len(), 1);
        assert!(resources.images.contains_key("data:image/png;base64,test"));
        assert!(resources.retained_bytes() <= CONTENT_RESOURCE_BUDGET_BYTES);
    }
}
