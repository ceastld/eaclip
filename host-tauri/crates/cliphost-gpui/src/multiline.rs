//! Native multiline text input shared by history, favorites and text actions.
//! Uses the same UTF-16/grapheme/IME state as the single-line search control.
//! Newlines stay in the original payload, including CRLF read from the store.

use gpui_rsx::rsx;
use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem,
    ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, KeyDownEvent, LayoutId,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollWheelEvent, SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle,
    Window, WrappedLine,
};

use crate::input::{floor_boundary, from_utf16, to_utf16, InputColors, TextState};

actions!(
    clip_multiline,
    [
        Backspace,
        Delete,
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
        SelectAll,
        Copy,
        Cut,
        Paste
    ]
);

#[derive(Debug, Clone)]
pub enum MultilineEvent {
    Changed(String),
    Save,
    /// Only emitted by inputs explicitly configured for chat-style Enter.
    Submit,
    Escape,
    /// LAN delegates the user gesture to its serialized background worker.
    PasteRequested(PasteTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteTarget {
    revision: u64,
    selection: Range<usize>,
}

fn paste_target_matches(
    target: &PasteTarget,
    revision: u64,
    selection: &Range<usize>,
    composing: bool,
    read_only: bool,
) -> bool {
    target.revision == revision && &target.selection == selection && !composing && !read_only
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterAction {
    Propagate,
    Ignore,
    Newline,
    Submit,
}

fn enter_action(
    submit_on_enter: bool,
    modifiers: Modifiers,
    held: bool,
    composing: bool,
    read_only: bool,
) -> EnterAction {
    let plain_enter = !modifiers.modified();
    let shift_enter = modifiers.shift && modifiers.number_of_modifiers() == 1;
    let control_enter = modifiers.control && modifiers.number_of_modifiers() == 1;
    if !plain_enter && !(submit_on_enter && (shift_enter || control_enter)) {
        return EnterAction::Propagate;
    }
    // A held composition commit must not turn into a send or a new line.
    if held || composing || read_only {
        return EnterAction::Ignore;
    }
    if submit_on_enter && (plain_enter || control_enter) {
        EnterAction::Submit
    } else {
        EnterAction::Newline
    }
}

#[derive(Clone)]
struct LogicalLine {
    range: Range<usize>,
    line: WrappedLine,
    y: Pixels,
}

#[derive(Clone, Default)]
struct TextLayout {
    lines: Vec<LogicalLine>,
    height: Pixels,
}

/// Compact chat inputs opt in; history and property editors retain their
/// existing fixed viewport, line height and padding.
#[derive(Clone, Copy, PartialEq)]
struct AutoGrow {
    min_height: f32,
    max_height: f32,
}

impl AutoGrow {
    const LINE_HEIGHT: f32 = 20.;
    const VERTICAL_PADDING: f32 = 2.;

    fn new(min_height: f32, max_height: f32) -> Self {
        let min_height = min_height.max(Self::LINE_HEIGHT + Self::VERTICAL_PADDING * 2.);
        Self {
            min_height,
            max_height: max_height.max(min_height),
        }
    }

    fn height(self, empty: bool, content_height: Pixels) -> f32 {
        if empty {
            // A wrapped placeholder or the previous draft's layout must not
            // keep an empty composer expanded after sending or clearing.
            return self.min_height;
        }
        (f32::from(content_height) + Self::VERTICAL_PADDING * 2.)
            .max(self.min_height)
            .min(self.max_height)
    }
}

impl TextLayout {
    fn position(&self, offset: usize, line_height: Pixels) -> Point<Pixels> {
        let logical = self
            .lines
            .iter()
            .find(|line| offset <= line.range.end)
            .or_else(|| self.lines.last());
        let Some(logical) = logical else {
            return point(px(0.), px(0.));
        };
        let index = offset
            .saturating_sub(logical.range.start)
            .min(logical.line.len());
        let position = logical
            .line
            .position_for_index(index, line_height)
            .unwrap_or_else(|| point(px(0.), px(0.)));
        point(position.x, logical.y + position.y)
    }

    fn index_at(&self, position: Point<Pixels>, line_height: Pixels) -> usize {
        let Some(logical) = self
            .lines
            .iter()
            .find(|logical| position.y < logical.y + logical.line.size(line_height).height)
            .or_else(|| self.lines.last())
        else {
            return 0;
        };
        let local = point(position.x, (position.y - logical.y).max(px(0.)));
        let index = logical
            .line
            .closest_index_for_position(local, line_height)
            .unwrap_or_else(|index| index);
        logical.range.start + index.min(logical.line.len())
    }
}

pub struct MultilineInput {
    state: TextState,
    focus: FocusHandle,
    colors: InputColors,
    height: f32,
    font_size: f32,
    horizontal_padding: f32,
    auto_grow: Option<AutoGrow>,
    auto_grow_width: Option<Pixels>,
    auto_grow_requested_width: Option<Pixels>,
    last_committed: String,
    layout: Option<TextLayout>,
    layout_width: Option<Pixels>,
    last_bounds: Option<Bounds<Pixels>>,
    last_font: Option<gpui::Font>,
    scroll_y: Pixels,
    reveal_caret: bool,
    selecting: bool,
    read_only: bool,
    external_paste: bool,
    submit_on_enter: bool,
    edit_revision: u64,
    placeholder: SharedString,
}

impl EventEmitter<MultilineEvent> for MultilineInput {}

impl MultilineInput {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let context = Some("ClipMultilineInput");
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
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
            KeyBinding::new("ctrl-a", SelectAll, context),
            KeyBinding::new("ctrl-c", Copy, context),
            KeyBinding::new("ctrl-x", Cut, context),
            KeyBinding::new("ctrl-v", Paste, context),
        ]);
        Self {
            state: TextState::default(),
            focus: cx.focus_handle(),
            colors: InputColors::default(),
            height: 220.,
            font_size: 13.,
            horizontal_padding: 8.,
            auto_grow: None,
            auto_grow_width: None,
            auto_grow_requested_width: None,
            last_committed: String::new(),
            layout: None,
            layout_width: None,
            last_bounds: None,
            last_font: None,
            scroll_y: px(0.),
            reveal_caret: true,
            selecting: false,
            read_only: false,
            external_paste: false,
            submit_on_enter: false,
            edit_revision: 0,
            placeholder: "".into(),
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

    #[cfg(debug_assertions)]
    pub(crate) fn fixture_is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn set_text(&mut self, text: String, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_revision = self.edit_revision.wrapping_add(1);
        self.state.text = text;
        self.state.marked = None;
        self.state.move_to(0);
        self.last_committed.clone_from(&self.state.text);
        self.layout = None;
        self.scroll_y = px(0.);
        self.reveal_caret = true;
        cx.notify();
    }

    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.read_only == read_only {
            return;
        }
        self.read_only = read_only;
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub(crate) fn fixture_layout_snapshot(&self) -> serde_json::Value {
        let bounds = self.last_bounds.map(|bounds| serde_json::json!({
            "x":f32::from(bounds.left()), "y":f32::from(bounds.top()),
            "width":f32::from(bounds.size.width), "height":f32::from(bounds.size.height),
            "right":f32::from(bounds.right()), "bottom":f32::from(bounds.bottom()),
        }));
        let height = self.layout.as_ref().map(|layout| f32::from(layout.height));
        serde_json::json!({"text":self.state.text, "inputBounds":bounds,
            "layoutHeight":height, "lineCount":height.map(|height|
                (height / f32::from(self.line_height())).round() as usize)})
    }

    pub fn set_external_paste(&mut self, enabled: bool, _: &mut Context<Self>) {
        self.external_paste = enabled;
    }

    pub fn set_submit_on_enter(&mut self, enabled: bool, _: &mut Context<Self>) {
        self.submit_on_enter = enabled;
    }

    pub fn set_placeholder(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        if self.placeholder == text {
            return;
        }
        self.placeholder = text;
        self.layout = None;
        cx.notify();
    }

    pub fn paste_target(&self) -> PasteTarget {
        PasteTarget {
            revision: self.edit_revision,
            selection: self.state.selection.clone(),
        }
    }

    pub fn paste_target_is_current(&self, target: &PasteTarget) -> bool {
        paste_target_matches(
            target,
            self.edit_revision,
            &self.state.selection,
            self.is_composing(),
            self.read_only,
        )
    }

    /// Empty text validates an attachment-only paste without deleting selection.
    pub fn apply_external_paste(
        &mut self,
        target: &PasteTarget,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.paste_target_is_current(target) {
            return false;
        }
        if !text.is_empty() {
            self.replace_text_in_range(None, text, window, cx);
        }
        true
    }

    pub fn set_colors(&mut self, colors: InputColors, cx: &mut Context<Self>) {
        if self.colors.background == colors.background
            && self.colors.foreground == colors.foreground
            && self.colors.placeholder == colors.placeholder
            && self.colors.border == colors.border
            && self.colors.selection == colors.selection
            && self.colors.caret == colors.caret
        {
            return;
        }
        self.colors = colors;
        self.layout = None;
        cx.notify();
    }

    pub fn set_metrics(
        &mut self,
        height: f32,
        font_size: f32,
        horizontal_padding: f32,
        cx: &mut Context<Self>,
    ) {
        let height = if self.auto_grow.is_some() {
            self.height
        } else {
            height.max(1.)
        };
        let font_size = font_size.max(1.);
        let horizontal_padding = horizontal_padding.max(0.);
        if self.height == height
            && self.font_size == font_size
            && self.horizontal_padding == horizontal_padding
        {
            return;
        }
        self.height = height;
        self.font_size = font_size;
        self.horizontal_padding = horizontal_padding;
        self.layout = None;
        cx.notify();
    }

    /// Opt in to the compact 20 DIP chat line with 2 DIP vertical padding.
    /// Call `auto_grow_height` with the input's available outer width when its
    /// parent renders; text layout is shared with caret navigation and paint.
    pub fn set_auto_grow(&mut self, min_height: f32, max_height: f32, cx: &mut Context<Self>) {
        let auto_grow = AutoGrow::new(min_height, max_height);
        if self.auto_grow == Some(auto_grow) {
            return;
        }
        self.auto_grow = Some(auto_grow);
        self.height = auto_grow.min_height;
        self.layout = None;
        self.reveal_caret = true;
        cx.notify();
    }

    /// `width` includes this input's horizontal padding, but excludes sibling
    /// buttons and gaps. Width changes and restored drafts invalidate the same
    /// shaping cache used by the text element; idle renders never shape again.
    pub fn auto_grow_height(
        &mut self,
        width: f32,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> f32 {
        let Some(auto_grow) = self.auto_grow else {
            return self.height;
        };
        let width = px((width - self.horizontal_padding * 2.).max(1.));
        if self.auto_grow_requested_width != Some(width) {
            self.auto_grow_requested_width = Some(width);
            self.auto_grow_width = Some(width);
        }
        // Once flex layout provides its actual pixel-snapped width, preserve it
        // until the parent width changes. An estimated fractional width must not
        // cause a second shape on every otherwise idle render.
        let width = self.auto_grow_width.unwrap_or(width);
        self.update_auto_grow_height(auto_grow, width, window);
        self.height
    }

    fn update_auto_grow_height(&mut self, auto_grow: AutoGrow, width: Pixels, window: &mut Window) {
        self.ensure_layout(width, window);
        self.height = auto_grow.height(
            self.state.text.is_empty(),
            self.layout.as_ref().map_or(px(0.), |layout| layout.height),
        );
    }

    fn line_height(&self) -> Pixels {
        px(if self.auto_grow.is_some() {
            AutoGrow::LINE_HEIGHT
        } else {
            self.font_size + 6.
        })
    }

    fn vertical_padding(&self) -> f32 {
        if self.auto_grow.is_some() {
            AutoGrow::VERTICAL_PADDING
        } else {
            6.
        }
    }

    fn ensure_layout(&mut self, width: Pixels, window: &mut Window) {
        let width = width.max(px(1.));
        if self.layout_width != Some(width) {
            self.layout = None;
        }
        self.layout_width = Some(width);
        // IME range queries also arrive outside the input's inherited style
        // scope. Preserve the font observed by prepaint for those queries.
        if self.last_font.is_none() {
            self.last_font = Some(window.text_style().font());
        }
        if self.layout.is_none() {
            self.layout = Some(self.shape(width, window));
        }
    }

    fn committed_changed(&mut self, cx: &mut Context<Self>) {
        self.layout = None;
        self.reveal_caret = true;
        if self.last_committed != self.state.text {
            self.edit_revision = self.edit_revision.wrapping_add(1);
            self.last_committed.clone_from(&self.state.text);
            cx.emit(MultilineEvent::Changed(self.state.text.clone()));
        }
        cx.notify();
    }

    fn navigate(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        if self.is_composing() {
            return;
        }
        if select {
            self.state.select_to(offset);
        } else {
            self.state.move_to(offset);
        }
        self.reveal_caret = true;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.state.selection.is_empty() {
            self.state.previous()
        } else {
            self.state.selection.start
        };
        self.navigate(offset, false, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.state.selection.is_empty() {
            self.state.next()
        } else {
            self.state.selection.end
        };
        self.navigate(offset, false, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(self.state.previous(), true, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(self.state.next(), true, cx);
    }

    fn vertical(&mut self, direction: f32, select: bool, cx: &mut Context<Self>) {
        if let Some(layout) = &self.layout {
            let position = layout.position(self.state.cursor(), self.line_height());
            let target = point(
                position.x,
                (position.y + self.line_height() * direction).max(px(0.)),
            );
            let offset = layout.index_at(target, self.line_height());
            self.navigate(offset, select, cx);
        }
    }
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1., false, cx);
    }
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1., false, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1., true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1., true, cx);
    }

    fn line_edge(&mut self, end: bool, select: bool, cx: &mut Context<Self>) {
        let cursor = self.state.cursor();
        let offset = if end {
            self.state.text[cursor..]
                .find('\n')
                .map(|i| cursor + i)
                .unwrap_or(self.state.text.len())
        } else {
            self.state.text[..cursor]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        let offset = if end && offset > 0 && self.state.text.as_bytes()[offset - 1] == b'\r' {
            offset - 1
        } else {
            offset
        };
        self.navigate(offset, select, cx);
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(false, false, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(true, false, cx);
    }
    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(false, true, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(true, true, cx);
    }
    fn document_home(&mut self, _: &DocumentHome, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(0, false, cx);
    }
    fn document_end(&mut self, _: &DocumentEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.navigate(self.state.text.len(), false, cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() {
            self.state.move_to(0);
            self.navigate(self.state.text.len(), true, cx);
        }
    }
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only {
            if self.state.selection.is_empty() {
                self.state.select_to(self.state.previous());
            }
            self.replace_text_in_range(None, "", window, cx);
        }
    }
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only {
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
        if !self.is_composing() && !self.read_only && !self.state.selection.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_composing() && !self.read_only {
            if self.external_paste {
                cx.stop_propagation();
                cx.emit(MultilineEvent::PasteRequested(self.paste_target()));
                return;
            }
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.replace_text_in_range(None, &text, window, cx);
            }
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self.read_only && (key == "escape" || event.keystroke.modifiers.control && key == "s") {
            return;
        }
        if event.keystroke.modifiers.control && key == "s" {
            cx.stop_propagation();
            if !event.is_held && !self.is_composing() {
                cx.emit(MultilineEvent::Save);
            }
        } else if matches!(key, "pageup" | "pagedown") {
            // A text area owns paging keys (as in a browser); don't let an
            // enclosing page scroll underneath the caret.
            cx.stop_propagation();
        } else if key == "enter" {
            // Key bindings run before this listener, so Enter stays unbound
            // and composition/held guards decide whether it can be handled.
            let action = enter_action(
                self.submit_on_enter,
                event.keystroke.modifiers,
                event.is_held,
                self.is_composing(),
                self.read_only,
            );
            if action != EnterAction::Propagate {
                cx.stop_propagation();
            }
            match action {
                EnterAction::Newline => self.replace_text_in_range(None, "\n", window, cx),
                EnterAction::Submit => cx.emit(MultilineEvent::Submit),
                EnterAction::Propagate | EnterAction::Ignore => {}
            }
        } else if matches!(key, "escape" | "tab")
            && event.keystroke.modifiers.number_of_modifiers() == 0
        {
            // Held IME commit/cancel keys must never insert a tab or close.
            cx.stop_propagation();
            if event.is_held || self.is_composing() {
                return;
            }
            match key {
                "tab" if !self.read_only => self.replace_text_in_range(None, "\t", window, cx),
                "escape" => cx.emit(MultilineEvent::Escape),
                _ => {}
            }
        }
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        let (Some(layout), Some(bounds)) = (&self.layout, self.last_bounds) else {
            return 0;
        };
        let local = point(
            position.x - bounds.left(),
            position.y - bounds.top() + self.scroll_y,
        );
        floor_boundary(&self.state.text, layout.index_at(local, self.line_height()))
    }
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        cx.stop_propagation();
        if self.is_composing() {
            return;
        }
        self.selecting = true;
        let offset = self.index_at(event.position);
        if event.modifiers.shift {
            self.state.select_to(offset);
        } else {
            self.state.move_to(offset);
        }
        if event.click_count >= 2 {
            // Select the word under the pointer without altering the payload.
            let mut start = offset;
            let mut end = offset;
            while start > 0 {
                let (i, ch) = self.state.text[..start].char_indices().next_back().unwrap();
                if ch.is_whitespace() {
                    break;
                }
                start = i;
            }
            while let Some(ch) = self.state.text[end..].chars().next() {
                if ch.is_whitespace() {
                    break;
                }
                end += ch.len_utf8();
            }
            self.state.move_to(start);
            self.state.select_to(end);
            self.selecting = false;
        }
        cx.notify();
    }
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            self.state.select_to(self.index_at(event.position));
            self.reveal_caret = true;
            cx.notify();
        }
    }
    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }
    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = &self.layout else {
            return;
        };
        let viewport = self
            .last_bounds
            .map(|bounds| bounds.size.height)
            .unwrap_or(px(self.height));
        // Fitted readonly property values have no inner scroll. Let their
        // containing property page receive the wheel gesture instead.
        if layout.height <= viewport {
            return;
        }
        self.scroll_y = (self.scroll_y - event.delta.pixel_delta(self.line_height()).y)
            .max(px(0.))
            .min((layout.height - viewport).max(px(0.)));
        self.reveal_caret = false;
        cx.stop_propagation();
        cx.notify();
    }

    fn shape(&self, width: Pixels, window: &mut Window) -> TextLayout {
        let mut layout = TextLayout::default();
        let font = self
            .last_font
            .clone()
            .unwrap_or_else(|| window.text_style().font());
        let placeholder = self.state.text.is_empty();
        let displayed = if placeholder {
            self.placeholder.as_ref()
        } else {
            &self.state.text
        };
        for range in logical_ranges(displayed) {
            let text: SharedString = displayed[range.clone()].to_string().into();
            let base = TextRun {
                len: text.len(),
                font: font.clone(),
                color: if placeholder {
                    self.colors.placeholder
                } else {
                    self.colors.foreground
                },
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = if let Some(marked) = self
                .state
                .marked
                .as_ref()
                .filter(|marked| marked.start < range.end && marked.end > range.start)
            {
                let start = marked.start.max(range.start) - range.start;
                let end = marked.end.min(range.end) - range.start;
                vec![
                    TextRun {
                        len: start,
                        ..base.clone()
                    },
                    TextRun {
                        len: end - start,
                        underline: Some(UnderlineStyle {
                            color: Some(base.color),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        ..base.clone()
                    },
                    TextRun {
                        len: text.len() - end,
                        ..base
                    },
                ]
            } else {
                vec![base]
            };
            if let Ok(lines) = window.text_system().shape_text(
                text,
                px(self.font_size),
                &runs,
                Some(width.max(px(1.))),
                None,
            ) {
                if let Some(line) = lines.into_iter().next() {
                    let y = layout.height;
                    layout.height += line.size(self.line_height()).height;
                    layout.lines.push(LogicalLine { range, line, y });
                }
            }
        }
        layout
    }

    fn reveal(&mut self, layout: &TextLayout, bounds: Bounds<Pixels>) {
        if self.reveal_caret {
            let caret = layout.position(self.state.cursor(), self.line_height());
            if caret.y < self.scroll_y {
                self.scroll_y = caret.y;
            } else if caret.y + self.line_height() > self.scroll_y + bounds.size.height {
                self.scroll_y = caret.y + self.line_height() - bounds.size.height;
            }
        }
        self.scroll_y = self
            .scroll_y
            .max(px(0.))
            .min((layout.height - bounds.size.height).max(px(0.)));
    }
}

/// Empty final lines participate in caret navigation. CRLF is excluded only
/// from painting; editing and saving never normalize the original bytes.
fn logical_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            let end = if index > start && text.as_bytes()[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            ranges.push(start..end);
            start = index + 1;
        }
    }
    ranges.push(start..text.len());
    ranges
}

impl Focusable for MultilineInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for MultilineInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.state.text, range);
        *adjusted_range = Some(to_utf16(&self.state.text, range.clone()));
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
        self.state.replace_multiline(range, text);
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
        self.state
            .replace_marked_multiline(range, text, selected_range);
        self.layout = None;
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
        self.ensure_layout(bounds.size.width, window);
        let layout = self.layout.as_ref()?.clone();
        self.reveal(&layout, bounds);
        let start = layout.position(range.start, self.line_height());
        let end = layout.position(range.end, self.line_height());
        let x = (bounds.left() + start.x)
            .max(bounds.left())
            .min((bounds.right() - px(1.)).max(bounds.left()));
        let y = (bounds.top() + start.y - self.scroll_y)
            .max(bounds.top())
            .min((bounds.bottom() - self.line_height()).max(bounds.top()));
        let width = if end.y == start.y {
            (end.x - start.x).max(px(1.))
        } else {
            px(1.)
        };
        self.layout = Some(layout);
        Some(Bounds::new(
            point(x, y),
            size(
                width.min(bounds.size.width),
                self.line_height().min(bounds.size.height),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_bounds?;
        let byte = self.index_at(point);
        Some(to_utf16(&self.state.text, byte..byte).start)
    }
}

struct TextElement {
    input: Entity<MultilineInput>,
}
struct Prepaint {
    layout: TextLayout,
    font: gpui::Font,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;
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
    ) -> Prepaint {
        self.input.update(cx, |input, cx| {
            let font = window.text_style().font();
            if input.last_font.as_ref() != Some(&font) {
                input.layout = None;
            }
            input.last_font = Some(font.clone());
            if input.auto_grow.is_some() {
                // Keep the actual flex layout width for IME-only input renders,
                // which deliberately do not emit a committed Changed event.
                input.auto_grow_width = Some(bounds.size.width);
            }
            input.ensure_layout(bounds.size.width, window);
            let layout = input.layout.as_ref().unwrap().clone();
            if let Some(auto_grow) = input.auto_grow {
                let height = auto_grow.height(input.state.text.is_empty(), layout.height);
                if input.height != height {
                    // Flex/pixel snapping can slightly change the estimated
                    // parent width. Settle the exact height on the next frame.
                    input.height = height;
                    cx.notify();
                }
            }
            input.reveal(&layout, bounds);
            Prepaint { layout, font }
        })
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let input = self.input.read(cx);
        let focus = input.focus.clone();
        let focused = focus.is_focused(window);
        let scroll_y = input.scroll_y;
        let line_height = input.line_height();
        let selection = input.state.selection.clone();
        let colors = input.colors;
        let cursor = input.state.cursor();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for logical in &prepaint.layout.lines {
                let top = bounds.top() + logical.y - scroll_y;
                if top + logical.line.size(line_height).height < bounds.top()
                    || top > bounds.bottom()
                {
                    continue;
                }
                if focused
                    && !selection.is_empty()
                    && selection.start <= logical.range.end
                    && selection.end > logical.range.start
                {
                    let start = selection
                        .start
                        .saturating_sub(logical.range.start)
                        .min(logical.line.len());
                    let end = selection
                        .end
                        .saturating_sub(logical.range.start)
                        .min(logical.line.len());
                    let p1 = logical
                        .line
                        .position_for_index(start, line_height)
                        .unwrap_or_default();
                    let p2 = logical
                        .line
                        .position_for_index(end, line_height)
                        .unwrap_or_default();
                    let first_row = (p1.y / line_height) as usize;
                    let last_row = (p2.y / line_height) as usize;
                    for row in first_row..=last_row {
                        let left = if row == first_row { p1.x } else { px(0.) };
                        let right = if row == last_row {
                            p2.x.max(left + px(2.))
                        } else {
                            bounds.size.width
                        };
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.left() + left, top + line_height * row),
                                size(right - left, line_height),
                            ),
                            colors.selection,
                        ));
                    }
                }
                let _ = logical.line.paint(
                    point(bounds.left(), top),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if focused && selection.is_empty() {
                let position = prepaint.layout.position(cursor, line_height);
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.left() + position.x,
                            bounds.top() + position.y - scroll_y,
                        ),
                        size(px(1.), line_height),
                    ),
                    colors.caret,
                ));
            }
        });
        self.input.update(cx, |input, _| {
            input.layout = Some(prepaint.layout.clone());
            input.last_font = Some(prepaint.font.clone());
            input.last_bounds = Some(bounds);
            input.reveal_caret = false;
        });
    }
}

impl Render for MultilineInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let (Some(auto_grow), Some(width)) = (self.auto_grow, self.auto_grow_width) {
            // Composition updates redraw this entity before the parent receives
            // committed text. Size from the marked text without changing IME,
            // selection, revision or pending asynchronous paste state.
            self.update_auto_grow_height(auto_grow, width, window);
        }
        rsx! {
            <div flex w_full min_w={px(0.)} h={px(self.height)} px={px(self.horizontal_padding)}
                py={px(self.vertical_padding())} bg={self.colors.background} text_color={self.colors.foreground}
                text_size={px(self.font_size)} line_height={self.line_height()} overflow_hidden />
        }
            .key_context("ClipMultilineInput")
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
            .child(TextElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_composer_uses_shaped_wrapped_rows_and_caps_its_scroll_viewport() {
        let auto_grow = AutoGrow::new(24., 124.);
        // Shaping reports the same row height for soft wrapping and explicit
        // newlines. The seventh row scrolls within the six-row viewport.
        for (rows, expected) in [(1, 24.), (2, 44.), (6, 124.), (7, 124.)] {
            assert_eq!(
                auto_grow.height(false, px(AutoGrow::LINE_HEIGHT * rows as f32)),
                expected,
            );
        }
    }

    #[test]
    fn compact_composer_clearing_always_returns_to_one_line() {
        let auto_grow = AutoGrow::new(24., 124.);
        assert_eq!(auto_grow.height(false, px(240.)), 124.);
        // Ignore both stale draft height and a multi-row placeholder.
        assert_eq!(auto_grow.height(true, px(240.)), 24.);
        assert_eq!(auto_grow.height(true, px(0.)), 24.);
        assert_eq!(auto_grow.height(false, px(20.)), 24.);
    }

    #[test]
    fn compact_composer_growth_keeps_marked_text_and_unicode_selection_intact() {
        let auto_grow = AutoGrow::new(24., 124.);
        let mut state = TextState {
            text: "前\r\n后".into(),
            selection: 5..5,
            ..Default::default()
        };
        state.replace_marked_multiline(None, "😀\n中", Some(3..4));
        let text = state.text.clone();
        let marked = state.marked.clone();
        let selection = state.selection.clone();
        let rows = logical_ranges(&state.text).len();
        assert_eq!(rows, 3);
        assert_eq!(
            auto_grow.height(state.text.is_empty(), px(rows as f32 * 20.)),
            64.,
        );
        assert!(marked.is_some());
        assert_eq!(state.text, text);
        assert_eq!(state.marked, marked);
        assert_eq!(state.selection, selection);
    }

    #[test]
    fn compact_composer_limits_always_fit_the_line_and_padding() {
        let auto_grow = AutoGrow::new(0., 1.);
        assert_eq!(auto_grow.height(true, px(0.)), 24.);
        assert_eq!(auto_grow.height(false, px(100.)), 24.);
    }

    #[test]
    fn chat_enter_submits_and_only_shift_enter_inserts_a_newline() {
        for bits in 0..32 {
            let modifiers = Modifiers {
                control: bits & 1 != 0,
                alt: bits & 2 != 0,
                shift: bits & 4 != 0,
                platform: bits & 8 != 0,
                function: bits & 16 != 0,
            };
            let expected = match bits {
                0 | 1 => EnterAction::Submit,
                4 => EnterAction::Newline,
                _ => EnterAction::Propagate,
            };
            assert_eq!(
                enter_action(true, modifiers, false, false, false),
                expected,
                "modifiers {modifiers:?}",
            );
        }
    }

    #[test]
    fn chat_enter_never_submits_or_inserts_while_composing_held_or_read_only() {
        for modifiers in [
            Modifiers::none(),
            Modifiers {
                shift: true,
                ..Modifiers::none()
            },
            Modifiers {
                control: true,
                ..Modifiers::none()
            },
        ] {
            for (held, composing, read_only) in [
                (true, false, false),
                (false, true, false),
                (false, false, true),
                (true, true, true),
            ] {
                assert_eq!(
                    enter_action(true, modifiers, held, composing, read_only),
                    EnterAction::Ignore,
                );
            }
        }
    }

    #[test]
    fn chat_shift_enter_replaces_the_unicode_selection_without_losing_surrounding_text() {
        let mut state = TextState {
            text: "前😀后".into(),
            selection: 3..7,
            ..Default::default()
        };
        let action = enter_action(
            true,
            Modifiers {
                shift: true,
                ..Modifiers::none()
            },
            false,
            state.marked.is_some(),
            false,
        );
        assert_eq!(action, EnterAction::Newline);
        if action == EnterAction::Newline {
            state.replace_multiline(None, "\n");
        }
        assert_eq!(state.text, "前\n后");
        assert_eq!(state.selection, 4..4);
        assert_eq!(state.marked, None);
    }

    #[test]
    fn ordinary_multiline_editors_keep_their_existing_enter_behavior() {
        assert_eq!(
            enter_action(false, Modifiers::none(), false, false, false),
            EnterAction::Newline,
        );
        assert_eq!(
            enter_action(
                false,
                Modifiers {
                    shift: true,
                    ..Modifiers::none()
                },
                false,
                false,
                false,
            ),
            EnterAction::Propagate,
        );
        assert_eq!(
            enter_action(
                false,
                Modifiers {
                    control: true,
                    ..Modifiers::none()
                },
                false,
                false,
                false,
            ),
            EnterAction::Propagate,
        );
        for (held, composing, read_only) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert_eq!(
                enter_action(false, Modifiers::none(), held, composing, read_only),
                EnterAction::Ignore,
            );
        }
    }

    #[test]
    fn asynchronous_paste_requires_the_same_edit_selection_and_non_ime_target() {
        let target = PasteTarget {
            revision: 8,
            selection: 1..5,
        };
        assert!(paste_target_matches(&target, 8, &(1..5), false, false));
        assert!(!paste_target_matches(&target, 9, &(1..5), false, false));
        assert!(!paste_target_matches(&target, 8, &(5..5), false, false));
        assert!(!paste_target_matches(&target, 8, &(1..5), true, false));
        assert!(!paste_target_matches(&target, 8, &(1..5), false, true));
        let mut state = TextState::default();
        state.text = "A😀B".into();
        state.selection = 1..5;
        state.replace_multiline(None, "图片说明\n");
        assert_eq!(state.text, "A图片说明\nB");
    }
    #[test]
    fn logical_lines_preserve_crlf_and_empty_final_line_offsets() {
        assert_eq!(logical_ranges("中\r\n😀\n"), vec![0..3, 5..9, 10..10]);
        assert_eq!(logical_ranges(""), vec![0..0]);
    }
    #[test]
    fn ime_replacement_preserves_newlines_and_emoji_selection() {
        let mut state = TextState {
            text: "前\r\n后".into(),
            selection: 5..5,
            ..Default::default()
        };
        state.replace_marked_multiline(None, "😀中", Some(2..3));
        assert_eq!(state.text, "前\r\n😀中后");
        assert_eq!(state.selection, 9..12);
        state.replace_multiline(None, "中文\n");
        assert_eq!(state.text, "前\r\n中文\n后");
        assert_eq!(state.marked, None);
    }
}
