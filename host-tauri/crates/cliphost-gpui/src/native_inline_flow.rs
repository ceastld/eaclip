//! GPUI consumer of the pure continuous inline layout. No disk, Shell, network
//! or clipboard work happens here. Only the hover Open button consumes clicks.

use gpui_rsx::rsx;
use super::{
    colors::{collect_color_tokens_with_boundaries, ColorToken},
    inline_layout::{
        ellipsized_text, fitting_prefix_bytes, layout_inline_with_spaces, wrap_grapheme_lines,
        InlineAtom, InlineLayout, InlineLine, PlacedInline,
    },
    model::{self, MixedPart, TextProjection},
    ContentImage, ContentMetrics, ContentOpenRequest, ContentPaintObserver, ContentPaintPart,
    ContentResources, ContentRole, OpenHandler, ThumbnailWrapper,
};
use crate::{preview::DesignTokens, view::color};
use cliphost_core::i18n::tr;
use gpui::{
    div, img, point, prelude::*, px, relative, rgba, size, AnyElement, App, AvailableSpace, Bounds,
    ContentMask, DispatchPhase, Element, ElementId, Font, GlobalElementId, Hsla,
    InspectorElementId, LayoutId, MouseButton, MouseMoveEvent, ObjectFit, Pixels, ShapedLine,
    SharedString, Style, TextRun, UnderlineStyle, Window,
};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Flags {
    link: Option<usize>,
    hit: bool,
}

#[derive(Clone)]
enum FlowSource {
    Text { text: String, flags: Flags },
    Object { object: InlineObject, flags: Flags },
    Break,
}

#[derive(Clone)]
enum InlineObject {
    Image {
        image: ContentImage,
        width: f32,
        height: f32,
        original_segment_index: Option<usize>,
    },
    Chip {
        text: String,
        tooltip: Option<String>,
        missing: bool,
    },
    Swatch(ColorToken),
}

#[derive(Clone)]
struct FlowTheme {
    foreground: Hsla,
    accent: Hsla,
    selected: Hsla,
    surface: Hsla,
    border: Hsla,
}

fn theme(tokens: &DesignTokens, dark: bool, secondary: bool) -> FlowTheme {
    let t = tokens.theme(dark);
    FlowTheme {
        foreground: color(if secondary {
            &t.text_secondary
        } else {
            &t.text
        }),
        accent: color(&t.accent),
        selected: color(&t.selected),
        surface: color(&t.surface),
        border: color(&t.border_secondary).opacity(0.55),
    }
}

fn source_flags(projection: &TextProjection, offset: usize) -> Flags {
    Flags {
        link: projection
            .links
            .iter()
            .position(|link| link.range.contains(&offset)),
        hit: projection
            .hit_ranges
            .iter()
            .any(|range| range.contains(&offset)),
    }
}

/// Source-only splitting: recognize colors separately inside exact link/hit
/// slices, matching the existing frontend scanner order. Atom boundaries never
/// insert spaces or create word-break opportunities by themselves.
fn text_sources(projection: &TextProjection) -> Vec<FlowSource> {
    let mut cuts = vec![0, projection.text.len()];
    for range in projection
        .links
        .iter()
        .map(|link| &link.range)
        .chain(&projection.hit_ranges)
    {
        cuts.extend([range.start, range.end]);
    }
    let colors = collect_color_tokens_with_boundaries(&projection.text, &cuts);
    for token in &colors {
        cuts.extend([token.range.start, token.range.end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut sources = Vec::new();
    for pair in cuts.windows(2) {
        if pair[0] >= pair[1] {
            continue;
        }
        let flags = source_flags(projection, pair[0]);
        if let Some(token) = colors
            .iter()
            .find(|token| token.range == (pair[0]..pair[1]))
        {
            sources.push(FlowSource::Object {
                object: InlineObject::Swatch(token.clone()),
                flags,
            });
        } else {
            // CSS .item-inline-link is white-space:normal even inside the
            // ordinary pre-wrap text body. Other text retains hard line breaks.
            let text = &projection.text[pair[0]..pair[1]];
            if flags.link.is_some() {
                sources.push(FlowSource::Text {
                    text: model::collapse_inline_whitespace(text),
                    flags,
                });
            } else {
                push_pre_wrap_sources(&mut sources, text, flags);
            }
        }
    }
    sources
}

fn push_pre_wrap_sources(sources: &mut Vec<FlowSource>, text: &str, flags: Flags) {
    let mut start = 0;
    let mut cursor = 0;
    while cursor < text.len() {
        let ch = text[cursor..].chars().next().unwrap();
        if matches!(ch, '\r' | '\n' | '\u{2028}' | '\u{2029}') {
            if start < cursor {
                sources.push(FlowSource::Text {
                    text: text[start..cursor].into(),
                    flags,
                });
            }
            cursor += ch.len_utf8();
            if ch == '\r' && text[cursor..].starts_with('\n') {
                cursor += 1;
            }
            sources.push(FlowSource::Break);
            start = cursor;
        } else {
            cursor += ch.len_utf8();
        }
    }
    if start < text.len() {
        sources.push(FlowSource::Text {
            text: text[start..].into(),
            flags,
        });
    }
}

pub(super) fn render_mixed_inline(
    parts: &[MixedPart],
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
) -> AnyElement {
    render_mixed_inline_with_observer(parts, resources, metrics, tokens, dark, None)
}

pub(super) fn render_mixed_inline_with_observer(
    parts: &[MixedPart],
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    observer: Option<ContentPaintObserver>,
) -> AnyElement {
    render_mixed_inline_with_callbacks(parts, resources, metrics, tokens, dark, observer, &[], None)
}

/// Bind only an explicit original DTO image index. Display projections may
/// filter blank text and cap segments, so a flow source index is not a fallback.
fn original_segment_index(indices: &[usize], part_index: usize) -> Option<usize> {
    indices.get(part_index).copied()
}

pub(super) fn render_mixed_inline_with_callbacks(
    parts: &[MixedPart],
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    observer: Option<ContentPaintObserver>,
    original_segment_indices: &[usize],
    thumbnail_wrapper: Option<ThumbnailWrapper>,
) -> AnyElement {
    let mut sources = Vec::new();
    for (part_index, part) in parts.iter().enumerate() {
        let object = match part {
            MixedPart::Text(text) => {
                sources.push(FlowSource::Text {
                    text: model::collapse_inline_whitespace(text),
                    flags: Flags::default(),
                });
                continue;
            }
            MixedPart::Image { path } => {
                match path
                    .as_ref()
                    .and_then(|path| resources.images.get(path))
                    .and_then(Option::as_ref)
                {
                    Some(image) => {
                        let (width, height) = model::compact_image_size(
                            image.width,
                            image.height,
                            metrics.width - 8.0,
                        );
                        InlineObject::Image {
                            image: image.clone(),
                            width,
                            height,
                            original_segment_index: original_segment_index(
                                original_segment_indices,
                                part_index,
                            ),
                        }
                    }
                    None => InlineObject::Chip {
                        text: tr("[图片不可用]", "[Image unavailable]").into(),
                        tooltip: None,
                        missing: true,
                    },
                }
            }
            MixedPart::File { name, path } => InlineObject::Chip {
                text: model::collapse_inline_whitespace(name),
                tooltip: Some(path.clone()),
                missing: false,
            },
            MixedPart::Placeholder(text) => InlineObject::Chip {
                text: model::collapse_inline_whitespace(text),
                tooltip: None,
                missing: false,
            },
        };
        sources.push(FlowSource::Object {
            object,
            flags: Flags::default(),
        });
    }
    let max_height = model::COMPACT_IMAGE_MAX_HEIGHT + metrics.font_size * 1.45 + 4.0;
    rsx! {
        <div w_full min_w_0 text_size={px(metrics.font_size)} line_height={relative(1.35)}
            max_h={px(max_height)} overflow_hidden mb={px(2.0)}>
            {InlineFlow {
            id: None,
            sources: Rc::new(sources),
            metrics: ContentMetrics {
                line_height: 1.35,
                ..metrics
            },
            theme: theme(tokens, dark, false),
            tokens: tokens.clone(),
            dark,
            preserve_spaces: false,
            ellipsis: false,
            max_height,
            links: None,
            observer,
            thumbnail_wrapper,
            computed: Rc::new(RefCell::new(None)),
        }}
        </div>
    }
        .into_any_element()
}

pub(super) fn render_text_inline(
    item_id: &str,
    role: ContentRole,
    projection: &TextProjection,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
) -> AnyElement {
    render_text_inline_with_observer(
        item_id, role, projection, metrics, tokens, dark, on_open, None,
    )
}

pub(super) fn render_text_inline_with_observer(
    item_id: &str,
    role: ContentRole,
    projection: &TextProjection,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
) -> AnyElement {
    let max_height = metrics.font_size * metrics.line_height * metrics.max_lines.max(1) as f32;
    let id = SharedString::from(format!(
        "native-color-inline-{item_id}-{}",
        match role {
            ContentRole::Primary => "primary",
            ContentRole::Secondary => "secondary",
        }
    ));
    rsx! {
        <div w_full min_w_0 text_size={px(metrics.font_size)}
            line_height={relative(metrics.line_height)} max_h={px(max_height)} overflow_hidden />
    }
        .child(InlineFlow {
            id: Some(id.into()),
            sources: Rc::new(text_sources(projection)),
            metrics,
            theme: theme(tokens, dark, matches!(role, ContentRole::Secondary)),
            tokens: tokens.clone(),
            dark,
            preserve_spaces: true,
            ellipsis: true,
            max_height,
            links: Some(FlowLinks {
                item_id: item_id.into(),
                targets: projection
                    .links
                    .iter()
                    .map(|link| link.target.clone())
                    .collect(),
                on_open,
            }),
            observer,
            thumbnail_wrapper: None,
            computed: Rc::new(RefCell::new(None)),
        })
        .into_any_element()
}

struct FlowLinks {
    item_id: String,
    targets: Vec<super::links::OpenTarget>,
    on_open: OpenHandler,
}

struct InlineFlow {
    id: Option<ElementId>,
    sources: Rc<Vec<FlowSource>>,
    metrics: ContentMetrics,
    theme: FlowTheme,
    tokens: DesignTokens,
    dark: bool,
    preserve_spaces: bool,
    ellipsis: bool,
    max_height: f32,
    links: Option<FlowLinks>,
    observer: Option<ContentPaintObserver>,
    thumbnail_wrapper: Option<ThumbnailWrapper>,
    computed: Rc<RefCell<Option<InlineLayout>>>,
}

fn measured(text: &str, font: &Font, font_size: f32, window: &Window) -> f32 {
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    f32::from(
        window
            .text_system()
            .shape_line(text.to_owned().into(), px(font_size), &[run], None)
            .width,
    )
}

fn label_dimensions(
    text: &str,
    font: &Font,
    font_size: f32,
    line_height: f32,
    window: &Window,
) -> (f32, f32) {
    // An inline-flex label may contain explicit newlines. Its complete box
    // remains atomic; measure each label line without feeding \n to shape_line.
    let lines = text
        .split(['\r', '\n', '\u{2028}', '\u{2029}'])
        .collect::<Vec<_>>();
    let width = lines
        .iter()
        .map(|text| measured(text, font, font_size, window))
        .fold(0.0_f32, f32::max);
    (width, line_height * lines.len().max(1) as f32)
}

fn object_size(
    object: &InlineObject,
    flags: Flags,
    font: &Font,
    metrics: ContentMetrics,
    available_width: f32,
    window: &Window,
) -> (f32, f32) {
    match object {
        InlineObject::Image { width, height, .. } => {
            let scale = ((available_width - 8.0).max(1.0) / width.max(1.0)).min(1.0);
            (width * scale + 8.0, height * scale)
        }
        InlineObject::Chip { text, .. } => {
            let (width, height) = label_dimensions(text, font, 12.0, 12.0 * 1.35, window);
            (width + 16.0, height + 4.0)
        }
        InlineObject::Swatch(token) => {
            let text = color_label_text(token, flags);
            let width = (label_dimensions(&text, font, metrics.font_size, 1.0, window).0 + 16.0)
                .min(available_width.max(1.0));
            let lines = wrap_grapheme_lines(&text, (width - 16.0).max(1.0), |text| {
                measured(text, font, metrics.font_size, window)
            });
            (
                width,
                (lines.len() as f32 * metrics.font_size * metrics.line_height).max(12.0),
            )
        }
    }
}

fn compute_layout(
    sources: &[FlowSource],
    width: f32,
    metrics: ContentMetrics,
    preserve_spaces: bool,
    font: &Font,
    window: &Window,
) -> InlineLayout {
    let width = width.max(1.0);
    let line_height = metrics.font_size * metrics.line_height;
    let mut output = InlineLayout {
        width,
        ..Default::default()
    };
    let mut atoms = Vec::new();
    let append = |output: &mut InlineLayout, atoms: &mut Vec<InlineAtom>, empty_line: bool| {
        let layout =
            layout_inline_with_spaces(atoms, width, line_height, preserve_spaces, |text| {
                measured(text, font, metrics.font_size, window)
            });
        if layout.lines.is_empty() && empty_line {
            output.lines.push(InlineLine {
                y: output.height,
                height: line_height,
                ..Default::default()
            });
            output.height += line_height;
        } else {
            for mut line in layout.lines {
                line.y += output.height;
                for fragment in &mut line.fragments {
                    match fragment {
                        PlacedInline::Text(text) => text.y += output.height,
                        PlacedInline::Object(object) => object.y += output.height,
                    }
                }
                output.lines.push(line);
            }
            output.height += layout.height;
        }
        atoms.clear();
    };
    let mut ended_with_break = false;
    for (source, value) in sources.iter().enumerate() {
        match value {
            FlowSource::Text { text, .. } => atoms.push(InlineAtom::Text {
                source,
                text: text.clone(),
            }),
            FlowSource::Object { object, flags } => {
                let (object_width, height) =
                    object_size(object, *flags, font, metrics, width, window);
                atoms.push(InlineAtom::Object {
                    source,
                    width: object_width,
                    height,
                });
            }
            FlowSource::Break => append(&mut output, &mut atoms, true),
        }
        ended_with_break = matches!(value, FlowSource::Break);
    }
    append(&mut output, &mut atoms, ended_with_break);
    output
}

#[derive(Clone)]
struct LinkArea {
    bounds: Bounds<Pixels>,
    index: usize,
}

enum Painted {
    Text {
        line: ShapedLine,
        origin: gpui::Point<Pixels>,
        line_height: Pixels,
        source_ranges: Vec<(usize, Range<usize>)>,
    },
    Object(AnyElement),
}

struct FlowPrepaint {
    painted: Vec<Painted>,
    areas: Vec<LinkArea>,
    hovered: Rc<Cell<Option<usize>>>,
    button: Option<AnyElement>,
    button_bounds: Option<Bounds<Pixels>>,
    bounds: Bounds<Pixels>,
}

#[derive(Default)]
struct HoverState {
    signature: u64,
    hovered: Rc<Cell<Option<usize>>>,
}

fn run(text: &str, flags: Flags, font: &Font, theme: &FlowTheme) -> TextRun {
    TextRun {
        len: text.len(),
        font: font.clone(),
        color: if flags.link.is_some() {
            theme.accent
        } else {
            theme.foreground
        },
        background_color: flags.hit.then_some(theme.selected),
        underline: flags.link.map(|_| UnderlineStyle {
            thickness: px(1.0),
            color: Some(theme.accent),
            ..Default::default()
        }),
        strikethrough: None,
    }
}

fn color_label_text(token: &ColorToken, flags: Flags) -> String {
    if flags.link.is_some() {
        model::collapse_inline_whitespace(&token.css)
    } else {
        token.css.clone()
    }
}

/// An atomic inline-flex wrapper has a shrinking, break-all value. Its value
/// lines use the same pure grapheme boundaries in measurement and painting.
struct ColorLabel {
    text: String,
    flags: Flags,
    font: Font,
    theme: FlowTheme,
    width: f32,
    height: f32,
    visible_height: f32,
    font_size: f32,
    line_height: f32,
    force_ellipsis: bool,
    observer: Option<ContentPaintObserver>,
    source: usize,
    clip: Bounds<Pixels>,
}

impl IntoElement for ColorLabel {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ColorLabel {
    type RequestLayoutState = ();
    type PrepaintState = Vec<(ShapedLine, gpui::Point<Pixels>)>;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = px(self.width).into();
        style.size.height = px(self.height).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        if self.visible_height <= 0.0 {
            return Vec::new();
        }
        let ranges = wrap_grapheme_lines(&self.text, self.width, |text| {
            measured(text, &self.font, self.font_size, window)
        });
        let count = (((self.visible_height + 0.001) / self.line_height).floor() as usize)
            .max(1)
            .min(ranges.len());
        let truncated = self.force_ellipsis || count < ranges.len();
        ranges
            .iter()
            .take(count)
            .enumerate()
            .map(|(index, range)| {
                let source = &self.text[range.clone()];
                let display = if truncated && index + 1 == count {
                    ellipsized_text(source, self.width, |text| {
                        measured(text, &self.font, self.font_size, window)
                    })
                } else {
                    source.to_owned()
                };
                let retained = if truncated && index + 1 == count {
                    display.len() - "…".len()
                } else {
                    display.len()
                };
                let mut runs = Vec::new();
                if retained > 0 {
                    runs.push(run(
                        &display[..retained],
                        self.flags,
                        &self.font,
                        &self.theme,
                    ));
                }
                if retained < display.len() {
                    runs.push(run("…", Flags::default(), &self.font, &self.theme));
                }
                let shaped = window.text_system().shape_line(
                    display.into(),
                    px(self.font_size),
                    &runs,
                    None,
                );
                (
                    shaped,
                    bounds.origin + point(px(0.0), px(index as f32 * self.line_height)),
                )
            })
            .collect()
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (line, origin) in state {
            let _ = line.paint_background(*origin, px(self.line_height), window, cx);
            if line
                .paint(*origin, px(self.line_height), window, cx)
                .is_ok()
            {
                if let Some(observer) = &self.observer {
                    observe_text(
                        observer,
                        self.source,
                        line,
                        *origin,
                        px(self.line_height),
                        0..line.len(),
                        self.clip.intersect(&window.content_mask().bounds),
                    );
                }
            }
        }
    }
}

fn observe_text(
    observer: &ContentPaintObserver,
    index: usize,
    line: &ShapedLine,
    origin: gpui::Point<Pixels>,
    line_height: Pixels,
    range: Range<usize>,
    clip: Bounds<Pixels>,
) {
    let start = line.x_for_index(range.start.min(line.len()));
    let end = line.x_for_index(range.end.min(line.len()));
    let height = line.ascent + line.descent;
    let bounds = Bounds::new(
        origin + point(start.min(end), (line_height - height) / 2.0),
        size((end - start).abs(), height),
    )
    .intersect(&clip);
    if bounds.size.width > px(0.0) && bounds.size.height > px(0.0) {
        observer(ContentPaintPart {
            role: "step-text",
            index,
            bounds,
            available: true,
        });
    }
}

/// Delegate the real swatch element's existing layout. The reported rectangle
/// comes from its resolved prepaint bounds, then the current paint mask; it is
/// never inferred from the requested 12px style or a text atom position.
struct ObservedSwatch {
    child: AnyElement,
    observer: ContentPaintObserver,
    index: usize,
    available: bool,
    clip: Bounds<Pixels>,
}
impl IntoElement for ObservedSwatch {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ObservedSwatch {
    type RequestLayoutState = ();
    type PrepaintState = Bounds<Pixels>;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Bounds<Pixels> {
        self.child.prepaint(window, cx);
        bounds.intersect(&self.clip)
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        state: &mut Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let bounds = state.intersect(&window.content_mask().bounds);
        if bounds.size.width > px(0.0) && bounds.size.height > px(0.0) {
            (self.observer)(ContentPaintPart {
                role: "step-colour",
                index: self.index,
                bounds,
                available: self.available,
            });
        }
    }
}

impl InlineFlow {
    fn object_element(
        &self,
        source: usize,
        object: &InlineObject,
        flags: Flags,
        font: &Font,
        width: f32,
        height: f32,
        visible_height: f32,
        ellipsize_label: bool,
        clip: Bounds<Pixels>,
    ) -> AnyElement {
        match object {
            InlineObject::Image {
                image,
                original_segment_index,
                ..
            } => {
                let child = img(image.image.clone())
                    .w(px((width - 8.0).max(1.0)))
                    .h(px(height))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(color(&self.tokens.theme(self.dark).border))
                    .object_fit(ObjectFit::Contain)
                    .with_fallback(|| {
                        div()
                            .child(tr("[图片不可用]", "[Image unavailable]"))
                            .into_any_element()
                    })
                    .into_any_element();
                // The wrapper checks readiness after this real image child
                // paints, avoiding both fallback hover targets and an extra
                // frame before binding a newly ready image. It delegates the
                // child's existing layout; the outer margins stay outside it.
                let child = match (original_segment_index, self.thumbnail_wrapper.as_ref()) {
                    (Some(index), Some(wrap)) => wrap(*index, image.image.clone(), child),
                    _ => child,
                };
                rsx! {
                    <div w={px(width)} h={px(height)} px={px(4.0)}>
                        {super::observed_content_part(
                        super::tracked_image(image.image.clone(), child),
                        &self.observer,
                        "step-icon",
                        source,
                        true,
                        Some(image.image.clone()),
                    )}
                    </div>
                }
                    .into_any_element()
            }
            InlineObject::Chip {
                text,
                tooltip,
                missing,
            } => {
                let chip = super::chip(text.clone(), &self.tokens, self.dark, *missing)
                    .mx(px(0.0))
                    .w_full()
                    .h_full()
                    .text_size(px(12.0))
                    .line_height(relative(1.35))
                    .whitespace_nowrap();
                let mut wrapper = rsx! {
                    <div id={("inline-chip", source)} w={px(width)} h={px(height)} px={px(2.0)}>
                        {chip}
                    </div>
                };
                if let Some(tooltip) = tooltip.clone() {
                    wrapper = wrapper.tooltip(move |_, cx| {
                        cx.new(|_| super::ContentTooltip(tooltip.clone())).into()
                    });
                }
                wrapper.into_any_element()
            }
            InlineObject::Swatch(token) => {
                let background: Hsla = token
                    .color
                    .map(|color| {
                        rgba(
                            (color.red as u32) << 24
                                | (color.green as u32) << 16
                                | (color.blue as u32) << 8
                                | color.alpha as u32,
                        )
                        .into()
                    })
                    .unwrap_or_else(|| rgba(0).into());
                let label = ColorLabel {
                    text: color_label_text(token, flags),
                    flags,
                    font: font.clone(),
                    theme: self.theme.clone(),
                    width: (width - 16.0).max(1.0),
                    height,
                    visible_height,
                    font_size: self.metrics.font_size,
                    line_height: self.metrics.font_size * self.metrics.line_height,
                    force_ellipsis: ellipsize_label,
                    observer: self.observer.clone(),
                    source,
                    clip,
                };
                let tooltip = token.css.clone();
                let swatch = rsx! {
                    <div size={px(12.0)} flex_none rounded={px(2.0)} border_1
                        border_color={self.theme.border} bg={background} />
                }
                    .into_any_element();
                let swatch = if let Some(observer) = self.observer.clone() {
                    ObservedSwatch {
                        child: swatch,
                        observer,
                        index: source,
                        available: token.color.is_some(),
                        clip,
                    }
                    .into_any_element()
                } else {
                    swatch
                };
                rsx! {
                    <div id={("inline-swatch", source)} w={px(width)} h={px(height)} flex
                        items_center gap={px(4.0)} />
                }
                    .when(flags.hit, |root| root.bg(self.theme.selected))
                    .child(swatch)
                    .child(rsx! {
                        <div min_w_0 w={px((width - 16.0).max(1.0))}>
                            {label}
                        </div>
                    })
                    .tooltip(move |_, cx| cx.new(|_| super::ContentTooltip(tooltip.clone())).into())
                    .into_any_element()
            }
        }
    }

    fn signature(&self) -> u64 {
        let mut hasher = blake3::Hasher::new();
        for source in self.sources.iter() {
            match source {
                FlowSource::Text { text, flags } => {
                    hasher.update(text.as_bytes());
                    hasher.update(&[flags.hit as u8]);
                    hasher.update(&flags.link.unwrap_or(usize::MAX).to_le_bytes());
                }
                FlowSource::Object {
                    object: InlineObject::Swatch(token),
                    flags,
                } => {
                    hasher.update(token.css.as_bytes());
                    hasher.update(&[flags.hit as u8]);
                    hasher.update(&flags.link.unwrap_or(usize::MAX).to_le_bytes());
                }
                FlowSource::Object { .. } => {
                    hasher.update(b"object");
                }
                FlowSource::Break => {
                    hasher.update(b"\n");
                }
            }
        }
        u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().unwrap())
    }
}

impl IntoElement for InlineFlow {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for InlineFlow {
    type RequestLayoutState = ();
    type PrepaintState = FlowPrepaint;
    fn id(&self) -> Option<ElementId> {
        self.id.clone()
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, ()) {
        let sources = self.sources.clone();
        let computed = self.computed.clone();
        let metrics = self.metrics;
        let preserve_spaces = self.preserve_spaces;
        let max_height = self.max_height;
        let font = window.text_style().font();
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known
                .width
                .map(f32::from)
                .or_else(|| match available.width {
                    AvailableSpace::Definite(width) => Some(f32::from(width)),
                    _ => None,
                })
                .unwrap_or(metrics.width)
                .max(1.0);
            let layout = compute_layout(&sources, width, metrics, preserve_spaces, &font, window);
            let result = size(px(width), px(layout.height.min(max_height)));
            *computed.borrow_mut() = Some(layout);
            result
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> FlowPrepaint {
        let font = window.text_style().font();
        let layout = self.computed.borrow().clone().unwrap_or_else(|| {
            compute_layout(
                &self.sources,
                f32::from(bounds.size.width),
                self.metrics,
                self.preserve_spaces,
                &font,
                window,
            )
        });
        let mut painted = Vec::new();
        let mut areas = Vec::new();
        let visible_height = f32::from(bounds.size.height);
        let text_height = self.metrics.font_size * self.metrics.line_height;
        let truncated = self.ellipsis && layout.height > visible_height + 0.001;
        let last_visible = layout
            .lines
            .iter()
            .rposition(|line| line.y < visible_height);
        let ellipsis_width = measured("…", &font, self.metrics.font_size, window);
        for (line_index, line) in layout.lines.iter().enumerate() {
            if line.y >= f32::from(bounds.size.height) {
                break;
            }
            let clamp_line = truncated && Some(line_index) == last_visible;
            // A bounded inline-flex label may occupy several visual text lines
            // inside one atomic outer line. Its visible last value line gets
            // the suffix, rather than discarding that complete color object.
            let color_handles_clamp = clamp_line
                && line.fragments.iter().any(|fragment| {
                    let PlacedInline::Object(object) = fragment else {
                        return false;
                    };
                    object.height > text_height + 0.001
                        && matches!(
                            self.sources[object.source],
                            FlowSource::Object {
                                object: InlineObject::Swatch(_),
                                ..
                            }
                        )
                });
            let cap = if clamp_line && !color_handles_clamp {
                (f32::from(bounds.size.width) - ellipsis_width).max(0.0)
            } else {
                f32::from(bounds.size.width)
            };
            let mut ellipsis_x = 0.0_f32;
            let mut index = 0;
            while index < line.fragments.len() {
                match &line.fragments[index] {
                    PlacedInline::Object(object) => {
                        let FlowSource::Object {
                            object: spec,
                            flags,
                        } = &self.sources[object.source]
                        else {
                            unreachable!()
                        };
                        if clamp_line
                            && !color_handles_clamp
                            && object.x + object.width > cap + 0.001
                        {
                            break;
                        }
                        let origin = bounds.origin + point(px(object.x), px(object.y));
                        let mut element = self.object_element(
                            object.source,
                            spec,
                            *flags,
                            &font,
                            object.width,
                            object.height,
                            (visible_height - object.y).max(0.0).min(object.height),
                            color_handles_clamp && object.height > text_height + 0.001,
                            bounds,
                        );
                        element.prepaint_as_root(
                            origin,
                            size(
                                AvailableSpace::Definite(px(object.width)),
                                AvailableSpace::Definite(px(object.height)),
                            ),
                            window,
                            cx,
                        );
                        if let Some(index) = flags.link {
                            areas.push(LinkArea {
                                bounds: Bounds::new(
                                    origin,
                                    size(px(object.width), px(object.height)),
                                ),
                                index,
                            });
                        }
                        painted.push(Painted::Object(element));
                        ellipsis_x = object.x + object.width;
                        index += 1;
                    }
                    PlacedInline::Text(first) => {
                        let origin = bounds.origin + point(px(first.x), px(first.y));
                        let mut text = String::new();
                        let mut runs = Vec::new();
                        let mut ranges = Vec::new();
                        let mut source_ranges = Vec::new();
                        // Shape contiguous Text atoms together, retaining source styles
                        // as runs rather than rendering an independent flex box per atom.
                        while let Some(PlacedInline::Text(part)) = line.fragments.get(index) {
                            let FlowSource::Text { flags, .. } = &self.sources[part.source] else {
                                unreachable!()
                            };
                            let start = text.len();
                            text.push_str(&part.text);
                            runs.push(run(&part.text, *flags, &font, &self.theme));
                            if self.observer.is_some() {
                                source_ranges.push((part.source, start..text.len()));
                            }
                            if let Some(link) = flags.link {
                                ranges.push((start..text.len(), link));
                            }
                            index += 1;
                        }
                        if clamp_line && !color_handles_clamp {
                            let retained =
                                fitting_prefix_bytes(&text, (cap - first.x).max(0.0), |text| {
                                    measured(text, &font, self.metrics.font_size, window)
                                });
                            text.truncate(retained);
                            let mut remaining = retained;
                            runs.retain_mut(|run| {
                                run.len = run.len.min(remaining);
                                remaining -= run.len;
                                run.len > 0
                            });
                        }
                        if text.is_empty() {
                            continue;
                        }
                        let retained = text.len();
                        let shaped = window.text_system().shape_line(
                            text.into(),
                            px(self.metrics.font_size),
                            &runs,
                            None,
                        );
                        let height = px(self.metrics.font_size * self.metrics.line_height);
                        for (range, index) in ranges {
                            if range.start >= retained {
                                continue;
                            }
                            let start = shaped.x_for_index(range.start);
                            let end = shaped.x_for_index(range.end.min(retained));
                            areas.push(LinkArea {
                                bounds: Bounds::new(
                                    origin + point(start, px(0.0)),
                                    size((end - start).max(px(0.0)), height),
                                ),
                                index,
                            });
                        }
                        ellipsis_x = first.x + f32::from(shaped.width);
                        painted.push(Painted::Text {
                            line: shaped,
                            origin,
                            line_height: height,
                            source_ranges,
                        });
                    }
                }
            }
            if clamp_line && !color_handles_clamp {
                let shaped = window.text_system().shape_line(
                    "…".into(),
                    px(self.metrics.font_size),
                    &[run("…", Flags::default(), &font, &self.theme)],
                    None,
                );
                let y = (line.y + (line.height - text_height) * 0.5)
                    .min((visible_height - text_height).max(0.0));
                painted.push(Painted::Text {
                    line: shaped,
                    origin: bounds.origin + point(px(ellipsis_x.min(cap)), px(y)),
                    line_height: px(text_height),
                    source_ranges: if self.observer.is_some() {
                        line.fragments
                            .last()
                            .map(|part| {
                                let source = match part {
                                    PlacedInline::Text(text) => text.source,
                                    PlacedInline::Object(object) => object.source,
                                };
                                vec![(source, 0.."…".len())]
                            })
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                });
            }
        }
        let signature = self.signature();
        let hovered = if let Some(id) = id {
            window.with_element_state::<HoverState, _>(id, |state, _| {
                let mut state = state.unwrap_or_default();
                if state.signature != signature {
                    state.signature = signature;
                    state.hovered.set(None);
                }
                (state.hovered.clone(), state)
            })
        } else {
            Rc::new(Cell::new(None))
        };
        let mut button = None;
        let mut button_bounds = None;
        if let (Some(index), Some(links)) = (hovered.get(), self.links.as_ref()) {
            if let (Some(area), Some(target)) = (
                areas
                    .iter()
                    .find(|area| area.index == index && bounds.intersects(&area.bounds)),
                links.targets.get(index),
            ) {
                let width = px(40.0).min(bounds.size.width);
                let x = area
                    .bounds
                    .left()
                    .min(bounds.right() - width)
                    .max(bounds.left());
                let height = area
                    .bounds
                    .size
                    .height
                    .max(px(18.0))
                    .min(bounds.size.height);
                let y = area
                    .bounds
                    .top()
                    .min(bounds.bottom() - height)
                    .max(bounds.top());
                let origin = point(x, y);
                let request = ContentOpenRequest {
                    item_id: links.item_id.clone(),
                    target: target.clone(),
                };
                let on_open = links.on_open.clone();
                let mut open = rsx! {
                    <div id={("inline-color-open", index)} w={width} h={height} flex items_center
                        justify_center rounded={px(3.0)} bg={self.theme.accent}
                        text_color={self.theme.surface} text_size={px(10.0)}
                        line_height={relative(1.0)} cursor_pointer />
                }
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        if event.click_count() <= 1 {
                            on_open(request.clone(), window, cx);
                        }
                    })
                    .child(tr("打开", "Open"))
                    .into_any_element();
                open.prepaint_as_root(
                    origin,
                    size(
                        AvailableSpace::Definite(width),
                        AvailableSpace::Definite(height),
                    ),
                    window,
                    cx,
                );
                button_bounds = Some(Bounds::new(origin, size(width, height)));
                button = Some(open);
            }
        }
        FlowPrepaint {
            painted,
            areas,
            hovered,
            button,
            button_bounds,
            bounds,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        state: &mut FlowPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(
            Some(ContentMask {
                bounds: state.bounds,
            }),
            |window| {
                for operation in &mut state.painted {
                    match operation {
                        Painted::Text {
                            line,
                            origin,
                            line_height,
                            source_ranges,
                        } => {
                            let _ = line.paint_background(*origin, *line_height, window, cx);
                            if line.paint(*origin, *line_height, window, cx).is_ok() {
                                if let Some(observer) = &self.observer {
                                    let clip =
                                        state.bounds.intersect(&window.content_mask().bounds);
                                    for (source, range) in source_ranges {
                                        observe_text(
                                            observer,
                                            *source,
                                            line,
                                            *origin,
                                            *line_height,
                                            range.clone(),
                                            clip,
                                        );
                                    }
                                }
                            }
                        }
                        Painted::Object(element) => element.paint(window, cx),
                    }
                }
                if let Some(button) = state.button.as_mut() {
                    button.paint(window, cx);
                }
            },
        );
        if self
            .links
            .as_ref()
            .is_some_and(|links| !links.targets.is_empty())
        {
            let areas = state.areas.clone();
            let hovered = state.hovered.clone();
            let button_bounds = state.button_bounds;
            let bounds = state.bounds;
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let updated =
                    if button_bounds.is_some_and(|bounds| bounds.contains(&event.position)) {
                        hovered.get()
                    } else if bounds.contains(&event.position) {
                        areas
                            .iter()
                            .find(|area| area.bounds.contains(&event.position))
                            .map(|area| area.index)
                    } else {
                        None
                    };
                if hovered.replace(updated) != updated {
                    window.refresh();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliphost_core::dto::HighlightHitRangeDto;

    #[test]
    fn colors_respect_link_and_hit_boundaries_and_keep_invalid_swatch() {
        let projection = TextProjection::new(
            "https://example.test/#abcdef rgb(garbage)",
            &[HighlightHitRangeDto { start: 21, end: 25 }],
        );
        let sources = text_sources(&projection);
        assert!(sources.iter().any(|source|matches!(source,FlowSource::Object{object:InlineObject::Swatch(token),flags:Flags{link:Some(_),..}} if token.css=="#abc")));
        assert!(sources.iter().any(|source|matches!(source,FlowSource::Object{object:InlineObject::Swatch(token),..} if token.css=="rgb(garbage)"&&token.color.is_none())));
    }

    #[test]
    fn pre_wrap_hard_breaks_and_spaces_survive_source_preparation() {
        let projection = TextProjection::new("  first\r\n\n#abc last", &[]);
        let sources = text_sources(&projection);
        assert_eq!(
            sources
                .iter()
                .filter(|source| matches!(source, FlowSource::Break))
                .count(),
            2
        );
        let FlowSource::Text { text, .. } = &sources[0] else {
            panic!()
        };
        assert_eq!(text, "  first");
        assert!(sources.iter().any(|source|matches!(source,FlowSource::Object{object:InlineObject::Swatch(token),..} if token.css=="#abc")));
    }

    #[test]
    fn image_bindings_keep_original_dto_indices_after_display_segments_are_filtered() {
        // DTO 0/2/4 are blank text; its display parts are text(1), image(3),
        // file(5), image(6). Neither image's display index addresses its DTO.
        let indices = [1, 3, 5, 6];
        let bindings = [1, 3].map(|part_index| original_segment_index(&indices, part_index));
        assert_eq!(bindings, [Some(3), Some(6)]);
        assert_eq!(original_segment_index(&indices, 4), None);
        assert_eq!(original_segment_index(&[], 1), None);
    }
}
