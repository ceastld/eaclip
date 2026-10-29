//! History file/local-path bodies. Projections and in-memory resources are
//! supplied by the parent; rendering never probes a path, Shell or image file.

use gpui_rsx::rsx;
use super::{
    files::{self, FileEntryProjection, FileListProjection, LocalPathProjection},
    links::OpenTarget,
    model::{self, ContentMode},
    ContentImage, ContentMetrics, ContentOpenRequest, ContentPaintObserver, ContentResources,
    ContentRole, HistoryThumbnailTarget, HistoryThumbnailWrapper, OpenHandler,
};
use crate::{
    preview::DesignTokens,
    raster_layout::{image_dpi_compensation, natural_image_layout},
    view::color,
};
use cliphost_core::i18n::tr;
use gpui::{
    canvas, div, fill, img, point, prelude::*, px, relative, rgb, size, AnyElement, Bounds,
    FontWeight, Hsla, MouseButton, ObjectFit, Rgba, SharedString,
};
use crate::crisp_svg::svg;

fn mix(a: Hsla, b: Hsla, a_weight: f32) -> Hsla {
    let a: Rgba = a.into();
    let b: Rgba = b.into();
    let other = 1.0 - a_weight;
    Rgba {
        r: a.r * a_weight + b.r * other,
        g: a.g * a_weight + b.g * other,
        b: a.b * a_weight + b.b * other,
        a: a.a * a_weight + b.a * other,
    }
    .into()
}

fn missing(resources: &ContentResources, path: &str) -> bool {
    resources
        .paths
        .get(path)
        .is_some_and(|info| info.exists == Some(false))
}

fn size_bytes(resources: &ContentResources, path: &str) -> Option<u64> {
    resources.paths.get(path).and_then(|info| info.size_bytes)
}

fn image<'a>(resources: &'a ContentResources, path: &str) -> Option<&'a ContentImage> {
    resources.images.get(path).and_then(Option::as_ref)
}

fn positive(value: f32) -> f32 {
    if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    }
}

fn natural_dimension(value: f32) -> Option<u32> {
    (value.is_finite() && value > 0.0).then(|| value.round() as u32)
}

fn image_meta(bitmap: Option<&ContentImage>, bytes: Option<u64>) -> Option<String> {
    files::format_image_meta(
        bitmap.and_then(|bitmap| natural_dimension(bitmap.width)),
        bitmap.and_then(|bitmap| natural_dimension(bitmap.height)),
        bytes,
    )
}

fn tooltip(text: String) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView {
    move |_, cx| cx.new(|_| super::ContentTooltip(text.clone())).into()
}

/// `reveal` (minimal rows, DESIGN 2026-10-14): the label stays hidden and
/// neutral until its chip is hovered, keeping the click target in place.
fn open_button(
    item_id: &str,
    entry: &FileEntryProjection,
    is_missing: bool,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    reveal: Option<SharedString>,
) -> AnyElement {
    let t = tokens.theme(dark);
    let request = ContentOpenRequest {
        item_id: item_id.into(),
        target: OpenTarget::ShellPath(entry.path.clone()),
    };
    rsx! {
        <div id={SharedString::from(format!(
            "history-file-open-{item_id}-{}",
            entry.original_index
        ))}
            flex_none ml={px(2.0)} pl={px(6.0)} border_l_1 />
    }
        .border_color(color(&t.border_secondary).opacity(if is_missing { 0.40 } else { 0.55 }))
        .text_size(px(11.0))
        .line_height(relative(1.3))
        .font_weight(FontWeight::NORMAL)
        .text_color(color(if is_missing {
            &t.text_muted
        } else if reveal.is_some() {
            &t.text_secondary
        } else {
            &t.accent
        }))
        .when_some(reveal, |button, group| {
            button.invisible().group_hover(group, |style| style.visible())
        })
        .when(!is_missing, |button| button.cursor_pointer())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            if !is_missing && event.click_count() <= 1 {
                on_open(request.clone(), window, cx);
            }
        })
        .tooltip(tooltip(
            tr(
                if is_missing {
                    "路径不存在"
                } else {
                    "在资源管理器或默认应用中打开"
                },
                if is_missing {
                    "Path unavailable"
                } else {
                    "Open in Explorer or default application"
                },
            )
            .into(),
        ))
        .child(tr("打开", "Open"))
        .into_any_element()
}

fn pill(label: String, tokens: &DesignTokens, dark: bool) -> AnyElement {
    let t = tokens.theme(dark);
    rsx! {
        <div id={"history-file-meta"} flex_none rounded={px(999.0)} border_1
            border_color={color(&t.border_secondary).opacity(0.55)} px={px(5.0)}
            text_size={px(10.0)} line_height={relative(1.4)} font_weight={FontWeight::NORMAL}
            text_color={color(&t.text_muted)} whitespace_nowrap />
    }
        .tooltip(tooltip(label.clone()))
        .child(label)
        .into_any_element()
}

/// A 14px tile consists of alternating 7px squares before image DPI zoom,
/// like the Web conic-gradient.
/// Edge strips stay inside the rounded frame. The canvas paints prepared colors
/// only and never requests an asset or changes the image child's layout.
fn checkerboard(tokens: &DesignTokens, dark: bool, compensation: f32) -> AnyElement {
    let t = tokens.theme(dark);
    let base = mix(color(&t.secondary), color(&t.tertiary), 0.88);
    let alternate = mix(color(&t.tertiary), color(&t.border), 0.55);
    let corner = 6.0 * compensation;
    let tile = 7.0 * compensation;
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            window.paint_quad(fill(bounds, base).corner_radii(px(corner)));
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            let radius = corner.min(width * 0.5).min(height * 0.5);
            let rows = (height / tile).ceil().max(0.0) as usize;
            let columns = (width / tile).ceil().max(0.0) as usize;
            for row in 0..rows {
                let y = row as f32 * tile;
                let bottom = (y + tile).min(height);
                for column in 0..columns {
                    if (row + column) % 2 != 0 {
                        continue;
                    }
                    let x = column as f32 * tile;
                    let right = (x + tile).min(width);
                    if y >= radius && bottom <= height - radius {
                        window.paint_quad(fill(
                            Bounds::new(
                                bounds.origin + point(px(x), px(y)),
                                size(px(right - x), px(bottom - y)),
                            ),
                            alternate,
                        ));
                        continue;
                    }
                    let mut strip = y;
                    while strip < bottom {
                        let end = (strip + 1.0).min(bottom);
                        let middle = (strip + end) * 0.5;
                        let edge = middle.min(height - middle);
                        let inset = if edge < radius {
                            radius - (radius * radius - (radius - edge).powi(2)).max(0.0).sqrt()
                        } else {
                            0.0
                        };
                        let left = x.max(inset);
                        let right = right.min(width - inset);
                        if right > left {
                            window.paint_quad(fill(
                                Bounds::new(
                                    bounds.origin + point(px(left), px(strip)),
                                    size(px(right - left), px(end - strip)),
                                ),
                                alternate,
                            ));
                        }
                        strip = end;
                    }
                }
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

fn large_image(
    bitmap: &ContentImage,
    available_width: f32,
    max_height: f32,
    raster_scale: f32,
    target: HistoryThumbnailTarget,
    index: usize,
    tokens: &DesignTokens,
    dark: bool,
    observer: &Option<ContentPaintObserver>,
    wrapper: &Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    let layout = natural_image_layout(
        bitmap.width,
        bitmap.height,
        available_width,
        max_height,
        raster_scale,
    );
    let (width, height) = (layout.width_dip, layout.height_dip);
    let compensation = image_dpi_compensation(raster_scale);
    let radius = 6.0 * compensation;
    let child = img(bitmap.image.clone())
        .w(px(width))
        .h(px(height))
        .object_fit(ObjectFit::Contain)
        .rounded(px(radius))
        .border(px(compensation))
        .border_color(color(&tokens.theme(dark).border))
        .with_fallback(|| div().into_any_element())
        .into_any_element();
    let role = match target {
        HistoryThumbnailTarget::FileImage(_) | HistoryThumbnailTarget::FileMedia(_) => "file-image",
        HistoryThumbnailTarget::TextImage => "text-image",
    };
    let child = if let Some(wrapper) = wrapper {
        wrapper(target, Some(bitmap.image.clone()), child)
    } else {
        child
    };
    let child = super::observed_content_part(
        super::tracked_image(bitmap.image.clone(), child),
        observer,
        role,
        index,
        true,
        Some(bitmap.image.clone()),
    );
    rsx! {
        <div relative flex_none w={px(width)} h={px(height)} max_w_full rounded={px(radius)}
            overflow_hidden>
            {checkerboard(tokens, dark, compensation)}
            {child}
        </div>
    }
        .into_any_element()
}

fn image_meta_badge(label: String, tokens: &DesignTokens, dark: bool) -> AnyElement {
    let t = tokens.theme(dark);
    rsx! {
        <div absolute bottom={px(2.0)} right={px(2.0)} max_w_full px={px(6.0)} py={px(2.0)}
            rounded={px(4.0)} border_1 border_color={color(&t.border).opacity(0.55)}
            bg={color(&t.surface).opacity(0.58)} text_size={px(10.0)} line_height={relative(1.35)}
            font_weight={FontWeight::NORMAL} text_color={color(&t.text)} whitespace_nowrap
            text_ellipsis overflow_hidden>
            {label}
        </div>
    }
        .into_any_element()
}

fn single_file_caption(
    item_id: &str,
    entry: &FileEntryProjection,
    minimal: bool,
    width: f32,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
) -> AnyElement {
    let t = tokens.theme(dark);
    let name = model::collapse_inline_whitespace(&entry.name);
    if minimal {
        return rsx! {
            <div id={"history-image-file-caption"} max_w={px(positive(width))} min_w_0
                text_size={px(12.0)} line_height={relative(1.35)}
                font_weight={FontWeight::NORMAL} text_color={color(&t.text_secondary)}
                whitespace_nowrap text_ellipsis overflow_hidden />
        }
            .tooltip(tooltip(entry.path.clone()))
            .child(name)
            .into_any_element();
    }
    rsx! {
        <div id={"history-image-file-footer"} flex items_center gap={px(4.0)}
            max_w={px(positive(width))} min_w_0 pl={px(8.0)} pr={px(4.0)} py={px(2.0)} border_1
            rounded={px(6.0)} border_color={color(&t.border_secondary).opacity(0.60)}
            bg={color(&t.tertiary).opacity(0.75)} text_size={px(11.0)}
            line_height={relative(1.3)} />
    }
        .tooltip(tooltip(entry.path.clone()))
        .child(
            rsx! {
                <div flex_1 min_w_0 font_weight={FontWeight::MEDIUM} text_color={color(&t.text)}
                    whitespace_nowrap text_ellipsis overflow_hidden>
                    {name}
                </div>
            },
        )
        .child(open_button(item_id, entry, false, tokens, dark, on_open, None))
        .into_any_element()
}

fn file_chip(
    item_id: &str,
    entry: &FileEntryProjection,
    resources: &ContentResources,
    width: f32,
    show_size: bool,
    show_metadata: bool,
    minimal: bool,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: &Option<ContentPaintObserver>,
) -> AnyElement {
    let t = tokens.theme(dark);
    let is_missing = missing(resources, &entry.path);
    let reveal = minimal.then(|| {
        SharedString::from(format!("history-file-chip-group-{item_id}-{}", entry.original_index))
    });
    let bitmap = if !is_missing && entry.is_image {
        image(resources, &entry.path)
    } else {
        None
    };
    let icon = if let Some(bitmap) = bitmap {
        super::observed_content_part(
            super::tracked_image(
                bitmap.image.clone(),
                img(bitmap.image.clone())
                    .size(px(40.0))
                    .object_fit(ObjectFit::Cover)
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(color(&t.border))
                    .bg(color(&t.secondary))
                    .into_any_element(),
            ),
            observer,
            "file-image",
            entry.original_index,
            true,
            Some(bitmap.image.clone()),
        )
    } else if let Some(icon) = resources.icons.get(&entry.path).and_then(Option::as_ref) {
        super::observed_content_part(
            super::tracked_image(
                icon.clone(),
                img(icon.clone())
                    .size(px(16.0))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
            ),
            observer,
            "file-icon",
            entry.original_index,
            true,
            Some(icon.clone()),
        )
    } else {
        super::observed_content_part(
            svg()
                .path(tokens.icons.get("file").cloned().unwrap_or_default())
                .size(px(16.0))
                .text_color(color(&t.text_secondary))
                .into_any_element(),
            observer,
            "file-icon",
            entry.original_index,
            false,
            None,
        )
    };
    let name = model::collapse_inline_whitespace(&entry.name);
    let mut chip = rsx! {
        <div id={SharedString::from(format!(
            "history-file-chip-{item_id}-{}",
            entry.original_index
        ))}
            flex items_center gap={px(6.0)} max_w={px(positive(width))} min_w_0 px={px(6.0)}
            py={px(2.0)} rounded={px(6.0)} />
    }
        // Minimal rows use a borderless field-coloured chip.
        .when(!minimal, |chip| {
            chip.border_1()
                .border_color(color(&t.border_secondary).opacity(0.60))
                .bg(color(&t.tertiary).opacity(0.75))
        })
        .when_some(reveal.clone(), |chip, group| {
            chip.group(group).bg(color(&t.secondary))
        })
        .text_size(px(11.0))
        .line_height(relative(1.3))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color(&t.text_secondary))
        .whitespace_nowrap()
        .tooltip(tooltip(entry.path.clone()))
        .child(div().flex_none().child(icon))
        .child(
            rsx! {
                <div flex_1 min_w_0 text_ellipsis overflow_hidden />
            }
                .when(is_missing, |name| name.line_through())
                .child(name),
        );
    if show_metadata && !is_missing && entry.is_image {
        if let Some(meta) = image_meta(bitmap, size_bytes(resources, &entry.path)) {
            chip = chip.child(pill(meta, tokens, dark));
        }
    } else if show_metadata && show_size {
        let label = files::format_file_size(size_bytes(resources, &entry.path));
        if !label.is_empty() {
            chip = chip.child(pill(label, tokens, dark));
        }
    }
    chip.child(open_button(
        item_id, entry, is_missing, tokens, dark, on_open, reveal,
    ))
    .into_any_element()
}

pub(super) fn render_files(
    item_id: &str,
    projection: &FileListProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    image_max_height: f32,
    raster_scale: f32,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    thumbnail_wrapper: Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    let minimal = projection.mode == ContentMode::Minimal;
    if projection.entries.is_empty() {
        return super::render_text_content_observed(
            item_id,
            ContentRole::Primary,
            &projection.fallback,
            metrics,
            tokens,
            dark,
            on_open,
            observer,
        );
    }
    if let Some(entry) = super::history_file_image_preview(projection, resources) {
        let bitmap = image(resources, &entry.path);
        let mut column = rsx! {
            <div flex flex_col items_start min_w_0 max_w_full gap={px(2.0)} />
        };
        if let Some(bitmap) = bitmap {
            column = column.child(large_image(
                bitmap,
                metrics.width,
                image_max_height,
                raster_scale,
                HistoryThumbnailTarget::FileImage(entry.original_index),
                entry.original_index,
                tokens,
                dark,
                &observer,
                &thumbnail_wrapper,
            ));
        }
        column = column.child(single_file_caption(
            item_id,
            entry,
            minimal,
            metrics.width,
            tokens,
            dark,
            on_open,
        ));
        if let Some(meta) = image_meta(bitmap, size_bytes(resources, &entry.path)) {
            column = column.child(image_meta_badge(meta, tokens, dark));
        }
        return column.relative().into_any_element();
    }
    render_file_chips(
        item_id, projection, resources, metrics, tokens, dark, on_open, observer, true,
        thumbnail_wrapper,
    )
}

/// Favorites share thumbnails, file names and stamped Open handling, while
/// keeping their existing Web chips free of history-only size/meta labels.
pub(super) fn render_file_chips(
    item_id: &str,
    projection: &FileListProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    show_metadata: bool,
    thumbnail_wrapper: Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    if projection.entries.is_empty() {
        return super::render_text_content_observed(
            item_id,
            ContentRole::Primary,
            &projection.fallback,
            metrics,
            tokens,
            dark,
            on_open,
            observer,
        );
    }
    let minimal = projection.mode == ContentMode::Minimal;
    let preview_len = projection.entries.len().min(files::FILE_CHIP_PREVIEW_LIMIT);
    let overflow = projection.total_entries > preview_len;
    let width = (metrics.width - if minimal { 28.0 } else { 0.0 }).max(1.0);
    let mut chips = rsx! {
        <div flex flex_wrap min_w_0 max_w_full />
    }
        .gap(px(if minimal { 4.0 } else { 6.0 }))
        .when(minimal, |chips| chips.pr(px(28.0)));
    for (index, entry) in projection
        .entries
        .iter()
        .take(files::FILE_CHIP_PREVIEW_LIMIT)
        .enumerate()
    {
        let is_image = entry.is_image && !missing(resources, &entry.path);
        let chip = file_chip(
            item_id,
            entry,
            resources,
            width,
            !is_image && index + 1 == preview_len && !overflow,
            show_metadata,
            minimal,
            tokens,
            dark,
            on_open.clone(),
            &observer,
        );
        let target = if entry.is_image {
            Some(HistoryThumbnailTarget::FileImage(entry.original_index))
        } else if crate::hover::content::local_video_path(&entry.path) {
            Some(HistoryThumbnailTarget::FileMedia(entry.original_index))
        } else {
            None
        };
        let chip = match (thumbnail_wrapper.as_ref(), target) {
            (Some(wrapper), Some(target)) if !missing(resources, &entry.path) =>
                wrapper(target, None, chip),
            _ => chip,
        };
        chips = chips.child(chip);
    }
    if overflow {
        let t = tokens.theme(dark);
        chips = chips.child(
            rsx! {
                <div id={"history-files-overflow"} min_w={px(36.0)} flex_none px={px(6.0)}
                    py={px(2.0)} border_1 border_color={color(&t.border_secondary).opacity(0.6)}
                    rounded={px(6.0)} bg={color(&t.tertiary).opacity(0.75)}
                    text_color={color(&t.text_muted)} text_size={px(11.0)}
                    line_height={relative(1.3)} />
            }
                .tooltip(tooltip(format!(
                    "{} {}",
                    projection.total_entries - preview_len,
                    tr("个文件未显示", "more files")
                )))
                .child("...")
                .into_any_element(),
        );
    }
    chips.into_any_element()
}

pub(super) fn render_local_path(
    item_id: &str,
    projection: &LocalPathProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    image_max_height: f32,
    raster_scale: f32,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    thumbnail_wrapper: Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    let is_missing = missing(resources, &projection.path);
    let mut column = rsx! {
        <div flex flex_col relative min_w_0 w_full gap={px(4.0)} />
    };
    if projection.is_image && !is_missing {
        if let Some(bitmap) = image(resources, &projection.path) {
            let mut preview =
                rsx! {
                    <div relative min_w_0 max_w_full mb={px(4.0)}>
                        {large_image(
                        bitmap,
                        metrics.width,
                        image_max_height,
                        raster_scale,
                        HistoryThumbnailTarget::TextImage,
                        0,
                        tokens,
                        dark,
                        &observer,
                        &thumbnail_wrapper,
                    )}
                    </div>
                };
            // Text-image rows show original dimensions, not a local-path byte
            // size merely because the background probe happened to collect it.
            if let Some(meta) = image_meta(Some(bitmap), None) {
                preview = preview.child(image_meta_badge(meta, tokens, dark));
            }
            column = column.child(preview);
        }
    }
    let text = super::render_text_content_observed(
        item_id,
        ContentRole::Primary,
        &projection.text,
        metrics,
        tokens,
        dark,
        on_open,
        observer,
    );
    if is_missing {
        let t = tokens.theme(dark);
        let red: Hsla = rgb(0xc62828).into();
        column = column.child(
            rsx! {
                <div flex flex_wrap items_baseline gap={px(8.0)} min_w_0>
                    <div flex_1 min_w_0>
                        {text}
                    </div>
                </div>
            }
                .child(
                    rsx! {
                        <div id={"history-path-missing"} flex_none px={px(7.0)} py={px(1.0)}
                            rounded={px(999.0)} border_1
                            border_color={mix(red, color(&t.border_secondary), 0.42)}
                            bg={mix(red, color(&t.tertiary), 0.12)}
                            text_color={mix(color(&t.text), red, 0.88)} text_size={px(10.0)}
                            font_weight={FontWeight::SEMIBOLD} line_height={relative(1.25)} />
                    }
                        .tooltip(tooltip(
                            tr(
                                "路径在磁盘上不存在或不可访问",
                                "Path does not exist or is inaccessible",
                            )
                            .into(),
                        ))
                        .child(tr("缺失", "Missing")),
                ),
        );
    } else {
        column = column.child(text);
    }
    column.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_or_size_only_metadata_never_invents_image_dimensions() {
        assert_eq!(image_meta(None, None), None);
        assert_eq!(image_meta(None, Some(1024)), Some("1KB".into()));
        assert_eq!(natural_dimension(f32::NAN), None);
        assert_eq!(natural_dimension(0.0), None);
    }
}
