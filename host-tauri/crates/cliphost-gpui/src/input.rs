//! Single-line GPUI search input, using the platform text/IME interface.
//!
//! The EntityInputHandler/Element integration follows GPUI 0.2.2's input example
//! (Zed Industries, Apache-2.0):
//! https://github.com/zed-industries/zed/blob/69e2130295c2649963eb639fc70b4f2ee8ea1624/crates/gpui/examples/input.rs
//! Text editing, composition-relative UTF-16 mapping and committed-change events
//! are implemented here; no character data is assembled from KeyDown events.

use gpui_rsx::rsx;
use std::ops::Range;

use cliphost_core::i18n::tr;
use gpui::{
    actions, div, fill, point, prelude::*, px, relative, rgb, rgba, size, App, Bounds,
    ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, Hsla, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    clip_search_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Submit,
        Escape
    ]
);

#[derive(Debug, Clone)]
pub enum SearchEvent {
    Changed(String),
    Submit,
    Escape,
}

/// Opt-in caption movement; ordinary inputs keep their existing text selection.
#[derive(Debug, Clone)]
pub struct InputWindowDrag(pub Point<Pixels>);

#[derive(Default)]
struct WindowDragGesture {
    origin: Option<Point<Pixels>>,
}

impl WindowDragGesture {
    fn update(&mut self, position: Point<Pixels>, left_held: bool) -> Option<Point<Pixels>> {
        if !left_held {
            self.origin = None;
            return None;
        }
        // Same threshold as GPUI 0.2.2's Div drag recognition.
        let origin = self.origin?;
        if (position - origin).magnitude() > 2.0 {
            self.origin.take()
        } else {
            None
        }
    }
}

/// Supplied from the same theme parameters as the browser design fixture.
#[derive(Clone, Copy)]
pub struct InputColors {
    pub background: Hsla,
    pub foreground: Hsla,
    pub placeholder: Hsla,
    pub border: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
}

impl Default for InputColors {
    fn default() -> Self {
        Self {
            background: rgb(0x20242b).into(),
            foreground: rgb(0xe5e7eb).into(),
            placeholder: rgb(0x9098a5).into(),
            border: rgb(0x454b55).into(),
            selection: rgba(0x4086d970).into(),
            caret: rgb(0x75adff).into(),
        }
    }
}

#[derive(Default)]
pub(crate) struct TextState {
    pub(crate) text: String,
    pub(crate) selection: Range<usize>,
    pub(crate) reversed: bool,
    pub(crate) marked: Option<Range<usize>>,
}

impl TextState {
    pub(crate) fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    pub(crate) fn move_to(&mut self, offset: usize) {
        let offset = floor_boundary(&self.text, offset);
        self.selection = offset..offset;
        self.reversed = false;
    }

    pub(crate) fn select_to(&mut self, offset: usize) {
        let anchor = if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        let offset = floor_boundary(&self.text, offset);
        self.selection = anchor.min(offset)..anchor.max(offset);
        self.reversed = offset < anchor;
    }

    pub(crate) fn previous(&self) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < self.cursor()).then_some(index))
            .unwrap_or(0)
    }

    pub(crate) fn next(&self) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > self.cursor()).then_some(index))
            .unwrap_or(self.text.len())
    }

    pub(crate) fn replacement_range(&self, range: Option<Range<usize>>) -> Range<usize> {
        range
            .map(|range| from_utf16(&self.text, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone())
    }

    fn replace(&mut self, range: Option<Range<usize>>, text: &str) {
        let text = single_line(text);
        self.replace_multiline(range, &text);
    }

    pub(crate) fn replace_multiline(&mut self, range: Option<Range<usize>>, text: &str) {
        let range = self.replacement_range(range);
        self.text.replace_range(range.clone(), text);
        self.move_to(range.start + text.len());
        self.marked = None;
    }

    fn replace_marked(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
    ) {
        let text = single_line(text);
        self.replace_marked_multiline(range, &text, selected_utf16);
    }

    pub(crate) fn replace_marked_multiline(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
    ) {
        let range = self.replacement_range(range);
        // The IME selection is relative to its new composition, not the full field.
        let selected = selected_utf16
            .map(|selection| from_utf16(text, selection))
            .unwrap_or(text.len()..text.len());
        self.text.replace_range(range.clone(), text);
        self.selection = range.start + selected.start..range.start + selected.end;
        self.reversed = false;
        self.marked = (!text.is_empty()).then_some(range.start..range.start + text.len());
    }
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if matches!(ch, '\r' | '\n') { ' ' } else { ch })
        .collect()
}

pub(crate) fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn utf16_offset(text: &str, offset: usize, round_up: bool) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if offset <= units {
            return byte;
        }
        let next = units + ch.len_utf16();
        if offset < next {
            return if round_up { byte + ch.len_utf8() } else { byte };
        }
        units = next;
    }
    text.len()
}

pub(crate) fn from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let start = range.start.min(range.end);
    let end = range.start.max(range.end);
    let first = utf16_offset(text, start, false);
    if start == end {
        first..first
    } else {
        first..utf16_offset(text, end, true)
    }
}

pub(crate) fn to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let start = floor_boundary(text, range.start);
    let end = floor_boundary(text, range.end);
    text[..start].encode_utf16().count()..text[..end].encode_utf16().count()
}

pub struct SearchInput {
    state: TextState,
    focus: FocusHandle,
    placeholder: SharedString,
    colors: InputColors,
    height: f32,
    font_size: f32,
    horizontal_padding: f32,
    last_committed: String,
    last_line: Option<ShapedLine>,
    last_font: Option<gpui::Font>,
    last_bounds: Option<Bounds<Pixels>>,
    last_candidate_bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    selecting: bool,
    window_draggable: bool,
    window_drag: WindowDragGesture,
    window_drag_requests: usize,
    composition_updates: usize,
    composition_commits: usize,
    composition_cancels: usize,
    changed_events: usize,
    submit_events: usize,
    escape_events: usize,
    candidate_bounds_requests: usize,
    held_enter_keys_suppressed: usize,
    held_escape_keys_suppressed: usize,
    /// Search boxes clear on double-click (FilterBox); form fields select all.
    double_click_clears: bool,
    read_only: bool,
}

impl EventEmitter<SearchEvent> for SearchInput {}
impl EventEmitter<InputWindowDrag> for SearchInput {}

impl SearchInput {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let context = Some("ClipSearchInput");
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            KeyBinding::new("home", Home, context),
            KeyBinding::new("end", End, context),
            KeyBinding::new("shift-home", SelectHome, context),
            KeyBinding::new("shift-end", SelectEnd, context),
            KeyBinding::new("ctrl-a", SelectAll, context),
            KeyBinding::new("ctrl-c", Copy, context),
            KeyBinding::new("ctrl-x", Cut, context),
            KeyBinding::new("ctrl-v", Paste, context),
        ]);
        Self {
            state: TextState::default(),
            focus: cx.focus_handle(),
            placeholder: tr("搜索剪贴板", "Search clipboard").into(),
            colors: InputColors::default(),
            height: 28.,
            font_size: 13.,
            horizontal_padding: 8.,
            last_committed: String::new(),
            last_line: None,
            last_font: None,
            last_bounds: None,
            last_candidate_bounds: None,
            scroll_x: px(0.),
            selecting: false,
            window_draggable: false,
            window_drag: WindowDragGesture::default(),
            window_drag_requests: 0,
            composition_updates: 0,
            composition_commits: 0,
            composition_cancels: 0,
            changed_events: 0,
            submit_events: 0,
            escape_events: 0,
            candidate_bounds_requests: 0,
            held_enter_keys_suppressed: 0,
            held_escape_keys_suppressed: 0,
            double_click_clears: true,
            read_only: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.state.text
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn is_composing(&self) -> bool {
        self.state.marked.is_some()
    }

    /// Read by explicit QA reports; this never writes input text to a log.
    pub fn diagnostics(&self) -> serde_json::Value {
        let selection = to_utf16(&self.state.text, self.state.selection.clone());
        let marked = self
            .state
            .marked
            .clone()
            .map(|range| to_utf16(&self.state.text, range));
        serde_json::json!({
            "displayedText": self.state.text,
            "committedText": self.last_committed,
            "selectionUtf16": [selection.start, selection.end],
            "selectionReversed": self.state.reversed,
            "markedRangeUtf16": marked.map(|range| [range.start, range.end]),
            "compositionUpdates": self.composition_updates,
            "compositionCommits": self.composition_commits,
            "compositionCancels": self.composition_cancels,
            "changedEvents": self.changed_events,
            "submitEvents": self.submit_events,
            "escapeEvents": self.escape_events,
            "candidateBoundsRequests": self.candidate_bounds_requests,
            "heldEnterKeysSuppressed": self.held_enter_keys_suppressed,
            "heldEscapeKeysSuppressed": self.held_escape_keys_suppressed,
            "windowDragEnabled": self.window_draggable,
            "windowDragRequests": self.window_drag_requests,
            "candidateBounds": self.last_candidate_bounds.map(|bounds| serde_json::json!({
                "x": f32::from(bounds.origin.x), "y": f32::from(bounds.origin.y),
                "width": f32::from(bounds.size.width), "height": f32::from(bounds.size.height)
            }))
        })
    }

    /// Synchronize parent-owned text without emitting a user edit back to it.
    pub fn set_text(&mut self, text: String, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.text = single_line(&text);
        self.state.marked = None;
        self.state.move_to(self.state.text.len());
        self.last_line = None;
        self.last_committed.clone_from(&self.state.text);
        cx.notify();
    }

    pub fn set_double_click_clears(&mut self, clears: bool) {
        self.double_click_clears = clears;
    }

    pub fn set_window_draggable(&mut self, enabled: bool) {
        self.window_draggable = enabled;
        self.cancel_window_drag();
    }

    pub fn arm_window_drag(&mut self, event: &MouseDownEvent) {
        self.window_drag.origin = (self.window_draggable
            && event.button == MouseButton::Left
            && event.click_count == 1
            && event.modifiers.number_of_modifiers() == 0
            && !self.is_composing())
        .then_some(event.position);
    }

    pub fn cancel_window_drag(&mut self) {
        self.window_drag.origin = None;
        self.selecting = false;
    }

    fn window_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.is_composing() {
            self.cancel_window_drag();
            return;
        }
        if let Some(origin) = self.window_drag.update(
            event.position,
            event.pressed_button == Some(MouseButton::Left),
        ) {
            self.selecting = false;
            self.window_drag_requests += 1;
            cx.emit(InputWindowDrag(origin));
            cx.stop_propagation();
        }
    }

    /// Keep selection and copy available while a submitted dialog is saving.
    /// Callers must finish IME composition before submitting its snapshot.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = read_only;
        cx.notify();
    }

    pub fn set_colors(&mut self, colors: InputColors, cx: &mut Context<Self>) {
        self.colors = colors;
        cx.notify();
    }

    pub fn set_placeholder(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = text.into();
        cx.notify();
    }

    pub fn set_metrics(
        &mut self,
        height: f32,
        font_size: f32,
        horizontal_padding: f32,
        cx: &mut Context<Self>,
    ) {
        self.height = height.max(1.);
        self.font_size = font_size.max(1.);
        self.horizontal_padding = horizontal_padding.max(0.);
        self.last_line = None;
        cx.notify();
    }

    fn committed_changed(&mut self, cx: &mut Context<Self>) {
        if self.last_committed != self.state.text {
            self.last_committed.clone_from(&self.state.text);
            self.changed_events += 1;
            cx.emit(SearchEvent::Changed(self.state.text.clone()));
        }
        cx.notify();
    }

    fn navigation(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        if !self.is_composing() {
            if select {
                self.state.select_to(offset);
            } else {
                self.state.move_to(offset);
            }
            cx.notify();
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.state.selection.is_empty() {
            self.state.previous()
        } else {
            self.state.selection.start
        };
        self.navigation(offset, false, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.state.selection.is_empty() {
            self.state.next()
        } else {
            self.state.selection.end
        };
        self.navigation(offset, false, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation(self.state.previous(), true, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation(self.state.next(), true, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation(0, false, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation(self.state.text.len(), false, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation(0, true, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation(self.state.text.len(), true, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    /// Parent-owned inputs can select before their new focus target has been
    /// painted into the dispatch tree (for example an address field opened by Ctrl+L).
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        if !self.is_composing() {
            self.state.move_to(0);
            self.state.select_to(self.state.text.len());
            cx.notify();
        }
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if !self.read_only && !self.is_composing() {
            if self.state.selection.is_empty() {
                self.state.select_to(self.state.previous());
            }
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if !self.read_only && !self.is_composing() {
            if self.state.selection.is_empty() {
                self.state.select_to(self.state.next());
            }
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.state.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.state.text[self.state.selection.clone()].to_owned(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.read_only && !self.is_composing() && !self.state.selection.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if !self.read_only && !self.is_composing() {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.replace_text_in_range(None, &text, window, cx);
            }
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() {
            self.submit_events += 1;
            cx.emit(SearchEvent::Submit);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(event.keystroke.key.as_str(), "enter" | "escape")
            && event.keystroke.modifiers.number_of_modifiers() == 0
        {
            // IME commit/cancellation clears the marked range before held key
            // repeats arrive. Do not turn repeats into Submit/Escape or let them
            // bubble into parent paste/dismiss actions. GPUI actions run before
            // key listeners, so these keys deliberately have no KeyBinding.
            cx.stop_propagation();
            if event.is_held {
                if event.keystroke.key == "enter" {
                    self.held_enter_keys_suppressed += 1;
                } else {
                    self.held_escape_keys_suppressed += 1;
                }
            }
            if !event.is_held && !self.is_composing() {
                if event.keystroke.key == "enter" {
                    self.submit(&Submit, window, cx);
                } else {
                    self.escape(&Escape, window, cx);
                }
            }
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() {
            self.escape_events += 1;
            cx.emit(SearchEvent::Escape);
        }
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (self.last_bounds, &self.last_line) else {
            return 0;
        };
        if self.state.text.is_empty() {
            return 0;
        }
        floor_boundary(
            &self.state.text,
            line.closest_index_for_x(position.x - bounds.left() + self.scroll_x),
        )
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.arm_window_drag(event);
        window.focus(&self.focus);
        cx.stop_propagation();
        if self.is_composing() {
            return;
        }
        self.selecting = true;
        if event.click_count >= 2 && (!self.double_click_clears || self.read_only) {
            self.selecting = false;
            self.state.move_to(0);
            self.state.select_to(self.state.text.len());
            cx.notify();
            return;
        }
        if event.click_count >= 2 {
            // Existing FilterBox defaults to clearing the query on double-click.
            self.selecting = false;
            self.state.move_to(0);
            self.state.select_to(self.state.text.len());
            self.replace_text_in_range(None, "", window, cx);
        } else if event.modifiers.shift {
            self.state.select_to(self.index_at(event.position));
        } else {
            self.state.move_to(self.index_at(event.position));
        }
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.window_drag.origin.is_some() {
            return;
        }
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            self.state.select_to(self.index_at(event.position));
            cx.notify();
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.cancel_window_drag();
    }
}

impl Focusable for SearchInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.state.text, range);
        *adjusted_range = Some(to_utf16(&self.state.text, range.clone()));
        Some(self.state.text[range].to_owned())
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
        if self.read_only {
            return;
        }
        if self.is_composing() {
            self.composition_commits += 1;
        }
        self.state.marked = None;
        self.committed_changed(cx);
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
        if self.is_composing() {
            if text.is_empty() {
                self.composition_cancels += 1;
            } else {
                self.composition_commits += 1;
            }
        }
        self.state.replace(range, text);
        self.last_line = None;
        self.committed_changed(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        self.composition_updates += 1;
        self.state.replace_marked(range, text, selected_range);
        self.last_line = None;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.candidate_bounds_requests += 1;
        // IME may ask for the next composition's caret before the frame after
        // a commit has painted. Shape current text with its actual painted font
        // rather than returning no candidate position or indexing stale text.
        if self.last_line.is_none() {
            let font = self
                .last_font
                .clone()
                .unwrap_or_else(|| window.text_style().font());
            let text: SharedString = self.state.text.clone().into();
            let run = TextRun {
                len: text.len(),
                font,
                color: self.colors.foreground,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            self.last_line = Some(window.text_system().shape_line(
                text,
                px(self.font_size),
                &[run],
                None,
            ));
        }
        let line = self.last_line.as_ref()?;
        self.scroll_x = caret_scroll(
            line.width,
            line.x_for_index(self.state.cursor()),
            bounds.size.width,
            self.scroll_x,
        );
        let range = from_utf16(&self.state.text, range);
        let start = (bounds.left() + line.x_for_index(range.start) - self.scroll_x)
            .max(bounds.left())
            .min((bounds.right() - px(1.)).max(bounds.left()));
        let end = (bounds.left() + line.x_for_index(range.end) - self.scroll_x)
            .max(start)
            .min(bounds.right());
        let candidate = Bounds::from_corners(
            point(start, bounds.top()),
            point(end.max(start + px(1.)), bounds.bottom()),
        );
        self.last_candidate_bounds = Some(candidate);
        Some(candidate)
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_bounds?;
        self.last_line.as_ref()?;
        let byte = self.index_at(point);
        Some(to_utf16(&self.state.text, byte..byte).start)
    }
}

struct InputText {
    input: Entity<SearchInput>,
}

struct InputPrepaint {
    line: ShapedLine,
    font: gpui::Font,
    selection: Option<PaintQuad>,
    caret: Option<PaintQuad>,
    origin: Point<Pixels>,
    scroll_x: Pixels,
}

fn caret_scroll(width: Pixels, cursor: Pixels, viewport: Pixels, previous: Pixels) -> Pixels {
    let available = (viewport - px(2.)).max(px(0.));
    let scroll = if width <= available {
        px(0.)
    } else if cursor < previous {
        cursor
    } else if cursor > previous + available {
        cursor - available
    } else {
        previous
    };
    scroll.max(px(0.)).min((width - available).max(px(0.)))
}

impl IntoElement for InputText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for InputText {
    type RequestLayoutState = ();
    type PrepaintState = InputPrepaint;

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
        style.size.height = window.line_height().into();
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
    ) -> InputPrepaint {
        let input = self.input.read(cx);
        let placeholder = input.state.text.is_empty();
        let text: SharedString = if placeholder {
            input.placeholder.clone()
        } else {
            input.state.text.clone().into()
        };
        let style = window.text_style();
        let font = style.font();
        let base = TextRun {
            len: text.len(),
            font: font.clone(),
            color: if placeholder {
                input.colors.placeholder
            } else {
                input.colors.foreground
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let mut runs = Vec::new();
        if let Some(marked) = &input.state.marked {
            if marked.start > 0 {
                runs.push(TextRun {
                    len: marked.start,
                    ..base.clone()
                });
            }
            runs.push(TextRun {
                len: marked.len(),
                underline: Some(UnderlineStyle {
                    color: Some(base.color),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..base.clone()
            });
            if marked.end < text.len() {
                runs.push(TextRun {
                    len: text.len() - marked.end,
                    ..base
                });
            }
        } else {
            runs.push(base);
        }
        let line = window.text_system().shape_line(
            text,
            style.font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        );
        let cursor_x = line.x_for_index(input.state.cursor());
        let scroll_x = if placeholder {
            px(0.)
        } else {
            caret_scroll(line.width, cursor_x, bounds.size.width, input.scroll_x)
        };
        let origin = point(bounds.left() - scroll_x, bounds.top());
        let focused = input.focus.is_focused(window);
        let (selection, caret) = if !focused {
            (None, None)
        } else if input.state.selection.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(origin.x + cursor_x, origin.y),
                        size(px(1.), bounds.size.height),
                    ),
                    input.colors.caret,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            origin.x + line.x_for_index(input.state.selection.start),
                            bounds.top(),
                        ),
                        point(
                            origin.x + line.x_for_index(input.state.selection.end),
                            bounds.bottom(),
                        ),
                    ),
                    input.colors.selection,
                )),
                None,
            )
        };
        InputPrepaint {
            line,
            font,
            selection,
            caret,
            origin,
            scroll_x,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut InputPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.input.read(cx).window_draggable {
            let input = self.input.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    input.update(cx, |input, cx| input.window_drag_move(event, cx));
                }
            });
            let input = self.input.clone();
            window.on_mouse_event(move |_: &MouseDownEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    input.update(cx, |input, _| input.cancel_window_drag());
                }
            });
            let input = self.input.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    input.update(cx, |input, _| input.cancel_window_drag());
                }
            });
        }
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection);
            }
            let _ = prepaint
                .line
                .paint(prepaint.origin, window.line_height(), window, cx);
            if let Some(caret) = prepaint.caret.take() {
                window.paint_quad(caret);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_line = Some(prepaint.line.clone());
            input.last_font = Some(prepaint.font.clone());
            input.last_bounds = Some(bounds);
            input.scroll_x = prepaint.scroll_x;
        });
    }
}

impl Render for SearchInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        rsx! {
            <div flex items_center flex_1 min_w={px(0.)} h={px(self.height)}
                px={px(self.horizontal_padding)} bg={self.colors.background}
                text_color={self.colors.foreground} text_size={px(self.font_size)}
                line_height={px(self.font_size + 4.)} overflow_hidden />
        }
        .key_context("ClipSearchInput")
        .track_focus(&self.focus)
        .cursor(CursorStyle::IBeam)
        .on_action(cx.listener(Self::left))
        .on_action(cx.listener(Self::right))
        .on_action(cx.listener(Self::select_left))
        .on_action(cx.listener(Self::select_right))
        .on_action(cx.listener(Self::home))
        .on_action(cx.listener(Self::end))
        .on_action(cx.listener(Self::select_home))
        .on_action(cx.listener(Self::select_end))
        .on_action(cx.listener(Self::select_all))
        .on_action(cx.listener(Self::backspace))
        .on_action(cx.listener(Self::delete))
        .on_action(cx.listener(Self::copy))
        .on_action(cx.listener(Self::cut))
        .on_action(cx.listener(Self::paste))
        .on_action(cx.listener(Self::submit))
        .on_key_down(cx.listener(Self::key_down))
        .on_action(cx.listener(Self::escape))
        .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
        .on_mouse_move(cx.listener(Self::mouse_move))
        .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
        .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
        .child(InputText { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_drag_preserves_click_and_emits_only_once_after_threshold() {
        let origin = point(px(12.0), px(18.0));
        let mut drag = WindowDragGesture {
            origin: Some(origin),
        };
        assert_eq!(drag.update(point(px(14.0), px(18.0)), true), None);
        assert_eq!(drag.update(point(px(15.0), px(18.0)), true), Some(origin));
        assert_eq!(drag.update(point(px(30.0), px(18.0)), true), None);
    }

    #[test]
    fn window_drag_release_disarms_a_candidate_before_a_later_move() {
        let mut drag = WindowDragGesture {
            origin: Some(point(px(12.0), px(18.0))),
        };
        assert_eq!(drag.update(point(px(12.0), px(18.0)), false), None);
        assert_eq!(drag.update(point(px(-10.0), px(-10.0)), true), None);
        drag.origin = Some(point(px(12.0), px(18.0)));
        assert_eq!(
            drag.update(point(px(-10.0), px(-10.0)), true),
            Some(point(px(12.0), px(18.0)))
        );
    }

    #[test]
    fn chinese_emoji_utf16_ranges_never_split_surrogates_or_utf8() {
        let text = "中😀文";
        assert_eq!(from_utf16(text, 1..3), 3..7);
        assert_eq!(to_utf16(text, 3..7), 1..3);
        assert_eq!(from_utf16(text, 2..2), 3..3);
        assert_eq!(from_utf16(text, 2..3), 3..7);
        assert_eq!(from_utf16(text, 99..100), 10..10);
        assert_eq!(to_utf16(text, 4..99), 1..4);
    }

    #[test]
    fn ime_selection_is_relative_to_replacement_not_surrounding_text() {
        let mut state = TextState {
            text: "前旧后".into(),
            selection: 3..6,
            ..TextState::default()
        };
        state.replace_marked(None, "😀中", Some(2..3));
        assert_eq!(state.text, "前😀中后");
        assert_eq!(state.marked, Some(3..10));
        assert_eq!(state.selection, 7..10);
        assert_eq!(to_utf16(&state.text, state.selection.clone()), 3..4);
        state.replace(None, "新");
        assert_eq!(state.text, "前新后");
        assert_eq!(state.selection, 6..6);
        assert_eq!(state.marked, None);
    }

    #[test]
    fn navigation_deletes_entire_emoji_grapheme_and_preserves_selection_anchor() {
        let emoji = "👨‍👩‍👧‍👦";
        let mut state = TextState {
            text: format!("甲{emoji}乙"),
            ..TextState::default()
        };
        state.move_to(3 + emoji.len());
        assert_eq!(state.previous(), 3);
        state.select_to(state.previous());
        state.replace(None, "");
        assert_eq!(state.text, "甲乙");
        state.move_to(3);
        state.select_to(0);
        assert!(state.reversed);
        state.select_to(6);
        assert_eq!(state.selection, 3..6);
        assert!(!state.reversed);
    }

    #[test]
    fn ime_replaces_the_marked_range_even_after_its_selection_moves() {
        let mut state = TextState {
            text: "已有".into(),
            selection: 6..6,
            ..TextState::default()
        };
        state.replace_marked(None, "zhong", Some(3..3));
        assert_eq!(state.selection, 9..9);
        state.replace_marked(None, "中", Some(1..1));
        assert_eq!(state.text, "已有中");
        assert_eq!(state.selection, 9..9);
        state.replace(None, "中文");
        assert_eq!(state.text, "已有中文");
        assert_eq!(state.selection, 12..12);
        assert_eq!(state.marked, None);
    }
}
