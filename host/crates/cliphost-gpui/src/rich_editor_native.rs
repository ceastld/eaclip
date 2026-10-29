//! Native rich canvas input/paint layer. All public ranges are UTF-8 bytes in
//! the parent document's projection; the OS input boundary alone uses UTF-16.
//! Source ownership and atomic source edits remain in the parent RichEditor.

use gpui_rsx::rsx;
use crate::{
    editor::text_history::{EditorTextHistory, TextEditKind, TextSnapshot},
    editor_html::{HtmlBlock, HtmlBlockKind, HtmlImageLayout, HtmlListKind, HtmlStyle},
    input::{floor_boundary, from_utf16, to_utf16, InputColors, TextState},
    preview::DesignTokens,
    view::color,
};
use gpui::{
    actions, div, fill, font, img, point, prelude::*, px, relative, size, AnyElement, App,
    AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    FontStyle, FontWeight, GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine,
    SharedString, StrikethroughStyle, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};
use unicode_segmentation::UnicodeSegmentation;

#[path = "rich_editor_image_layout.rs"]
mod image_layout;
use image_layout::{
    alignment_baseline, half_xheight, resolve_html_image_size, resolve_im_image_size,
    text_row_metrics, RowMetrics,
};

#[derive(Clone, Debug)]
pub(in crate::editor) struct CanvasFragment {
    pub range: Range<usize>,
    pub style: HtmlStyle,
}
#[derive(Clone, Debug)]
pub(in crate::editor) struct CanvasAtom {
    pub range: Range<usize>,
    pub kind: &'static str,
    pub html_image_layout: Option<HtmlImageLayout>,
    pub path: Option<PathBuf>,
    pub label: String,
}
#[derive(Clone, Debug)]
pub(in crate::editor) struct CanvasParagraph {
    pub range: Range<usize>,
    pub format: Option<HtmlBlock>,
}
#[derive(Clone, Debug)]
pub(in crate::editor) enum NativeCanvasEvent {
    Edited { range: Range<usize>, text: String },
    Paste,
    BoundaryKey { backspace: bool },
    SelectionChanged,
    Save,
    Escape,
}

actions!(
    clip_native_canvas,
    [
        Left,
        Right,
        SelectLeft,
        SelectRight,
        Up,
        Down,
        SelectUp,
        SelectDown,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocumentHome,
        DocumentEnd,
        SelectDocumentHome,
        SelectDocumentEnd,
        SelectAll,
        Backspace,
        Delete,
        Copy,
        Cut,
        Paste
    ]
);

/// This tracks native replacement coordinates, never guesses an edit by diffing
/// repeated characters. `original` belongs to the pre-composition projection;
/// `display` belongs to the mutable TextState containing the current preedit.
#[derive(Clone, Debug)]
struct Composition {
    original: Range<usize>,
    display: Range<usize>,
}
impl Composition {
    fn original_offset(&self, offset: usize, end: bool) -> usize {
        if offset <= self.display.start {
            offset
        } else if offset >= self.display.end {
            self.original.end + offset - self.display.end
        } else if end {
            self.original.end
        } else {
            self.original.start
        }
    }
    fn replacing(&mut self, range: &Range<usize>, inserted: usize) {
        let source =
            self.original_offset(range.start, false)..self.original_offset(range.end, true);
        self.original.start = self.original.start.min(source.start);
        self.original.end = self.original.end.max(source.end);
        let start = self.display.start.min(range.start);
        let old_end = self.display.end.max(range.end);
        self.display = start..old_end - (range.end - range.start) + inserted;
    }
    fn project(&self, range: &Range<usize>, atom: bool) -> Option<Range<usize>> {
        if atom && range.start < self.original.end && range.end > self.original.start {
            return None;
        }
        if atom && range.start >= self.original.end {
            return Some(
                self.display.end + range.start - self.original.end
                    ..self.display.end + range.end - self.original.end,
            );
        }
        let map = |offset: usize, end: bool| {
            if offset <= self.original.start {
                offset
            } else if offset >= self.original.end {
                self.display.end + offset - self.original.end
            } else if end {
                self.display.end
            } else {
                self.display.start
            }
        };
        Some(map(range.start, false)..map(range.end, true))
    }
}

#[derive(Clone)]
struct CanvasRow {
    range: Range<usize>,
    pieces: Range<usize>,
    top: Pixels,
    height: Pixels,
    left: Pixels,
    right: Pixels,
    format: Option<HtmlBlock>,
    marker: Option<ShapedLine>,
    baseline: Pixels,
}

#[derive(Clone)]
struct ResolvedImage {
    image: Arc<gpui::RenderImage>,
    natural: gpui::Size<f32>,
}
#[derive(Clone)]
enum Piece {
    Text {
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        line: ShapedLine,
        line_height: Pixels,
        paint_offset: Pixels,
    },
    Atom {
        index: usize,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
    },
}
impl Piece {
    fn range(&self) -> &Range<usize> {
        match self {
            Self::Text { range, .. } | Self::Atom { range, .. } => range,
        }
    }
    fn bounds(&self) -> Bounds<Pixels> {
        match self {
            Self::Text { bounds, .. } | Self::Atom { bounds, .. } => *bounds,
        }
    }
    fn bounds_mut(&mut self) -> &mut Bounds<Pixels> {
        match self {
            Self::Text { bounds, .. } | Self::Atom { bounds, .. } => bounds,
        }
    }
}
#[derive(Clone, Default)]
struct CanvasLayout {
    rows: Vec<CanvasRow>,
    pieces: Vec<Piece>,
    height: Pixels,
    width: Pixels,
}
impl CanvasLayout {
    fn atom_at(&self, position: Point<Pixels>) -> Option<Range<usize>> {
        self.pieces.iter().find_map(|piece| match piece {
            Piece::Atom { range, bounds, .. } if bounds.contains(&position) => Some(range.clone()),
            _ => None,
        })
    }
    fn row_for(&self, offset: usize) -> Option<&CanvasRow> {
        let index = self
            .rows
            .partition_point(|row| row.range.start <= offset)
            .saturating_sub(1);
        self.rows.get(index)
    }
    fn position(&self, offset: usize) -> (Point<Pixels>, Pixels) {
        let Some(row) = self.row_for(offset) else {
            return (point(px(0.), px(0.)), px(20.));
        };
        self.position_in_row(offset, row)
    }
    fn position_in_row(&self, offset: usize, row: &CanvasRow) -> (Point<Pixels>, Pixels) {
        for piece in self.pieces[row.pieces.clone()].iter().rev() {
            let bounds = piece.bounds();
            let range = piece.range();
            if offset < range.start || offset > range.end {
                continue;
            }
            let x = match piece {
                Piece::Text { line, .. } if offset < range.end => {
                    bounds.left() + line.x_for_index(offset - range.start)
                }
                Piece::Text { .. } => bounds.right(),
                Piece::Atom { .. } if offset <= range.start => bounds.left(),
                Piece::Atom { .. } => bounds.right(),
            };
            return (point(x, row.top), row.height);
        }
        (
            point(
                if offset == row.range.start {
                    row.left
                } else {
                    row.right
                },
                row.top,
            ),
            row.height,
        )
    }
    fn index_at(&self, position: Point<Pixels>) -> usize {
        let index = self
            .rows
            .partition_point(|row| row.top + row.height <= position.y);
        let Some(row) = self.rows.get(index).or_else(|| self.rows.last()) else {
            return 0;
        };
        let mut nearest = (row.range.start, (position.x - row.left).abs());
        for piece in &self.pieces[row.pieces.clone()] {
            let bounds = piece.bounds();
            if position.x >= bounds.left() && position.x <= bounds.right() {
                return match piece {
                    Piece::Text { range, line, .. } => {
                        range.start
                            + line
                                .closest_index_for_x(position.x - bounds.left())
                                .min(range.end - range.start)
                    }
                    Piece::Atom { range, .. } => {
                        if position.x < bounds.left() + bounds.size.width / 2. {
                            range.start
                        } else {
                            range.end
                        }
                    }
                };
            }
            for (offset, x) in [
                (piece.range().start, bounds.left()),
                (piece.range().end, bounds.right()),
            ] {
                let distance = (position.x - x).abs();
                if distance < nearest.1 {
                    nearest = (offset, distance);
                }
            }
        }
        nearest.0
    }
}

fn adjacent_offset(
    state: &TextState,
    atoms: impl Iterator<Item = Range<usize>>,
    forward: bool,
) -> usize {
    let cursor = state.cursor();
    let mut candidate = if forward {
        state.next()
    } else {
        state.previous()
    };
    for range in atoms {
        for edge in [range.start, range.end] {
            if forward && edge > cursor && edge < candidate {
                candidate = edge;
            } else if !forward && edge < cursor && edge > candidate {
                candidate = edge;
            }
        }
    }
    candidate
}

fn effective_font_marks(style: Option<&HtmlStyle>, heading: bool, code: bool) -> (bool, bool) {
    style.map_or((heading, code), |style| (style.bold, style.code))
}

fn editable_limit(text: &str, terminal_newline: bool) -> usize {
    if terminal_newline && text.ends_with('\n') {
        text.len() - 1
    } else {
        text.len()
    }
}

fn atom_display_label(label: &str) -> String {
    label.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

fn plain_field_text(text: &str, single_line: bool) -> String {
    if single_line {
        text.chars()
            .map(|ch| if matches!(ch, '\r' | '\n') { ' ' } else { ch })
            .collect()
    } else {
        text.to_string()
    }
}

fn snapshot_from_state(state: &TextState) -> TextSnapshot {
    TextSnapshot {
        text: state.text.clone(),
        selection: state.selection.clone(),
        reversed: state.reversed,
    }
}

fn restore_plain_state(state: &mut TextState, snapshot: TextSnapshot) {
    state.text = snapshot.text;
    state.marked = None;
    state.move_to(if snapshot.reversed {
        snapshot.selection.end
    } else {
        snapshot.selection.start
    });
    state.select_to(if snapshot.reversed {
        snapshot.selection.start
    } else {
        snapshot.selection.end
    });
}

#[derive(Clone)]
struct CanvasPaintStamp {
    frame_id: u64,
    version: u64,
    bounds: Bounds<Pixels>,
    visible_bounds: Bounds<Pixels>,
    scroll_y: Pixels,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    read_only: bool,
    scale_factor: f32,
}

pub(in crate::editor) struct NativeCanvas {
    state: TextState,
    focus: FocusHandle,
    tokens: DesignTokens,
    colors: InputColors,
    code_background: gpui::Hsla,
    fragments: Vec<CanvasFragment>,
    atoms: Vec<CanvasAtom>,
    image_cache: Entity<gpui::RetainAllImageCache>,
    image_sources: HashSet<PathBuf>,
    resolved_images: HashMap<PathBuf, ResolvedImage>,
    paragraphs: Vec<CanvasParagraph>,
    composition: Option<Composition>,
    committed_text: String,
    layout: Option<CanvasLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_y: Pixels,
    reveal_caret: bool,
    selecting: bool,
    height: f32,
    fill_height: bool,
    padding_x: f32,
    padding_y: f32,
    read_only: bool,
    terminal_newline: bool,
    field_history: Option<EditorTextHistory>,
    field_before: Option<TextSnapshot>,
    single_line: bool,
    monospace: bool,
    placeholder: Option<SharedString>,
    next_field_edit: TextEditKind,
    last_focused: bool,
    render_version: u64,
    paint_clock: Option<Arc<AtomicU64>>,
    last_paint: Option<CanvasPaintStamp>,
    local_frame: u64,
}
impl EventEmitter<NativeCanvasEvent> for NativeCanvas {}
impl NativeCanvas {
    pub(in crate::editor) fn new(
        tokens: DesignTokens,
        dark: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let context = Some("ClipNativeCanvas");
        cx.bind_keys([
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            KeyBinding::new("up", Up, context),
            KeyBinding::new("down", Down, context),
            KeyBinding::new("shift-up", SelectUp, context),
            KeyBinding::new("shift-down", SelectDown, context),
            KeyBinding::new("home", Home, context),
            KeyBinding::new("end", End, context),
            KeyBinding::new("shift-home", SelectHome, context),
            KeyBinding::new("shift-end", SelectEnd, context),
            KeyBinding::new("ctrl-home", DocumentHome, context),
            KeyBinding::new("ctrl-end", DocumentEnd, context),
            KeyBinding::new("ctrl-shift-home", SelectDocumentHome, context),
            KeyBinding::new("ctrl-shift-end", SelectDocumentEnd, context),
            KeyBinding::new("ctrl-a", SelectAll, context),
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("ctrl-c", Copy, context),
            KeyBinding::new("ctrl-x", Cut, context),
            KeyBinding::new("ctrl-v", Paste, context),
        ]);
        let theme = tokens.theme(dark);
        let colors = InputColors {
            background: color(&theme.search_background),
            foreground: color(&theme.text),
            placeholder: color(&theme.text_muted),
            border: color(&theme.border),
            selection: color(&theme.selected),
            caret: color(&theme.accent),
        };
        let code_background = color(&theme.secondary);
        Self {
            state: TextState::default(),
            focus: cx.focus_handle(),
            tokens,
            colors,
            code_background,
            fragments: Vec::new(),
            atoms: Vec::new(),
            image_cache: gpui::RetainAllImageCache::new(cx),
            image_sources: HashSet::new(),
            resolved_images: HashMap::new(),
            paragraphs: Vec::new(),
            composition: None,
            committed_text: String::new(),
            layout: None,
            last_bounds: None,
            scroll_y: px(0.),
            reveal_caret: true,
            selecting: false,
            height: 320.,
            fill_height: false,
            padding_x: 8.,
            padding_y: 6.,
            read_only: false,
            terminal_newline: false,
            field_history: None,
            field_before: None,
            single_line: false,
            monospace: false,
            placeholder: None,
            next_field_edit: TextEditKind::Typing,
            last_focused: false,
            render_version: 0,
            paint_clock: None,
            last_paint: None,
            local_frame: 0,
        }
    }
    pub(in crate::editor) fn new_plain(
        tokens: DesignTokens,
        dark: bool,
        single_line: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = Self::new(tokens, dark, window, cx);
        input.single_line = single_line;
        input.padding_y = if single_line { 3. } else { 6. };
        input.height = if single_line { 30. } else { 300. };
        let mut history = EditorTextHistory::default();
        history.reset(input.field_snapshot());
        input.field_history = Some(history);
        input
    }
    fn field_snapshot(&self) -> TextSnapshot {
        snapshot_from_state(&self.state)
    }
    fn field_text(&self, text: &str) -> String {
        plain_field_text(text, self.single_line)
    }
    fn restore_field(&mut self, snapshot: TextSnapshot, cx: &mut Context<Self>) {
        restore_plain_state(&mut self.state, snapshot);
        self.composition = None;
        self.field_before = None;
        self.fragments.clear();
        self.atoms.clear();
        self.paragraphs.clear();
        self.committed_text.clone_from(&self.state.text);
        self.invalidate_layout();
        self.set_selection(self.state.selection.clone(), self.state.reversed, cx);
    }
    pub(in crate::editor) fn reset_text(
        &mut self,
        text: String,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_composing() {
            return;
        }
        let text = self.field_text(&text);
        let caret = if self.single_line { text.len() } else { 0 };
        let snapshot = TextSnapshot {
            text,
            selection: caret..caret,
            reversed: false,
        };
        if let Some(history) = &mut self.field_history {
            history.reset(snapshot.clone());
        }
        self.scroll_y = px(0.);
        self.restore_field(snapshot, cx);
    }
    /// A same-field external value update is quiet and remains undoable; callers
    /// reset_text when the document identity or mounted edit surface changes.
    pub(in crate::editor) fn set_text(
        &mut self,
        text: String,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_composing() {
            return;
        }
        let text = self.field_text(&text);
        if text == self.state.text {
            return;
        }
        let before = self.field_snapshot();
        let after = TextSnapshot {
            selection: floor_boundary(&text, before.selection.start.min(text.len()))
                ..floor_boundary(&text, before.selection.end.min(text.len())),
            reversed: before.reversed,
            text,
        };
        if let Some(history) = &mut self.field_history {
            history.record(
                before.clone(),
                after.clone(),
                0..before.text.len(),
                &after.text,
                TextEditKind::External,
                Instant::now(),
            );
        }
        self.restore_field(after, cx);
    }
    pub(in crate::editor) fn set_colors(&mut self, colors: InputColors, cx: &mut Context<Self>) {
        self.set_theme_colors(colors, self.code_background, cx);
    }
    pub(in crate::editor) fn set_theme_colors(
        &mut self,
        colors: InputColors,
        code_background: gpui::Hsla,
        cx: &mut Context<Self>,
    ) {
        self.colors = colors;
        self.code_background = code_background;
        self.invalidate_layout();
        cx.notify();
    }
    pub(in crate::editor) fn set_metrics(
        &mut self,
        height: f32,
        font_size: f32,
        _: f32,
        cx: &mut Context<Self>,
    ) {
        self.height = height.max(1.);
        self.tokens.typography.font_size = font_size.max(1.);
        self.invalidate_layout();
        cx.notify();
    }
    pub(in crate::editor) fn set_monospace(&mut self, value: bool, cx: &mut Context<Self>) {
        self.monospace = value;
        self.invalidate_layout();
        cx.notify();
    }
    pub(in crate::editor) fn set_placeholder(
        &mut self,
        value: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = Some(value.into());
        cx.notify();
    }
    pub(in crate::editor) fn text(&self) -> &str {
        &self.state.text
    }
    pub(in crate::editor) fn set_paint_clock(&mut self, clock: Arc<AtomicU64>) {
        self.paint_clock = Some(clock);
        self.last_paint = None;
    }
    fn invalidate_layout(&mut self) {
        self.render_version = self.render_version.wrapping_add(1);
        self.layout = None;
    }
    pub(in crate::editor) fn paint_is_current(&self, frame_id: u64) -> bool {
        self.last_paint.as_ref().is_some_and(|paint| {
            frame_id != 0
                && paint.frame_id == frame_id
                && self.layout.is_some()
                && paint.version == self.render_version
                && self.last_bounds == Some(paint.bounds)
                && paint.scroll_y == self.scroll_y
                && paint.selection == self.state.selection
                && paint.reversed == self.state.reversed
                && paint.marked == self.state.marked
                && paint.read_only == self.read_only
        })
    }
    pub(in crate::editor) fn selection(&self) -> (Range<usize>, bool) {
        (self.state.selection.clone(), self.state.reversed)
    }
    /// Inspect only metadata from the most recently painted layout. This never
    /// exposes input text, object paths, labels, or document source.
    pub(in crate::editor) fn diagnostics(&self) -> serde_json::Value {
        let viewport = self.last_bounds;
        let rect = |bounds: Bounds<Pixels>| {
            serde_json::json!({
                "x": f32::from(bounds.left()), "y": f32::from(bounds.top()),
                "width": f32::from(bounds.size.width), "height": f32::from(bounds.size.height),
            })
        };
        let caret = viewport.and_then(|bounds| {
            self.layout.as_ref().map(|layout| {
                let (position, height) = layout.position(self.state.cursor());
                rect(Bounds::new(
                    point(
                        bounds.left() + position.x,
                        bounds.top() + position.y - self.scroll_y,
                    ),
                    size(px(1.), height),
                ))
            })
        });
        let objects = viewport.zip(self.layout.as_ref()).map(|(viewport,layout)| {
            layout.pieces.iter().filter_map(|piece| {
                let Piece::Atom {index, range, bounds} = piece else {return None;};
                let visible = Bounds::new(point(viewport.left()+bounds.left(),viewport.top()+bounds.top()-self.scroll_y),bounds.size);
                Some(serde_json::json!({
                    "index":index,"kind":self.atoms.get(*index).map(|atom|atom.kind),
                    "rangeBytes":[range.start,range.end],
                    "rangeUtf16": {"start":to_utf16(&self.state.text,range.clone()).start,"end":to_utf16(&self.state.text,range.clone()).end},
                    "bounds":rect(visible),
                    "visible":self.last_paint.as_ref().is_some_and(|paint|visible.right()>paint.visible_bounds.left()&&visible.left()<paint.visible_bounds.right()&&visible.bottom()>paint.visible_bounds.top()&&visible.top()<paint.visible_bounds.bottom()),
                }))
            }).collect::<Vec<_>>()
        }).unwrap_or_default();
        let selection_utf16 = to_utf16(&self.state.text, self.state.selection.clone());
        let marked = self
            .state
            .marked
            .as_ref()
            .map(|range| to_utf16(&self.state.text, range.clone()));
        let frame_id = self
            .last_paint
            .as_ref()
            .map(|paint| paint.frame_id)
            .unwrap_or(0);
        let mut caret_stops = Vec::new();
        if self.paint_is_current(frame_id) {
            if let Some((viewport, layout)) = viewport.zip(self.layout.as_ref()) {
                let visible = self
                    .last_paint
                    .as_ref()
                    .expect("current paint")
                    .visible_bounds;
                let mut utf16 = 0;
                for (offset, ch) in self
                    .state
                    .text
                    .char_indices()
                    .map(|(offset, ch)| (offset, Some(ch)))
                    .chain(std::iter::once((self.state.text.len(), None)))
                {
                    if offset <= self.max_offset() && self.snap_offset(offset, false) == offset {
                        let (position, height) = layout.position(offset);
                        let x = viewport.left() + position.x;
                        let top = viewport.top() + position.y - self.scroll_y;
                        let y = (top + height / 2.)
                            .max(visible.top())
                            .min(visible.bottom() - px(1.));
                        let mut click = point(x, y);
                        let mut hit_target = "caret";
                        let hits = |click: Point<Pixels>| {
                            visible.size.width > px(0.)
                                && visible.size.height > px(0.)
                                && click.x >= visible.left()
                                && click.x < visible.right()
                                && click.y >= visible.top()
                                && click.y < visible.bottom()
                                && top + height > visible.top()
                                && top < visible.bottom()
                                && self.index_at(click) == offset
                        };
                        if !hits(click) {
                            // A preceding zero-width glyph may own the exact
                            // object edge. Use a real hit in its left/right half.
                            if let Some((range, bounds)) =
                                layout.pieces.iter().find_map(|piece| match piece {
                                    Piece::Atom { range, bounds, .. }
                                        if offset == range.start || offset == range.end =>
                                    {
                                        Some((range, bounds))
                                    }
                                    _ => None,
                                })
                            {
                                let object = Bounds::new(
                                    point(
                                        viewport.left() + bounds.left(),
                                        viewport.top() + bounds.top() - self.scroll_y,
                                    ),
                                    bounds.size,
                                );
                                let clipped = object.intersect(&visible);
                                if clipped.size.width > px(0.) && clipped.size.height > px(0.) {
                                    click = point(
                                        (object.left()
                                            + object.size.width
                                                * if offset == range.start { 0.25 } else { 0.75 })
                                        .max(clipped.left())
                                        .min(clipped.right() - px(0.5)),
                                        (object.top() + object.size.height / 2.)
                                            .max(clipped.top())
                                            .min(clipped.bottom() - px(0.5)),
                                    );
                                    hit_target = "object-interior";
                                }
                            }
                        }
                        if hits(click) {
                            caret_stops.push(serde_json::json!({"utf16":utf16,"byte":offset,"x":f32::from(click.x),"y":f32::from(click.y),"hitTarget":hit_target}));
                        }
                    }
                    if let Some(ch) = ch {
                        utf16 += ch.len_utf16();
                    }
                }
            }
        }
        serde_json::json!({
            "coordinateSpace":"window-client-logical-pixels",
            "inputMode":if self.field_history.is_some() {if self.single_line {"single-line"} else {"multiline"}} else {"rich-canvas"},
            "textLengthBytes":self.state.text.len(),"textLengthUtf16":self.state.text.encode_utf16().count(),
            "selectionBytes":[self.state.selection.start,self.state.selection.end],
            "selectionUtf16":[selection_utf16.start,selection_utf16.end],"selectionReversed":self.state.reversed,
            "composing":self.is_composing(),"markedRangeUtf16":marked.map(|range|[range.start,range.end]),
            "readOnly":self.read_only,"focusObserved":self.last_focused,
            "layoutReady":self.layout.is_some(),"bounds":viewport.map(rect),"caretBounds":caret,"objects":objects,
            "frameId":frame_id,"paintCompleted":self.paint_is_current(frame_id),
            "scaleFactor":self.last_paint.as_ref().map(|paint|paint.scale_factor),
            "visibleBounds":self.last_paint.as_ref().map(|paint|rect(paint.visible_bounds)),
            "caretStops":caret_stops,"caretStopCoordinates":"caret-column-or-object-interior",
            "scrollY":f32::from(self.scroll_y),"terminalNewline":self.terminal_newline,
            "history":self.field_history.as_ref().map(|history|serde_json::json!({
                "canUndo":history.can_undo(),"canRedo":history.can_redo(),
                "undoDepth":history.undo_depth(),"redoDepth":history.redo_depth(),
            })),
        })
    }
    pub(in crate::editor) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    pub(in crate::editor) fn is_read_only(&self) -> bool {
        self.read_only
    }
    pub(in crate::editor) fn is_composing(&self) -> bool {
        self.composition.is_some() || self.state.marked.is_some()
    }
    pub(in crate::editor) fn set_terminal_newline(&mut self, value: bool, cx: &mut Context<Self>) {
        self.terminal_newline = value;
        self.invalidate_layout();
        cx.notify();
    }
    fn max_offset(&self) -> usize {
        editable_limit(&self.state.text, self.terminal_newline)
    }
    pub(in crate::editor) fn set_height(&mut self, height: f32, cx: &mut Context<Self>) {
        let height = height.max(if self.single_line { 24. } else { 80. });
        if self.height != height {
            self.height = height;
            cx.notify();
        }
    }
    /// Layout only: selection, undo history, scroll and IME preedit stay intact.
    pub(in crate::editor) fn set_fill_height(&mut self, fill: bool, cx: &mut Context<Self>) {
        if self.fill_height != fill {
            self.fill_height = fill;
            cx.notify();
        }
    }
    pub(in crate::editor) fn set_single_line_height(&mut self, height: f32, cx: &mut Context<Self>) {
        if self.single_line { self.set_height(height, cx); }
    }
    pub(in crate::editor) fn set_padding(&mut self, horizontal: f32, vertical: f32, cx: &mut Context<Self>) {
        let (horizontal, vertical) = (horizontal.max(0.), vertical.max(0.));
        if self.padding_x != horizontal || self.padding_y != vertical {
            self.padding_x = horizontal;
            self.padding_y = vertical;
            self.invalidate_layout();
            cx.notify();
        }
    }
    pub(in crate::editor) fn set_read_only(&mut self, value: bool, cx: &mut Context<Self>) {
        self.read_only = value;
        cx.notify();
    }
    pub(in crate::editor) fn set_selection(
        &mut self,
        selection: Range<usize>,
        reversed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.is_composing() {
            return;
        }
        let start = self.snap_offset(
            floor_boundary(&self.state.text, selection.start.min(self.max_offset())),
            false,
        );
        let end = self.snap_offset(
            floor_boundary(&self.state.text, selection.end.min(self.max_offset())),
            true,
        );
        self.state.move_to(if reversed { end } else { start });
        self.state.select_to(if reversed { start } else { end });
        self.reveal_caret = true;
        cx.notify();
    }
    /// Quiet canonical acknowledgment; does not focus, emit Edited, reset to
    /// offset zero, or replace a live IME preedit. Selection is the real canvas
    /// selection supplied by the parent after its source edit/undo.
    pub(in crate::editor) fn set_document(
        &mut self,
        text: String,
        fragments: Vec<CanvasFragment>,
        atoms: Vec<CanvasAtom>,
        paragraphs: Vec<CanvasParagraph>,
        selection: Range<usize>,
        reversed: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_document(text, fragments, atoms, paragraphs, selection, reversed, cx);
    }
    pub(in crate::editor) fn sync_document(
        &mut self,
        text: String,
        fragments: Vec<CanvasFragment>,
        atoms: Vec<CanvasAtom>,
        paragraphs: Vec<CanvasParagraph>,
        selection: Range<usize>,
        reversed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.is_composing() {
            return;
        }
        self.state.text = text;
        self.committed_text.clone_from(&self.state.text);
        self.fragments = fragments;
        self.atoms = atoms;
        self.paragraphs = paragraphs;
        self.atoms.sort_by_key(|atom| atom.range.start);
        self.fragments.sort_by_key(|fragment| fragment.range.start);
        self.paragraphs
            .sort_by_key(|paragraph| paragraph.range.start);
        self.state.marked = None;
        self.set_selection(selection, reversed, cx);
        self.invalidate_layout();
        self.reveal_caret = true;
        cx.notify();
    }
    fn displayed_range(&self, range: &Range<usize>, atom: bool) -> Option<Range<usize>> {
        match &self.composition {
            Some(composition) => composition.project(range, atom),
            None => Some(range.clone()),
        }
    }
    fn snap_offset(&self, offset: usize, end: bool) -> usize {
        for atom in &self.atoms {
            if let Some(range) = self.displayed_range(&atom.range, true) {
                if offset > range.start && offset < range.end {
                    return if end { range.end } else { range.start };
                }
            }
        }
        offset.min(self.max_offset())
    }
    fn atomic_range(&self, range: Range<usize>) -> Range<usize> {
        let range = range.start.min(self.max_offset())..range.end.min(self.max_offset());
        if range.is_empty() {
            let start = self.snap_offset(range.start, false);
            return start..start;
        }
        self.snap_offset(range.start, false)..self.snap_offset(range.end, true)
    }
    fn previous(&self) -> usize {
        self.snap_offset(
            adjacent_offset(
                &self.state,
                self.atoms
                    .iter()
                    .filter_map(|atom| self.displayed_range(&atom.range, true)),
                false,
            ),
            false,
        )
    }
    fn next(&self) -> usize {
        self.snap_offset(
            adjacent_offset(
                &self.state,
                self.atoms
                    .iter()
                    .filter_map(|atom| self.displayed_range(&atom.range, true)),
                true,
            ),
            true,
        )
    }
    fn selection_changed(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.field_snapshot();
        if let Some(history) = &mut self.field_history {
            history.selection_changed(snapshot);
        }
        self.reveal_caret = true;
        cx.emit(NativeCanvasEvent::SelectionChanged);
        cx.notify();
    }
    fn navigate(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        if self.is_composing() {
            return;
        }
        let offset = floor_boundary(&self.state.text, offset.min(self.max_offset()));
        if select {
            self.state.select_to(offset);
        } else {
            self.state.move_to(offset);
        }
        self.selection_changed(cx);
    }
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.state.selection.is_empty() {
            self.previous()
        } else {
            self.state.selection.start
        };
        self.navigate(offset, false, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.state.selection.is_empty() {
            self.next()
        } else {
            self.state.selection.end
        };
        self.navigate(offset, false, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(self.previous(), true, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(self.next(), true, cx);
    }
    fn vertical(&mut self, direction: isize, select: bool, cx: &mut Context<Self>) {
        if self.is_composing() {
            return;
        }
        let Some(layout) = &self.layout else {
            return;
        };
        let (position, _) = layout.position(self.state.cursor());
        let row_index = layout
            .rows
            .iter()
            .position(|row| row.top == position.y)
            .unwrap_or(0);
        let target = (row_index as isize + direction)
            .clamp(0, layout.rows.len().saturating_sub(1) as isize) as usize;
        if let Some(row) = layout.rows.get(target) {
            let offset = layout.index_at(point(position.x, row.top + row.height / 2.));
            self.navigate(offset, select, cx);
        }
    }
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, false, cx);
    }
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, false, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, cx);
    }
    fn edge(&mut self, end: bool, select: bool, cx: &mut Context<Self>) {
        if let Some(row) = self
            .layout
            .as_ref()
            .and_then(|layout| layout.row_for(self.state.cursor()))
        {
            let offset = if end { row.range.end } else { row.range.start };
            self.navigate(offset, select, cx);
        }
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.edge(false, false, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.edge(true, false, cx);
    }
    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.edge(false, true, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.edge(true, true, cx);
    }
    fn document_home(&mut self, _: &DocumentHome, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(0, false, cx);
    }
    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(self.max_offset(), false, cx);
    }
    fn select_document_home(
        &mut self,
        _: &SelectDocumentHome,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(0, true, cx);
    }
    fn select_document_end(
        &mut self,
        _: &SelectDocumentEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(self.max_offset(), true, cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() {
            self.state.move_to(0);
            self.navigate(self.max_offset(), true, cx);
        }
    }
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only {
            if self.field_history.is_some() {
                self.field_before = Some(self.field_snapshot());
            }
            if self.state.selection.is_empty() {
                let cursor = self.state.cursor();
                if self.terminal_newline
                    && self.paragraphs.iter().any(|paragraph| {
                        paragraph.range.start == cursor
                            && paragraph
                                .format
                                .as_ref()
                                .is_some_and(|format| format.indent > 0 || format.list.is_some())
                    })
                {
                    cx.emit(NativeCanvasEvent::BoundaryKey { backspace: true });
                    return;
                }
                self.state.select_to(self.previous());
            }
            self.replace_text_in_range(None, "", window, cx);
        }
    }
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only {
            if self.field_history.is_some() {
                self.field_before = Some(self.field_snapshot());
            }
            if self.state.selection.is_empty() {
                self.state.select_to(self.next());
            }
            self.replace_text_in_range(None, "", window, cx);
        }
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.state.selection.clone();
        if range.is_empty() {
            return;
        }
        let mut result = String::new();
        let mut cursor = range.start;
        for atom in &self.atoms {
            if let Some(atom_range) = self.displayed_range(&atom.range, true) {
                if atom_range.start >= cursor && atom_range.end <= range.end {
                    result.push_str(&self.state.text[cursor..atom_range.start]);
                    result.push_str(&atom.label);
                    cursor = atom_range.end;
                }
            }
        }
        result.push_str(&self.state.text[cursor..range.end]);
        cx.write_to_clipboard(ClipboardItem::new_string(result));
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only && !self.state.selection.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only {
            if self.field_history.is_none() {
                // Rich paste needs the coherent HTML/plain payload and its
                // source transaction, owned by the parent RichEditor.
                cx.emit(NativeCanvasEvent::Paste);
                return;
            }
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.next_field_edit = TextEditKind::Paste;
                self.replace_text_in_range(None, &text, window, cx);
            }
        }
    }
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if key == "tab"
            && self.terminal_newline
            && self.field_history.is_none()
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
        {
            // The rich parent resolves Quill's context before consuming this
            // key. An unmatched Shift+Tab must remain available to focus logic.
            if self.is_composing() || event.is_held {
                cx.stop_propagation();
            }
            return;
        }
        if self.field_history.is_some()
            && modifiers.control
            && !modifiers.alt
            && !modifiers.platform
        {
            let redo = if key == "z" && !modifiers.shift {
                Some(false)
            } else if key == "y" && !modifiers.shift {
                Some(true)
            } else if key == "z" && modifiers.shift && !cfg!(windows) {
                Some(true)
            } else {
                None
            };
            if let Some(redo) = redo {
                cx.stop_propagation();
                if !event.is_held {
                    self.field_history_step(redo, cx);
                }
                return;
            }
        }
        if event.keystroke.modifiers.control && key == "s" {
            cx.stop_propagation();
            if !event.is_held && !self.is_composing() {
                cx.emit(NativeCanvasEvent::Save);
            }
        } else if (matches!(key, "enter" | "escape") || key == "tab" && !self.single_line)
            && event.keystroke.modifiers.number_of_modifiers() == 0
        {
            cx.stop_propagation();
            if event.is_held || self.is_composing() {
                return;
            }
            match key {
                "enter" if !self.read_only && self.single_line => cx.emit(NativeCanvasEvent::Save),
                "enter" if !self.read_only => {
                    let cursor = self.state.cursor();
                    let special = self.terminal_newline
                        && self.state.selection.is_empty()
                        && self.paragraphs.iter().any(|paragraph| {
                            let line_end = paragraph.range.end.saturating_sub(1);
                            paragraph.range.start <= cursor
                                && cursor <= line_end
                                && paragraph.format.as_ref().is_some_and(|format| {
                                    matches!(format.kind, HtmlBlockKind::Header(_))
                                        && cursor == line_end
                                        || format.list.is_some()
                                            && paragraph.range.start == line_end
                                })
                        });
                    if special {
                        cx.emit(NativeCanvasEvent::BoundaryKey { backspace: false });
                    } else {
                        self.replace_text_in_range(None, "\n", window, cx);
                    }
                }
                "tab" if !self.read_only && !self.single_line => {
                    self.replace_text_in_range(None, "\t", window, cx)
                }
                "escape" => cx.emit(NativeCanvasEvent::Escape),
                _ => {}
            }
        }
    }
    fn index_at(&self, position: Point<Pixels>) -> usize {
        let (Some(layout), Some(bounds)) = (&self.layout, self.last_bounds) else {
            return 0;
        };
        let offset = layout.index_at(point(
            position.x - bounds.left(),
            position.y - bounds.top() + self.scroll_y,
        ));
        self.snap_offset(
            floor_boundary(&self.state.text, offset.min(self.max_offset())),
            false,
        )
    }
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        cx.stop_propagation();
        if self.is_composing() {
            return;
        }
        let offset = self.index_at(event.position);
        self.selecting = true;
        if event.modifiers.shift {
            self.state.select_to(offset);
        } else {
            self.state.move_to(offset);
        }
        if event.click_count >= 2 {
            let hit = self.last_bounds.and_then(|bounds| {
                self.layout.as_ref().and_then(|layout| {
                    let position = point(
                        event.position.x - bounds.left(),
                        event.position.y - bounds.top() + self.scroll_y,
                    );
                    layout.atom_at(position)
                })
            });
            if let Some(range) = hit {
                self.state.move_to(range.start);
                self.state.select_to(range.end);
            } else {
                let mut start = offset;
                let mut end = offset;
                while start > 0 {
                    let (index, ch) = self.state.text[..start].char_indices().next_back().unwrap();
                    if ch.is_whitespace()
                        || self
                            .atoms
                            .iter()
                            .any(|atom| index >= atom.range.start && index < atom.range.end)
                    {
                        break;
                    }
                    start = index;
                }
                while let Some(ch) = self.state.text[end..].chars().next() {
                    if ch.is_whitespace()
                        || self
                            .atoms
                            .iter()
                            .any(|atom| end >= atom.range.start && end < atom.range.end)
                    {
                        break;
                    }
                    end += ch.len_utf8();
                }
                self.state.move_to(start);
                self.state.select_to(end);
            }
            self.selecting = false;
        }
        self.selection_changed(cx);
    }
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting && event.pressed_button == Some(MouseButton::Left) && !self.is_composing()
        {
            if let Some(bounds) = self.last_bounds {
                if event.position.y < bounds.top() {
                    self.scroll_y = (self.scroll_y - px(16.)).max(px(0.));
                } else if event.position.y > bounds.bottom() {
                    self.scroll_y += px(16.);
                }
            }
            self.state.select_to(self.index_at(event.position));
            self.selection_changed(cx);
        }
    }
    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }
    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = &self.layout else {
            return;
        };
        let height = self
            .last_bounds
            .map(|bounds| bounds.size.height)
            .unwrap_or(px(self.height));
        self.scroll_y = (self.scroll_y
            - event
                .delta
                .pixel_delta(px(self.tokens.typography.font_size + 6.))
                .y)
            .max(px(0.))
            .min((layout.height - height).max(px(0.)));
        self.reveal_caret = false;
        cx.stop_propagation();
        cx.notify();
    }
    /// Advance all presentation ranges using the same exact accepted replacement.
    /// Parent canonical ack replaces them; no source reconstruction happens here.
    fn shift_model(&mut self, range: &Range<usize>, inserted: usize) {
        let projection = Composition {
            original: range.clone(),
            display: range.start..range.start + inserted,
        };
        self.fragments = self
            .fragments
            .iter()
            .filter_map(|fragment| {
                projection
                    .project(&fragment.range, false)
                    .map(|range| CanvasFragment {
                        range,
                        style: fragment.style.clone(),
                    })
            })
            .collect();
        self.atoms = self
            .atoms
            .iter()
            .filter_map(|atom| {
                projection
                    .project(&atom.range, true)
                    .map(|range| CanvasAtom {
                        range,
                        kind: atom.kind,
                        html_image_layout: atom.html_image_layout,
                        path: atom.path.clone(),
                        label: atom.label.clone(),
                    })
            })
            .collect();
        self.paragraphs = self
            .paragraphs
            .iter()
            .filter_map(|paragraph| {
                projection
                    .project(&paragraph.range, false)
                    .map(|range| CanvasParagraph {
                        range,
                        format: paragraph.format.clone(),
                    })
            })
            .collect();
    }
    fn emit_edit(
        &mut self,
        range: Range<usize>,
        text: String,
        before: Option<TextSnapshot>,
        kind: TextEditKind,
        cx: &mut Context<Self>,
    ) {
        let after = self.field_snapshot();
        let changed = before
            .as_ref()
            .is_none_or(|before| before.text != after.text);
        if let (Some(history), Some(before)) = (&mut self.field_history, before) {
            history.record(before, after, range.clone(), &text, kind, Instant::now());
        }
        self.shift_model(&range, text.len());
        self.committed_text.clone_from(&self.state.text);
        self.invalidate_layout();
        self.reveal_caret = true;
        if changed {
            cx.emit(NativeCanvasEvent::Edited { range, text });
        }
        // The parent acknowledges the edit and its resulting caret together.
        // Only explicit navigation cuts the document's typing history group.
        cx.notify();
    }
    fn commit_composition(&mut self, cx: &mut Context<Self>) {
        let Some(composition) = self.composition.take() else {
            return;
        };
        self.state.marked = None;
        let text = self
            .state
            .text
            .get(composition.display.clone())
            .unwrap_or_default()
            .to_string();
        let before = self.field_before.take();
        self.emit_edit(
            composition.original,
            text,
            before,
            TextEditKind::Composition,
            cx,
        );
    }
    fn field_history_step(&mut self, redo: bool, cx: &mut Context<Self>) {
        if self.is_composing() || self.read_only {
            return;
        }
        let before = self.field_snapshot();
        let Some(history) = &mut self.field_history else {
            return;
        };
        let restored = if redo {
            history.redo(before.clone())
        } else {
            history.undo(before.clone())
        };
        let Some(snapshot) = restored else {
            return;
        };
        let text = snapshot.text.clone();
        self.restore_field(snapshot, cx);
        // Restoring a field is a real parent-owned draft change, but must not
        // record a new history entry or turn a selection into a fake IME mark.
        cx.emit(NativeCanvasEvent::Edited {
            range: 0..before.text.len(),
            text,
        });
        cx.notify();
    }
    fn explicit_style_at(&self, offset: usize) -> Option<HtmlStyle> {
        self.fragments.iter().find_map(|fragment| {
            self.displayed_range(&fragment.range, false)
                .filter(|fragment| offset >= fragment.start && offset < fragment.end)
                .map(|_| fragment.style.clone())
        })
    }
    fn style_at(&self, offset: usize) -> HtmlStyle {
        self.explicit_style_at(offset).unwrap_or_default()
    }
    fn run(&self, range: &Range<usize>, font_size: f32, heading: bool, code: bool) -> TextRun {
        let explicit = self.explicit_style_at(range.start);
        let (bold, code_font) = effective_font_marks(explicit.as_ref(), heading, code);
        let style = explicit.unwrap_or_default();
        let mut font = font(if self.monospace {
            "Consolas".to_string()
        } else if code_font {
            self.tokens.typography.fallback_font_family.clone()
        } else {
            self.tokens.typography.font_family.clone()
        });
        if bold {
            font.weight = FontWeight::BOLD;
        }
        if style.italic {
            font.style = FontStyle::Italic;
        }
        let foreground = style
            .color
            .as_deref()
            .and_then(super::css_color)
            .unwrap_or(self.colors.foreground);
        let marked = self
            .state
            .marked
            .as_ref()
            .is_some_and(|marked| range.start < marked.end && range.end > marked.start);
        let _ = font_size;
        TextRun {
            len: range.end - range.start,
            font,
            color: foreground,
            background_color: style.background.as_deref().and_then(super::css_color),
            underline: (style.underline || style.link.is_some() || marked).then_some(
                UnderlineStyle {
                    thickness: px(1.),
                    color: Some(foreground),
                    wavy: false,
                },
            ),
            strikethrough: style.strike.then_some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(foreground),
            }),
        }
    }
    fn format_at(&self, offset: usize) -> Option<HtmlBlock> {
        self.paragraphs.iter().find_map(|paragraph| {
            self.displayed_range(&paragraph.range, false)
                .filter(|range| offset >= range.start && (offset < range.end || range.is_empty()))
                .and_then(|_| paragraph.format.clone())
        })
    }
    fn refresh_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The parent supplies only already-permitted local paths. This cache
        // belongs to this canvas, so pruning cannot evict another surface's assets.
        let active = self
            .atoms
            .iter()
            .filter_map(|atom| atom.path.clone())
            .collect::<HashSet<_>>();
        let mut changed = false;
        for path in self.image_sources.difference(&active) {
            let resource = gpui::Resource::from(path.clone());
            self.image_cache
                .update(cx, |cache, cx| cache.remove(&resource, window, cx));
            changed |= self.resolved_images.remove(path).is_some();
        }
        self.image_sources = active;
        for path in &self.image_sources {
            let resource = gpui::Resource::from(path.clone());
            let Some(Ok(image)) = self
                .image_cache
                .update(cx, |cache, cx| cache.load(&resource, window, cx))
            else {
                continue;
            };
            if self
                .resolved_images
                .get(path)
                .is_some_and(|old| old.image.id == image.id)
            {
                continue;
            }
            // Img knows SVG's internal render scale. Measuring its intrinsic
            // layout avoids interpreting device raster pixels as logical pixels.
            let mut measurement = img(image.clone()).into_any_element();
            let natural = measurement
                .layout_as_root(
                    size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                    window,
                    cx,
                )
                .map(f32::from);
            if natural.width.is_finite()
                && natural.height.is_finite()
                && natural.width >= 0.
                && natural.height >= 0.
            {
                self.resolved_images
                    .insert(path.clone(), ResolvedImage { image, natural });
                changed = true;
            }
        }
        if changed {
            self.invalidate_layout();
        }
    }
    fn image_size(&self, atom: &CanvasAtom, available: Pixels) -> Option<gpui::Size<Pixels>> {
        let natural = atom
            .path
            .as_ref()
            .and_then(|path| self.resolved_images.get(path))
            .map(|image| image.natural);
        if let Some(layout) = atom.html_image_layout {
            resolve_html_image_size(layout, natural, f32::from(available.max(px(0.))), None)
                .map(|size| size.map(px))
        } else if atom.kind == "image" {
            // The IM wrapper has a 1px border around the 90px image content.
            resolve_im_image_size(natural?, f32::from((available - px(2.)).max(px(0.))))
                .map(|content| content.map(|axis| px(axis + 2.)))
        } else {
            None
        }
    }
    fn shape(&self, width: Pixels, window: &mut Window) -> CanvasLayout {
        let mut layout = CanvasLayout {
            width,
            ..Default::default()
        };
        let text = &self.state.text;
        let mut offset = 0;
        let mut row_start = 0;
        let mut row_top = px(0.);
        let mut row_first_piece = 0;
        let mut format = self.format_at(0);
        let mut first_row = true;
        let mut x = px(0.);
        let mut row_height = px(self.tokens.typography.font_size + 6.);
        let mut paragraph_number = 0usize;
        loop {
            let heading = format.as_ref().and_then(|format| {
                if let HtmlBlockKind::Header(level) = format.kind {
                    Some(level)
                } else {
                    None
                }
            });
            let font_size = heading
                .map(|level| 24. - f32::from(level) * 2.)
                .unwrap_or(self.tokens.typography.font_size);
            let line_height = px(font_size + 6.);
            row_height = row_height.max(line_height);
            let left = px(format
                .as_ref()
                .map(|format| {
                    f32::from(format.indent) * 16.
                        + if format.quote { 10. } else { 0. }
                        + if format.list.is_some() { 24. } else { 0. }
                })
                .unwrap_or(0.));
            x = x.max(left);
            let at_end = offset >= text.len();
            if at_end && self.terminal_newline && text.ends_with('\n') {
                break;
            }
            let newline = !at_end && text.as_bytes()[offset] == b'\n';
            if at_end || newline {
                row_height = finish_row(
                    &mut layout,
                    row_start..offset,
                    row_top,
                    row_height,
                    left,
                    x,
                    format.clone(),
                    first_row,
                    paragraph_number,
                    row_first_piece,
                    window,
                    self,
                );
                row_top += row_height;
                if at_end {
                    break;
                }
                offset += 1;
                row_start = offset;
                row_first_piece = layout.pieces.len();
                row_height = px(self.tokens.typography.font_size + 6.);
                x = px(0.);
                first_row = true;
                paragraph_number += 1;
                format = self.format_at(offset);
                continue;
            }
            if text.as_bytes()[offset] == b'\r' && text.as_bytes().get(offset + 1) == Some(&b'\n') {
                offset += 1;
                continue;
            }
            if let Some((index, range)) = self.atoms.iter().enumerate().find_map(|(index, atom)| {
                self.displayed_range(&atom.range, true)
                    .filter(|range| range.start == offset)
                    .map(|range| (index, range))
            }) {
                let atom = &self.atoms[index];
                let atom_size = if let Some(resolved) = self.image_size(atom, width - left) {
                    resolved
                } else if atom.path.is_some() {
                    size(px(100.).min((width - left).max(px(16.))), px(76.))
                } else {
                    let label = atom_display_label(&atom.label);
                    let mut run = self.run(&(offset..range.end), font_size, false, false);
                    run.len = label.len();
                    let line =
                        window
                            .text_system()
                            .shape_line(label.into(), px(font_size), &[run], None);
                    size(
                        (line.width + px(12.))
                            .min(px(240.))
                            .min((width - left).max(px(16.))),
                        line_height + px(8.),
                    )
                };
                if x + atom_size.width > width && x > left {
                    row_height = finish_row(
                        &mut layout,
                        row_start..offset,
                        row_top,
                        row_height,
                        left,
                        x,
                        format.clone(),
                        first_row,
                        paragraph_number,
                        row_first_piece,
                        window,
                        self,
                    );
                    row_top += row_height;
                    row_start = offset;
                    row_first_piece = layout.pieces.len();
                    row_height = line_height;
                    x = left;
                    first_row = false;
                    continue;
                }
                layout.pieces.push(Piece::Atom {
                    index,
                    range: range.clone(),
                    bounds: Bounds::new(point(x, row_top), atom_size),
                });
                x += atom_size.width;
                row_height = row_height.max(atom_size.height);
                offset = range.end;
                continue;
            }
            let mut end = text[offset..]
                .find('\n')
                .map(|index| offset + index)
                .unwrap_or(text.len());
            for atom in &self.atoms {
                if let Some(range) = self.displayed_range(&atom.range, true) {
                    if range.start > offset {
                        end = end.min(range.start);
                    }
                }
            }
            for fragment in &self.fragments {
                if let Some(range) = self.displayed_range(&fragment.range, false) {
                    for boundary in [range.start, range.end] {
                        if boundary > offset {
                            end = end.min(boundary);
                        }
                    }
                }
            }
            if let Some(marked) = &self.state.marked {
                for boundary in [marked.start, marked.end] {
                    if boundary > offset {
                        end = end.min(boundary);
                    }
                }
            }
            // Bound each shaping request, rather than repeatedly shaping the
            // whole remaining megabyte-sized clipboard paragraph on every row.
            end = floor_boundary(text, end.min(offset.saturating_add(4096)));
            if end > offset
                && end < text.len()
                && text.as_bytes()[end] == b'\n'
                && text.as_bytes()[end - 1] == b'\r'
            {
                end -= 1;
            }
            if text.as_bytes()[offset] == b'\r' && end == offset + 1 {
                offset = end;
                continue;
            }
            if end <= offset {
                end = offset + text[offset..].chars().next().unwrap().len_utf8();
            }
            let run = self.run(
                &(offset..end),
                font_size,
                heading.is_some(),
                format.as_ref().is_some_and(|format| format.code),
            );
            let text_style = self.style_at(offset);
            let glyph_font_size = if text_style.subscript || text_style.superscript {
                font_size * 0.8
            } else {
                font_size
            };
            let paint_offset = if text_style.superscript {
                px(-font_size * 0.2)
            } else if text_style.subscript {
                px(font_size * 0.2)
            } else {
                px(0.)
            };
            let shaped = window.text_system().shape_line(
                text[offset..end].to_string().into(),
                px(glyph_font_size),
                &[run.clone()],
                None,
            );
            let available = (width - x).max(px(0.));
            let mut take = end - offset;
            if shaped.width > available {
                let boundaries = text[offset..end]
                    .grapheme_indices(true)
                    .map(|(index, _)| index)
                    .chain(std::iter::once(end - offset))
                    .collect::<Vec<_>>();
                take = boundaries
                    .iter()
                    .copied()
                    .filter(|index| shaped.x_for_index(*index) <= available)
                    .last()
                    .unwrap_or(0);
                if take == 0 && x > left {
                    row_height = finish_row(
                        &mut layout,
                        row_start..offset,
                        row_top,
                        row_height,
                        left,
                        x,
                        format.clone(),
                        first_row,
                        paragraph_number,
                        row_first_piece,
                        window,
                        self,
                    );
                    row_top += row_height;
                    row_start = offset;
                    row_first_piece = layout.pieces.len();
                    row_height = line_height;
                    x = left;
                    first_row = false;
                    continue;
                }
                if take == 0 {
                    take = boundaries
                        .iter()
                        .copied()
                        .find(|index| *index > 0)
                        .unwrap_or(end - offset);
                }
                // Prefer an existing whitespace break without introducing or
                // dropping any character. Long words still wrap by grapheme.
                if take < end - offset {
                    if let Some((index, ch)) = text[offset..offset + take]
                        .char_indices()
                        .rev()
                        .find(|(_, ch)| ch.is_whitespace())
                    {
                        let boundary = index + ch.len_utf8();
                        if boundary > 0 {
                            take = boundary;
                        }
                    }
                }
            }
            let range = offset..offset + take;
            let line = if take == end - offset {
                shaped
            } else {
                let mut run = run;
                run.len = take;
                window.text_system().shape_line(
                    text[range.clone()].to_string().into(),
                    px(glyph_font_size),
                    &[run],
                    None,
                )
            };
            let piece_width = line.width;
            layout.pieces.push(Piece::Text {
                range: range.clone(),
                bounds: Bounds::new(point(x, row_top), size(piece_width, line_height)),
                line,
                line_height,
                paint_offset,
            });
            x += piece_width;
            offset = range.end;
            if take < end - row_start && offset < end && x >= width {
                row_height = finish_row(
                    &mut layout,
                    row_start..offset,
                    row_top,
                    row_height,
                    left,
                    x,
                    format.clone(),
                    first_row,
                    paragraph_number,
                    row_first_piece,
                    window,
                    self,
                );
                row_top += row_height;
                row_start = offset;
                row_first_piece = layout.pieces.len();
                row_height = line_height;
                x = left;
                first_row = false;
            }
        }
        layout.height = row_top;
        layout
    }
    fn reveal(&mut self, layout: &CanvasLayout, bounds: Bounds<Pixels>) {
        if self.reveal_caret {
            let (position, height) = layout.position(self.state.cursor());
            if position.y < self.scroll_y {
                self.scroll_y = position.y;
            } else if position.y + height > self.scroll_y + bounds.size.height {
                self.scroll_y = position.y + height - bounds.size.height;
            }
        }
        self.scroll_y = self
            .scroll_y
            .max(px(0.))
            .min((layout.height - bounds.size.height).max(px(0.)));
    }
}

fn finish_row(
    layout: &mut CanvasLayout,
    range: Range<usize>,
    top: Pixels,
    _height: Pixels,
    left: Pixels,
    right: Pixels,
    format: Option<HtmlBlock>,
    first: bool,
    number: usize,
    first_piece: usize,
    window: &mut Window,
    input: &NativeCanvas,
) -> Pixels {
    let heading = format.as_ref().and_then(|format| match format.kind {
        HtmlBlockKind::Header(level) => Some(level),
        _ => None,
    });
    let font_size = heading
        .map(|level| 24. - f32::from(level) * 2.)
        .unwrap_or(input.tokens.typography.font_size);
    let line_height = font_size + 6.;
    let run = input.run(
        &(range.start..range.start),
        font_size,
        heading.is_some(),
        format.as_ref().is_some_and(|format| format.code),
    );
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&run.font);
    let mut metrics = text_row_metrics(
        line_height,
        f32::from(text_system.ascent(font_id, px(font_size))),
        f32::from(text_system.descent(font_id, px(font_size))),
        0.,
    );
    let x_height = f32::from(text_system.x_height(font_id, px(font_size)));
    for piece in &layout.pieces[first_piece..] {
        let piece_metrics = match piece {
            Piece::Text {
                line,
                line_height,
                paint_offset,
                ..
            } => text_row_metrics(
                f32::from(*line_height),
                f32::from(line.ascent),
                f32::from(line.descent),
                f32::from(*paint_offset),
            ),
            Piece::Atom { index, bounds, .. } => {
                if input.atoms[*index].html_image_layout.is_some() {
                    RowMetrics::new(f32::from(bounds.size.height), 0.)
                } else {
                    half_xheight(f32::from(bounds.size.height), x_height)
                }
            }
        };
        metrics.include(piece_metrics);
    }
    for piece in &mut layout.pieces[first_piece..] {
        let anchor = match piece {
            Piece::Text {
                line, line_height, ..
            } => alignment_baseline(
                f32::from(*line_height),
                f32::from(line.ascent),
                f32::from(line.descent),
            ),
            Piece::Atom { index, bounds, .. } => {
                if input.atoms[*index].html_image_layout.is_some() {
                    f32::from(bounds.size.height)
                } else {
                    half_xheight(f32::from(bounds.size.height), x_height).above
                }
            }
        };
        piece.bounds_mut().origin.y = px(metrics.piece_origin(f32::from(top), anchor));
    }
    let height = px(metrics.height());
    let marker = first
        .then(|| format.as_ref().and_then(|format| format.list))
        .flatten()
        .map(|kind| {
            let text: SharedString = if kind == HtmlListKind::Ordered {
                format!("{}. ", number + 1).into()
            } else {
                "• ".into()
            };
            let run = TextRun {
                len: text.len(),
                font: font(input.tokens.typography.font_family.clone()),
                color: input.colors.foreground,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window.text_system().shape_line(
                text,
                px(input.tokens.typography.font_size),
                &[run],
                None,
            )
        });
    layout.rows.push(CanvasRow {
        range,
        pieces: first_piece..layout.pieces.len(),
        top,
        height,
        left,
        right,
        format,
        marker,
        baseline: px(metrics.baseline(f32::from(top))),
    });
    height
}

impl Focusable for NativeCanvas {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EntityInputHandler for NativeCanvas {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.state.text, range);
        *adjusted = Some(to_utf16(&self.state.text, range.clone()));
        Some(self.state.text[range].to_string())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: to_utf16(&self.state.text, self.state.selection.clone()),
            reversed: self.state.reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.state
            .marked
            .clone()
            .map(|range| to_utf16(&self.state.text, range))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.state.marked = None;
        self.commit_composition(cx);
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let text = self.field_text(text);
        let replacement = self.atomic_range(self.state.replacement_range(range));
        let native = to_utf16(&self.state.text, replacement.clone());
        if let Some(composition) = &mut self.composition {
            composition.replacing(&replacement, text.len());
            self.state.replace_multiline(Some(native), &text);
            self.commit_composition(cx);
        } else {
            let before = if self.field_history.is_some() {
                Some(
                    self.field_before
                        .take()
                        .unwrap_or_else(|| self.field_snapshot()),
                )
            } else {
                None
            };
            let kind = if self.next_field_edit == TextEditKind::Typing && text.is_empty() {
                TextEditKind::Delete
            } else {
                self.next_field_edit
            };
            self.next_field_edit = TextEditKind::Typing;
            self.state.replace_multiline(Some(native), &text);
            self.emit_edit(replacement, text, before, kind, cx);
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let text = self.field_text(text);
        let replacement = self.atomic_range(self.state.replacement_range(range));
        let native = to_utf16(&self.state.text, replacement.clone());
        if let Some(composition) = &mut self.composition {
            composition.replacing(&replacement, text.len());
        } else {
            if self.field_history.is_some() {
                self.field_before = Some(self.field_snapshot());
            }
            self.composition = Some(Composition {
                original: replacement.clone(),
                display: replacement.start..replacement.start + text.len(),
            });
        }
        self.state
            .replace_marked_multiline(Some(native), &text, selected);
        self.invalidate_layout();
        self.reveal_caret = true;
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = from_utf16(&self.state.text, range);
        let layout = self
            .layout
            .clone()
            .unwrap_or_else(|| self.shape(bounds.size.width, window));
        self.reveal(&layout, bounds);
        let (position, height) = layout.position(range.start);
        let (end, _) = layout.position(range.end);
        let x = (bounds.left() + position.x)
            .max(bounds.left())
            .min((bounds.right() - px(1.)).max(bounds.left()));
        let y = (bounds.top() + position.y - self.scroll_y)
            .max(bounds.top())
            .min((bounds.bottom() - height).max(bounds.top()));
        self.layout = Some(layout);
        Some(Bounds::new(
            point(x, y),
            size(
                if end.y == position.y {
                    (end.x - position.x).max(px(1.)).min(bounds.size.width)
                } else {
                    px(1.)
                },
                height.min(bounds.size.height),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_bounds?;
        let byte = self.index_at(position);
        Some(to_utf16(&self.state.text, byte..byte).start)
    }
}

struct CanvasElement {
    input: Entity<NativeCanvas>,
}
struct CanvasPrepaint {
    layout: CanvasLayout,
    atoms: Vec<(usize, AnyElement)>,
}
impl IntoElement for CanvasElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for CanvasElement {
    type RequestLayoutState = ();
    type PrepaintState = CanvasPrepaint;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> CanvasPrepaint {
        let (layout, scroll, colors, atoms, font_size, images, image_cache) =
            self.input.update(cx, |input, cx| {
                input.refresh_images(window, cx);
                let layout = input
                    .layout
                    .clone()
                    .filter(|layout| layout.width == bounds.size.width)
                    .unwrap_or_else(|| input.shape(bounds.size.width, window));
                input.reveal(&layout, bounds);
                input.last_bounds = Some(bounds);
                input.layout = Some(layout.clone());
                (
                    layout,
                    input.scroll_y,
                    input.colors,
                    input.atoms.clone(),
                    input.tokens.typography.font_size,
                    input.resolved_images.clone(),
                    input.image_cache.clone(),
                )
            });
        let mut children = Vec::new();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (piece_index, piece) in layout.pieces.iter().enumerate() {
                let Piece::Atom {
                    index,
                    bounds: atom_bounds,
                    ..
                } = piece
                else {
                    continue;
                };
                let top = bounds.top() + atom_bounds.top() - scroll;
                if top + atom_bounds.size.height < bounds.top() || top > bounds.bottom() {
                    continue;
                }
                let atom = &atoms[*index];
                let html_image = atom.html_image_layout.is_some();
                let label = atom_display_label(&atom.label);
                let content = if let Some(path) = &atom.path {
                    let image = if let Some(resolved) = images.get(path) {
                        img(resolved.image.clone())
                    } else {
                        img(path.clone()).image_cache(&image_cache)
                    };
                    image
                        .w_full()
                        .h_full()
                        .object_fit(if html_image {
                            gpui::ObjectFit::Fill
                        } else {
                            gpui::ObjectFit::Contain
                        })
                        .with_fallback(move || div().child(label.clone()).into_any_element())
                        .into_any_element()
                } else {
                    rsx! {
                        <div size_full flex items_center overflow_hidden px={px(5.)}>
                            {label}
                        </div>
                    }
                        .into_any_element()
                };
                let mut child = rsx! {
                    <div w={atom_bounds.size.width} h={atom_bounds.size.height} overflow_hidden
                        text_color={colors.foreground} text_size={px(font_size)}>
                        {content}
                    </div>
                };
                if !html_image {
                    child = child.border_1().border_color(colors.border).rounded(px(3.));
                }
                let mut child = child.into_any_element();
                child.prepaint_as_root(
                    point(
                        bounds.left() + atom_bounds.left(),
                        bounds.top() + atom_bounds.top() - scroll,
                    ),
                    size(
                        AvailableSpace::Definite(atom_bounds.size.width),
                        AvailableSpace::Definite(atom_bounds.size.height),
                    ),
                    window,
                    cx,
                );
                children.push((piece_index, child));
            }
        });
        CanvasPrepaint {
            layout,
            atoms: children,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut CanvasPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let input = self.input.read(cx);
        let focus = input.focus.clone();
        let focused = focus.is_focused(window);
        let selection = input.state.selection.clone();
        let cursor = input.state.cursor();
        let colors = input.colors;
        let scroll = input.scroll_y;
        let code_background = input.code_background;
        let marker_height = px(input.tokens.typography.font_size + 6.);
        let visible_bounds = bounds.intersect(&window.content_mask().bounds);
        let placeholder = if input.state.text.is_empty() {
            input.placeholder.as_ref().map(|placeholder| {
                let mut run = input.run(&(0..0), input.tokens.typography.font_size, false, false);
                run.len = placeholder.len();
                run.color = colors.placeholder;
                (placeholder.clone(), input.tokens.typography.font_size, run)
            })
        } else {
            None
        };
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some((placeholder, font_size, run)) = placeholder {
                let line =
                    window
                        .text_system()
                        .shape_line(placeholder, px(font_size), &[run], None);
                let _ = line.paint(bounds.origin, px(font_size + 6.), window, cx);
            }
            for row in &prepaint.layout.rows {
                let top = bounds.top() + row.top - scroll;
                if top + row.height < bounds.top() || top > bounds.bottom() {
                    continue;
                }
                if let Some(format) = &row.format {
                    if format.code {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.left() + row.left, top),
                                size((bounds.size.width - row.left).max(px(0.)), row.height),
                            ),
                            code_background,
                        ));
                    }
                    if format.quote {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.left() + row.left - px(8.), top),
                                size(px(2.), row.height),
                            ),
                            colors.border,
                        ));
                    }
                    if format.kind == HtmlBlockKind::HorizontalRule {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.left() + row.left, top + row.height / 2.),
                                size((bounds.size.width - row.left).max(px(0.)), px(1.)),
                            ),
                            colors.border,
                        ));
                    }
                }
                if let Some(marker) = &row.marker {
                    let anchor = alignment_baseline(
                        f32::from(marker_height),
                        f32::from(marker.ascent),
                        f32::from(marker.descent),
                    );
                    let _ = marker.paint(
                        point(
                            bounds.left() + row.left - px(24.),
                            bounds.top() + row.baseline - scroll - px(anchor),
                        ),
                        marker_height,
                        window,
                        cx,
                    );
                }
            }
            for piece in &prepaint.layout.pieces {
                if let Piece::Text {
                    bounds: piece_bounds,
                    line,
                    line_height,
                    paint_offset,
                    ..
                } = piece
                {
                    let top = bounds.top() + piece_bounds.top() - scroll;
                    if top + piece_bounds.size.height >= bounds.top() && top <= bounds.bottom() {
                        let _ = line.paint_background(
                            point(bounds.left() + piece_bounds.left(), top + *paint_offset),
                            *line_height,
                            window,
                            cx,
                        );
                    }
                }
            }
            for row in &prepaint.layout.rows {
                let top = bounds.top() + row.top - scroll;
                if top + row.height < bounds.top() || top > bounds.bottom() {
                    continue;
                }
                if focused
                    && !selection.is_empty()
                    && selection.start <= row.range.end
                    && selection.end > row.range.start
                {
                    let start = selection.start.max(row.range.start);
                    let end = selection.end.min(row.range.end);
                    let (p1, _) = prepaint.layout.position_in_row(start, row);
                    let (p2, _) = prepaint.layout.position_in_row(end, row);
                    let left = if start <= row.range.start {
                        row.left
                    } else {
                        p1.x
                    };
                    let right = if selection.end > row.range.end {
                        bounds.size.width
                    } else {
                        p2.x
                    };
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.left() + left, top),
                            size((right - left).max(px(2.)), row.height),
                        ),
                        colors.selection,
                    ));
                }
            }
            for (index, piece) in prepaint.layout.pieces.iter().enumerate() {
                let piece_bounds = piece.bounds();
                let top = bounds.top() + piece_bounds.top() - scroll;
                if top + piece_bounds.size.height < bounds.top() || top > bounds.bottom() {
                    continue;
                }
                match piece {
                    Piece::Text {
                        line,
                        line_height,
                        paint_offset,
                        ..
                    } => {
                        let _ = line.paint(
                            point(bounds.left() + piece_bounds.left(), top + *paint_offset),
                            *line_height,
                            window,
                            cx,
                        );
                    }
                    Piece::Atom { range, .. } => {
                        if let Some((_, child)) =
                            prepaint.atoms.iter_mut().find(|(piece, _)| *piece == index)
                        {
                            child.paint(window, cx);
                        }
                        if focused && selection.start < range.end && selection.end > range.start {
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(bounds.left() + piece_bounds.left(), top),
                                    size(piece_bounds.size.width, px(2.)),
                                ),
                                colors.caret,
                            ));
                        }
                    }
                }
            }
            if focused && selection.is_empty() {
                let (position, height) = prepaint.layout.position(cursor);
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.left() + position.x,
                            bounds.top() + position.y - scroll,
                        ),
                        size(px(1.), height),
                    ),
                    colors.caret,
                ));
            }
        });
        self.input.update(cx, |input, _| {
            input.reveal_caret = false;
            input.last_focused = focused;
            input.local_frame = input.local_frame.wrapping_add(1);
            let frame_id = input
                .paint_clock
                .as_ref()
                .map(|clock| clock.load(Ordering::Acquire))
                .unwrap_or(input.local_frame);
            input.last_paint = Some(CanvasPaintStamp {
                frame_id,
                version: input.render_version,
                bounds,
                visible_bounds,
                scroll_y: input.scroll_y,
                selection: input.state.selection.clone(),
                reversed: input.state.reversed,
                marked: input.state.marked.clone(),
                read_only: input.read_only,
                scale_factor: window.scale_factor(),
            });
        });
    }
}
impl Render for NativeCanvas {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .w_full()
            .min_w(px(0.))
            .when(self.fill_height, |input| input.flex_1().min_h(px(0.)).h_full())
            .when(!self.fill_height, |input| input.h(px(self.height)).flex_none())
            .px(px(self.padding_x))
            .py(px(self.padding_y))
            .bg(self.colors.background)
            .text_color(self.colors.foreground)
            .overflow_hidden()
            .key_context("ClipNativeCanvas")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::document_home))
            .on_action(cx.listener(Self::document_end))
            .on_action(cx.listener(Self::select_document_home))
            .on_action(cx.listener(Self::select_document_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(CanvasElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declared_html_image_uses_logical_dimensions_without_the_thumbnail_frame() {
        use crate::editor_html::HtmlImageDimension::{Auto, Pixels};
        let dimensions = HtmlImageLayout {
            width: Pixels(32.),
            height: Pixels(20.),
            ..Default::default()
        };
        assert_eq!(
            resolve_html_image_size(dimensions, None, 600., None),
            Some(size(32., 20.))
        );
        assert_eq!(
            resolve_html_image_size(dimensions, None, 16., None),
            Some(size(16., 20.))
        );
        let width_only = HtmlImageLayout {
            height: Auto,
            ..dimensions
        };
        assert_eq!(
            resolve_html_image_size(width_only, Some(size(100., 50.)), 600., None),
            Some(size(32., 16.))
        );
        assert_eq!(resolve_html_image_size(width_only, None, 600., None), None);
    }
    #[test]
    fn plain_source_and_metadata_keep_their_existing_newline_contract() {
        let source = "<x vendor='&amp;'>😀\r\n\u{200b}</x>";
        assert_eq!(plain_field_text(source, false), source);
        assert_eq!(
            plain_field_text(source, true),
            "<x vendor='&amp;'>😀  \u{200b}</x>"
        );
    }

    #[test]
    fn committed_field_ime_undo_restores_real_reversed_selection_and_raw_source() {
        let original = "AA😀\r\nBB";
        let mut state = TextState {
            text: original.into(),
            ..Default::default()
        };
        state.move_to(6);
        state.select_to(2);
        let before = snapshot_from_state(&state);
        let mut history = EditorTextHistory::default();
        history.reset(before.clone());
        let mut composition = Composition {
            original: 2..6,
            display: 2..5,
        };
        state.replace_marked_multiline(None, "中", Some(0..0));
        composition.replacing(&(2..5), 6);
        state.replace_marked_multiline(None, "中文", Some(2..2));
        state.replace_multiline(None, "中文");
        let committed = snapshot_from_state(&state);
        assert_eq!(committed.text, "AA中文\r\nBB");
        history.record(
            before.clone(),
            committed.clone(),
            composition.original,
            "中文",
            TextEditKind::Composition,
            Instant::now(),
        );
        restore_plain_state(&mut state, history.undo(committed).unwrap());
        assert_eq!(snapshot_from_state(&state), before);
        assert!(state.marked.is_none());
        let redo = history.redo(snapshot_from_state(&state)).unwrap();
        restore_plain_state(&mut state, redo);
        assert_eq!(state.text, "AA中文\r\nBB");
        assert_eq!(state.selection, 8..8);
        assert!(state.marked.is_none());
    }
    #[test]
    fn object_display_label_removes_line_breaks_and_retains_real_zwsp() {
        let source = "A\r\nB\rC\n\u{200b}D";
        assert_eq!(atom_display_label(source), "A B C \u{200b}D");
        assert_eq!(source, "A\r\nB\rC\n\u{200b}D");
    }
    #[test]
    fn both_halves_of_an_object_resolve_to_the_same_atomic_selection() {
        let layout = CanvasLayout {
            rows: vec![CanvasRow {
                range: 0..3,
                pieces: 0..1,
                top: px(0.),
                height: px(76.),
                left: px(0.),
                right: px(100.),
                format: None,
                marker: None,
                baseline: px(76.),
            }],
            pieces: vec![Piece::Atom {
                index: 0,
                range: 0..3,
                bounds: Bounds::new(point(px(0.), px(0.)), size(px(100.), px(76.))),
            }],
            ..Default::default()
        };
        assert_eq!(layout.atom_at(point(px(10.), px(20.))), Some(0..3));
        assert_eq!(layout.atom_at(point(px(90.), px(20.))), Some(0..3));
        assert_eq!(layout.atom_at(point(px(110.), px(20.))), None);
        for offset in [0, 3] {
            let (position, height) = layout.position(offset);
            assert_eq!(
                layout.index_at(point(position.x, position.y + height / 2.)),
                offset
            );
        }
    }
    #[test]
    fn html_terminal_newline_is_outside_the_editable_caret_domain() {
        assert_eq!(editable_limit("A\n", true), 1);
        assert_eq!(editable_limit("\n", true), 0);
        assert_eq!(editable_limit("A\n", false), 2);
        assert_eq!(editable_limit("中😀\n", true), "中😀".len());
    }
    #[test]
    fn explicit_inline_resets_override_heading_and_code_defaults() {
        assert_eq!(
            effective_font_marks(Some(&HtmlStyle::default()), true, true),
            (false, false)
        );
        assert_eq!(effective_font_marks(None, true, true), (true, true));
    }
    #[test]
    fn composition_tracks_exact_original_range_across_preedit_replacements() {
        let mut composition = Composition {
            original: 3..6,
            display: 3..9,
        };
        composition.replacing(&(3..9), 3);
        assert_eq!(composition.original, 3..6);
        assert_eq!(composition.display, 3..6);
        composition.replacing(&(3..6), 9);
        assert_eq!(composition.original, 3..6);
        assert_eq!(composition.display, 3..12);
    }
    #[test]
    fn composition_native_extension_maps_back_without_content_diff() {
        let mut composition = Composition {
            original: 4..4,
            display: 4..7,
        };
        composition.replacing(&(2..7), 8);
        assert_eq!(composition.original, 2..4);
        assert_eq!(composition.display, 2..10);
        assert_eq!(composition.project(&(12..15), false), Some(18..21));
    }
    #[test]
    fn composition_atom_projection_hides_replaced_atom_and_shifts_following() {
        let composition = Composition {
            original: 4..7,
            display: 4..10,
        };
        assert_eq!(composition.project(&(4..7), true), None);
        assert_eq!(composition.project(&(10..13), true), Some(13..16));
    }
    #[test]
    fn insertion_before_atom_shifts_both_object_edges() {
        let composition = Composition {
            original: 4..4,
            display: 4..7,
        };
        assert_eq!(composition.project(&(4..7), true), Some(7..10));
        assert_eq!(composition.project(&(1..4), true), Some(1..4));
    }
    #[test]
    fn object_boundary_splits_fffc_combining_grapheme_for_navigation_and_delete() {
        let mut state = TextState {
            text: "\u{fffc}\u{301}X".into(),
            ..Default::default()
        };
        state.move_to(0);
        assert_eq!(adjacent_offset(&state, std::iter::once(0..3), true), 3);
        state.move_to(3);
        assert_eq!(adjacent_offset(&state, std::iter::once(0..3), false), 0);
        state.move_to(5);
        assert_eq!(adjacent_offset(&state, std::iter::once(0..3), false), 3);
        state.select_to(3);
        state.replace_multiline(None, "");
        assert_eq!(state.text, "\u{fffc}X");
    }
}
