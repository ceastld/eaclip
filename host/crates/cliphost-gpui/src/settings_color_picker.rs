//! Settings color popover (B1). The entity edits a private draft and emits
//! intents; the parent owns the text box, saves, focus restoration and lifetime.
//! Layout and commit rules: `docs/gpui-style-motion-alignment.md` section B1.

use std::time::Instant;

use cliphost_core::i18n::tr;
use gpui::{
    anchored, canvas, deferred, div, hsla, linear_color_stop, linear_gradient, point, prelude::*,
    px, quad, size, transparent_black, white, AnimationExt, BorderStyle, Bounds,
    Context, Corner, Corners, DispatchPhase, Edges, ElementId, Entity, EventEmitter, FocusHandle,
    Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    SharedString, Subscription, Window,
};
use crate::crisp_svg::svg;
use gpui_rsx::rsx;

use super::{
    settings_color::ArgbColor,
    settings_color_picker_model::{
        bar_ratio, popup_layout, square_sample, Adjust, ColorModel, EyedropperOutcome,
        EyedropperRuns, PopupState, HUE_MAX, PRESET_COLORS,
    },
    settings_help, SettingsPalette,
};
use crate::{
    input::{InputColors, SearchEvent, SearchInput},
    motion::{self, mix_hsla, Tweens},
    preview::ThemeTokens,
    view::color,
};

/// Content width of the card; the 1px border sits outside it.
const CONTENT_WIDTH: f32 = 220.0;
const PADDING: f32 = 10.0;
const SECTION_GAP: f32 = 8.0;
const SQUARE_HEIGHT: f32 = 140.0;
const BAR_HEIGHT: f32 = 12.0;
const THUMB: f32 = 14.0;
const MARKER: f32 = 12.0;
const COMPARE_SIZE: [f32; 2] = [28.0, 20.0];
const HEX_HEIGHT: f32 = 28.0;
const SWATCH: f32 = 18.0;
const ENTER_OFFSET: f32 = 4.0;
const RECENT_VISIBLE: usize = 8;
const EYEDROPPER_ICON: f32 = 16.0;
/// Shared stroke icon (`ui-design/assets/eyedropper.svg`, single-color mask).
const EYEDROPPER_ASSET: &str = "eyedropper.svg";
/// Checkerboard colors shared with the 22px swatch on the settings row.
const CHECKER_LIGHT: u32 = 0xeeeeee;
const CHECKER_DARK: u32 = 0xb8b8b8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ColorDismissal {
    Escape,
    Outside,
    Enter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ColorPickerEvent {
    /// The draft differs from the value the popover opened with.
    Chosen {
        key: String,
        value: String,
        by: ColorDismissal,
    },
    Dismissed(ColorDismissal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drag {
    Square,
    Hue,
    Alpha,
}

pub(super) fn argb_hsla(color: ArgbColor) -> Hsla {
    let mut fill: Hsla = gpui::rgb(color.rgb()).into();
    fill.a = f32::from(color.a) / 255.0;
    fill
}

/// Paints the two-tone checkerboard under translucent colors. Cells are about
/// `cell` px; the last column and row absorb the remainder. Only the corners that
/// coincide with the outer shape are rounded (`radius` must not exceed a cell).
pub(super) fn paint_checker(window: &mut Window, bounds: Bounds<Pixels>, cell: f32, radius: f32) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width <= 0.0 || height <= 0.0 || cell <= 0.0 {
        return;
    }
    let cols = ((width / cell).floor() as usize).max(1);
    let rows = ((height / cell).floor() as usize).max(1);
    let col_width = |col: usize| {
        if col + 1 == cols {
            width - cell * (cols - 1) as f32
        } else {
            cell
        }
    };
    let row_height = |row: usize| {
        if row + 1 == rows {
            height - cell * (rows - 1) as f32
        } else {
            cell
        }
    };
    let radius = px(radius);
    for row in 0..rows {
        for col in 0..cols {
            let rounded = |on: bool| if on { radius } else { px(0.0) };
            let corners = Corners {
                top_left: rounded(row == 0 && col == 0),
                top_right: rounded(row == 0 && col + 1 == cols),
                bottom_right: rounded(row + 1 == rows && col + 1 == cols),
                bottom_left: rounded(row + 1 == rows && col == 0),
            };
            let origin = point(
                bounds.left() + px(cell * col as f32),
                bounds.top() + px(cell * row as f32),
            );
            let tone = if (row + col) % 2 == 0 {
                CHECKER_LIGHT
            } else {
                CHECKER_DARK
            };
            window.paint_quad(quad(
                Bounds::new(origin, size(px(col_width(col)), px(row_height(row)))),
                corners,
                gpui::rgb(tone),
                Edges::<Pixels>::default(),
                transparent_black(),
                BorderStyle::default(),
            ));
        }
    }
}

/// Absolute, parent-sized checkerboard element.
pub(super) fn checker(cell: f32, radius: f32) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_checker(window, bounds, cell, radius),
    )
    .absolute()
    .size_full()
}

fn paint_square(window: &mut Window, bounds: Bounds<Pixels>, hue: f32) {
    let radius = Corners::all(px(4.0));
    let hue_color = hsla(hue / 360.0, 1.0, 0.5, 1.0);
    for background in [
        linear_gradient(
            90.0,
            linear_color_stop(white(), 0.0),
            linear_color_stop(hue_color, 1.0),
        ),
        linear_gradient(
            180.0,
            linear_color_stop(hsla(0.0, 0.0, 0.0, 0.0), 0.0),
            linear_color_stop(hsla(0.0, 0.0, 0.0, 1.0), 1.0),
        ),
    ] {
        window.paint_quad(quad(
            bounds,
            radius,
            background,
            Edges::<Pixels>::default(),
            transparent_black(),
            BorderStyle::default(),
        ));
    }
}

fn paint_hue_bar(window: &mut Window, bounds: Bounds<Pixels>) {
    const STOPS: [u32; 7] = [
        0xff0000, 0xffff00, 0x00ff00, 0x00ffff, 0x0000ff, 0xff00ff, 0xff0000,
    ];
    let segment = bounds.size.width / 6.0;
    let round = bounds.size.height / 2.0;
    for index in 0..6usize {
        // Neighbours overlap by one pixel so sub-pixel edges leave no seam.
        let width = if index < 5 {
            segment + px(1.0)
        } else {
            segment
        };
        let corners = Corners {
            top_left: if index == 0 { round } else { px(0.0) },
            bottom_left: if index == 0 { round } else { px(0.0) },
            top_right: if index == 5 { round } else { px(0.0) },
            bottom_right: if index == 5 { round } else { px(0.0) },
        };
        window.paint_quad(quad(
            Bounds::new(
                point(bounds.left() + segment * index as f32, bounds.top()),
                size(width, bounds.size.height),
            ),
            corners,
            linear_gradient(
                90.0,
                linear_color_stop(gpui::rgb(STOPS[index]), 0.0),
                linear_color_stop(gpui::rgb(STOPS[index + 1]), 1.0),
            ),
            Edges::<Pixels>::default(),
            transparent_black(),
            BorderStyle::default(),
        ));
    }
}

fn paint_alpha_bar(window: &mut Window, bounds: Bounds<Pixels>, color: Hsla) {
    paint_checker(window, bounds, BAR_HEIGHT / 2.0, BAR_HEIGHT / 2.0);
    window.paint_quad(quad(
        bounds,
        Corners::all(bounds.size.height / 2.0),
        linear_gradient(
            90.0,
            linear_color_stop(Hsla { a: 0.0, ..color }, 0.0),
            linear_color_stop(Hsla { a: 1.0, ..color }, 1.0),
        ),
        Edges::<Pixels>::default(),
        transparent_black(),
        BorderStyle::default(),
    ));
}

/// Checkerboard-backed color chip used for the compare pair and swatch rows.
fn swatch(width: f32, height: f32, radius: f32, cell: f32, fill: Hsla, border: Hsla) -> gpui::Div {
    rsx! {
        <div relative w={px(width)} h={px(height)} flex_none
            rounded={px(radius)} border_1 border_color={border} overflow_hidden>
            {checker(cell, (radius - 1.0).max(0.0))}
            <div absolute size_full rounded={px((radius - 1.0).max(0.0))} bg={fill} />
        </div>
    }
}

fn thumb(left: f32, fill: Hsla) -> gpui::Div {
    rsx! {
        <div absolute left={px(left)} top={px((BAR_HEIGHT - THUMB) / 2.0)} size={px(THUMB)}
            rounded_full border_2 border_color={white()} bg={fill} shadow_sm />
    }
}

fn focus_ring(radius: f32, color: Hsla) -> gpui::Div {
    rsx! {
        <div absolute top={px(-2.0)} left={px(-2.0)} right={px(-2.0)} bottom={px(-2.0)}
            rounded={px(radius + 2.0)} border_1 border_color={color} />
    }
}

pub(super) struct SettingsColorPopup {
    field_key: String,
    state: PopupState,
    palette: SettingsPalette,
    text: Hsla,
    theme: ThemeTokens,
    accent: Hsla,
    error: Hsla,
    anchor: Bounds<Pixels>,
    recents: Vec<ArgbColor>,
    square_focus: FocusHandle,
    hue_focus: FocusHandle,
    alpha_focus: FocusHandle,
    hex: Entity<SearchInput>,
    drag: Option<Drag>,
    square_bounds: Option<Bounds<Pixels>>,
    hue_bounds: Option<Bounds<Pixels>>,
    alpha_bounds: Option<Bounds<Pixels>>,
    /// Screen eyedropper runs (B2); retired whenever the popover closes or drops.
    eyedropper: EyedropperRuns,
    eyedropper_hovered: bool,
    eyedropper_motion: Tweens<()>,
    /// Preset / recent swatch hover wash (`motion::HOVER`); display only.
    swatch_hovered: Option<SharedString>,
    swatch_motion: Tweens<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ColorPickerEvent> for SettingsColorPopup {}

impl SettingsColorPopup {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        field_key: String,
        initial_text: &str,
        recents: Vec<ArgbColor>,
        theme: &ThemeTokens,
        palette: SettingsPalette,
        anchor: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = PopupState::new(initial_text);
        let hex = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(
                InputColors {
                    background: transparent_black(),
                    foreground: color(&theme.text),
                    placeholder: color(&theme.text_muted),
                    border: palette.control_border,
                    selection: color(&theme.tab_active),
                    caret: color(&theme.accent),
                },
                cx,
            );
            input.set_metrics(HEX_HEIGHT - 2.0, 12.0, 8.0, cx);
            input.set_placeholder("#AARRGGBB", cx);
            input.set_double_click_clears(false);
            input.set_text(state.model.hex(), window, cx);
            input
        });
        let hex_focus = hex.read(cx).focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&hex, window, |this, _, event, window, cx| match event {
                // Typing never rewrites the text; only a valid value moves the draft.
                SearchEvent::Changed(text) => {
                    if this.state.is_open() && this.state.model.apply_hex(text) {
                        cx.notify();
                    }
                }
                SearchEvent::Submit => this.confirm(ColorDismissal::Enter, window, cx),
                SearchEvent::Escape => this.cancel(ColorDismissal::Escape, cx),
            }),
            // Leaving the box with half-typed text shows the draft again.
            cx.on_blur(&hex_focus, window, |this, window, cx| {
                this.sync_hex(window, cx)
            }),
        ];
        Self {
            field_key,
            state,
            palette,
            text: color(&theme.text),
            theme: theme.clone(),
            accent: color(&theme.accent),
            error: color(&theme.error),
            anchor,
            recents,
            square_focus: cx.focus_handle(),
            hue_focus: cx.focus_handle(),
            alpha_focus: cx.focus_handle(),
            hex,
            drag: None,
            square_bounds: None,
            hue_bounds: None,
            alpha_bounds: None,
            eyedropper: EyedropperRuns::default(),
            eyedropper_hovered: false,
            eyedropper_motion: Tweens::default(),
            swatch_hovered: None,
            swatch_motion: Tweens::default(),
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.square_focus.focus(window);
        cx.notify();
    }

    /// Supply the trigger's fresh window bounds after scrolling or relayout.
    pub(super) fn set_anchor(&mut self, anchor: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.anchor != anchor {
            self.anchor = anchor;
            cx.notify();
        }
    }

    /// Enter or outside click: commit only when the draft changed.
    pub(super) fn confirm(
        &mut self,
        by: ColorDismissal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.is_open() {
            return;
        }
        self.drag = None;
        self.eyedropper.retire();
        match self.state.confirm() {
            Some(color) => cx.emit(ColorPickerEvent::Chosen {
                key: self.field_key.clone(),
                value: color.serialize(),
                by,
            }),
            None => cx.emit(ColorPickerEvent::Dismissed(by)),
        }
        cx.notify();
    }

    /// Escape: close and keep the original draft in the text box.
    pub(super) fn cancel(&mut self, by: ColorDismissal, cx: &mut Context<Self>) {
        self.drag = None;
        self.eyedropper.retire();
        if self.state.cancel() {
            cx.emit(ColorPickerEvent::Dismissed(by));
            cx.notify();
        }
    }

    /// Opens the full-screen eyedropper on a blocking worker. The worker polls a
    /// lease that turns false when this popover closes or drops, so a retired
    /// popover never keeps a screen-wide overlay up. The sampled RGB only moves
    /// the draft (alpha kept); Enter or an outside click still commits it.
    fn start_eyedropper(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.is_open() || self.eyedropper.is_busy() {
            return;
        }
        #[cfg(windows)]
        let owner = match crate::platform::hwnd(window) {
            Ok(hwnd) => hwnd.0 as isize,
            Err(error) => {
                self.eyedropper.fail(error);
                cx.notify();
                return;
            }
        };
        #[cfg(not(windows))]
        let owner = 0isize;
        let Some(ticket) = self.eyedropper.begin() else {
            return;
        };
        self.drag = None;
        self.eyedropper_hovered = false;
        cx.notify();
        let run = ticket.id();
        cx.spawn_in(window, async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async move {
                    cliphost_core::screen_color_pick::pick_screen_color(owner, &|| {
                        ticket.is_current()
                    })
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_eyedropper(run, picked, window, cx)
            });
        })
        .detach();
    }

    fn finish_eyedropper(
        &mut self,
        run: u64,
        picked: Result<Option<(u8, u8, u8)>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let outcome = self.eyedropper.finish(run, picked);
        if !self.state.is_open() {
            return;
        }
        match outcome {
            EyedropperOutcome::Apply(rgb) => {
                self.state.model.apply_rgb_keeping_alpha(rgb);
                self.draft_changed(window, cx);
            }
            EyedropperOutcome::Cancelled | EyedropperOutcome::Failed => cx.notify(),
            EyedropperOutcome::Stale => return,
        }
        // The overlay held the OS focus; keep Enter / Esc working in the card.
        self.square_focus.focus(window);
    }

    fn sync_hex(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.state.model.hex();
        if self.hex.read(cx).text() != text {
            self.hex
                .update(cx, |input, cx| input.set_text(text, window, cx));
        }
    }

    fn draft_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_hex(window, cx);
        cx.notify();
    }

    fn apply_pointer(
        &mut self,
        kind: Drag,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = &mut self.state.model;
        match kind {
            Drag::Square => {
                if let Some(bounds) = self.square_bounds {
                    if let Some((saturation, value)) = square_sample(
                        f32::from(position.x - bounds.left()),
                        f32::from(position.y - bounds.top()),
                        f32::from(bounds.size.width),
                        f32::from(bounds.size.height),
                    ) {
                        model.set_saturation_value(saturation, value);
                    }
                }
            }
            Drag::Hue => {
                if let Some(bounds) = self.hue_bounds {
                    if let Some(ratio) = bar_ratio(
                        f32::from(position.x - bounds.left()),
                        f32::from(bounds.size.width),
                    ) {
                        model.set_hue(ratio * HUE_MAX);
                    }
                }
            }
            Drag::Alpha => {
                if let Some(bounds) = self.alpha_bounds {
                    if let Some(ratio) = bar_ratio(
                        f32::from(position.x - bounds.left()),
                        f32::from(bounds.size.width),
                    ) {
                        model.set_alpha(ratio);
                    }
                }
            }
        }
        self.draft_changed(window, cx);
    }

    fn begin_drag(
        &mut self,
        kind: Drag,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.prevent_default();
        cx.stop_propagation();
        if !self.state.is_open() {
            return;
        }
        let focus = match kind {
            Drag::Square => &self.square_focus,
            Drag::Hue => &self.hue_focus,
            Drag::Alpha => &self.alpha_focus,
        };
        window.focus(focus);
        self.drag = Some(kind);
        self.apply_pointer(kind, event.position, window, cx);
    }

    /// Window-level so a drag keeps following after the pointer leaves the card.
    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            // A button-up lost outside the window must not leave a stuck drag.
            self.drag = None;
            cx.notify();
            return;
        }
        cx.stop_propagation();
        self.apply_pointer(kind, event.position, window, cx);
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left && self.drag.take().is_some() {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn apply_swatch(&mut self, color: ArgbColor, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.is_open() {
            self.state.model.apply_argb(color);
            self.draft_changed(window, cx);
        }
    }

    fn cycle_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let handles = [
            self.square_focus.clone(),
            self.hue_focus.clone(),
            self.alpha_focus.clone(),
            self.hex.read(cx).focus_handle(),
        ];
        let current = handles.iter().position(|handle| handle.is_focused(window));
        let last = handles.len() - 1;
        let next = match (current, reverse) {
            (None, false) => 0,
            (None, true) => last,
            (Some(index), false) => (index + 1) % handles.len(),
            (Some(index), true) => index.checked_sub(1).unwrap_or(last),
        };
        window.focus(&handles[next]);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.is_open() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "tab" => {
                window.prevent_default();
                cx.stop_propagation();
                if !event.is_held {
                    self.cycle_focus(modifiers.shift, window, cx);
                }
            }
            // The hex box handles its own Enter / Escape and emits intents instead.
            "escape" | "enter" => {
                window.prevent_default();
                cx.stop_propagation();
                if !event.is_held {
                    if key == "escape" {
                        self.cancel(ColorDismissal::Escape, cx);
                    } else {
                        self.confirm(ColorDismissal::Enter, window, cx);
                    }
                }
            }
            "left" | "right" | "up" | "down" => {
                let step = |axis: Adjust, positive: bool| (axis, if positive { 1 } else { -1 });
                let adjustment = if self.square_focus.is_focused(window) {
                    match key {
                        "left" => Some(step(Adjust::Saturation, false)),
                        "right" => Some(step(Adjust::Saturation, true)),
                        "up" => Some(step(Adjust::Value, true)),
                        _ => Some(step(Adjust::Value, false)),
                    }
                } else if self.hue_focus.is_focused(window) {
                    Some(step(Adjust::Hue, matches!(key, "right" | "up")))
                } else if self.alpha_focus.is_focused(window) {
                    Some(step(Adjust::Alpha, matches!(key, "right" | "up")))
                } else {
                    None
                };
                if let Some((axis, direction)) = adjustment {
                    window.prevent_default();
                    cx.stop_propagation();
                    if self.state.model.nudge(axis, direction, modifiers.shift) {
                        self.draft_changed(window, cx);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Render for SettingsColorPopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.state.is_open() {
            return div().into_any_element();
        }
        let palette = self.palette;
        let model: ColorModel = self.state.model;
        let draft = argb_hsla(model.to_argb());
        let opaque = Hsla { a: 1.0, ..draft };
        let hue_color = hsla(model.hue() / 360.0, 1.0, 0.5, 1.0);
        let viewport = window.viewport_size();
        let estimate = 2.0 * (PADDING + 1.0)
            + SQUARE_HEIGHT
            + 2.0 * BAR_HEIGHT
            + HEX_HEIGHT
            + SWATCH
            + 4.0 * SECTION_GAP
            + if self.recents.is_empty() {
                0.0
            } else {
                SWATCH + SECTION_GAP
            };
        let layout = popup_layout(
            [
                f32::from(self.anchor.left()),
                f32::from(self.anchor.top()),
                f32::from(self.anchor.size.width),
                f32::from(self.anchor.size.height),
            ],
            [f32::from(viewport.width), f32::from(viewport.height)],
            CONTENT_WIDTH + 2.0 * (PADDING + 1.0),
            estimate,
        );
        let travel = CONTENT_WIDTH - 2.0 * super::settings_color_picker_model::BAR_INSET;

        let square_focused = self.square_focus.is_focused(window);
        let hue_focused = self.hue_focus.is_focused(window);
        let alpha_focused = self.alpha_focus.is_focused(window);
        let hex_focused = self.hex.read(cx).focus_handle().is_focused(window);

        // Bounds are measured in prepaint; the drag listeners are window level.
        let weak = cx.weak_entity();
        let (square_bounds, hue_bounds, alpha_bounds) = (weak.clone(), weak.clone(), weak.clone());
        let (move_weak, up_weak) = (weak.clone(), weak);
        let hue = model.hue();
        let alpha_base = opaque;

        let marker_x = model.saturation() * CONTENT_WIDTH - MARKER / 2.0;
        let marker_y = (1.0 - model.value()) * SQUARE_HEIGHT - MARKER / 2.0;
        let square = rsx! {
            <div id={"settings-color-square"} relative w={px(CONTENT_WIDTH)}
                h={px(SQUARE_HEIGHT)} flex_none />
        }
            .cursor_crosshair()
            .track_focus(&self.square_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, window, cx| {
                    this.begin_drag(Drag::Square, event, window, cx)
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ =
                            square_bounds.update(cx, |this, _| this.square_bounds = Some(bounds));
                    },
                    move |bounds, _, window, _| {
                        paint_square(window, bounds, hue);
                        let weak = move_weak.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ =
                                    weak.update(cx, |this, cx| this.mouse_move(event, window, cx));
                            }
                        });
                        let weak = up_weak.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = weak.update(cx, |this, cx| this.mouse_up(event, cx));
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                rsx! {
                    <div absolute
                        left={px(
                        marker_x.clamp(-MARKER / 2.0, CONTENT_WIDTH - MARKER / 2.0)
                    )}
                        top={px(
                        marker_y.clamp(-MARKER / 2.0, SQUARE_HEIGHT - MARKER / 2.0)
                    )}
                        size={px(MARKER)} rounded_full border_2 border_color={white()}
                        bg={opaque} shadow_sm />
                },
            )
            .when(square_focused, |region| {
                region.child(focus_ring(4.0, palette.focus_border))
            });

        let hue_bar = rsx! {
            <div id={"settings-color-hue"} relative w={px(CONTENT_WIDTH)} h={px(BAR_HEIGHT)}
                flex_none cursor_pointer />
        }
            .track_focus(&self.hue_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, window, cx| {
                    this.begin_drag(Drag::Hue, event, window, cx)
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = hue_bounds.update(cx, |this, _| this.hue_bounds = Some(bounds));
                    },
                    |bounds, _, window, _| paint_hue_bar(window, bounds),
                )
                .absolute()
                .size_full(),
            )
            .child(thumb(
                super::settings_color_picker_model::BAR_INSET - THUMB / 2.0
                    + travel * (hue / HUE_MAX).clamp(0.0, 1.0),
                hue_color,
            ))
            .when(hue_focused, |region| {
                region.child(focus_ring(BAR_HEIGHT / 2.0, palette.focus_border))
            });

        let alpha_bar = rsx! {
            <div id={"settings-color-alpha"} relative w={px(CONTENT_WIDTH)} h={px(BAR_HEIGHT)}
                flex_none cursor_pointer />
        }
            .track_focus(&self.alpha_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, window, cx| {
                    this.begin_drag(Drag::Alpha, event, window, cx)
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = alpha_bounds.update(cx, |this, _| this.alpha_bounds = Some(bounds));
                    },
                    move |bounds, _, window, _| paint_alpha_bar(window, bounds, alpha_base),
                )
                .absolute()
                .size_full(),
            )
            .child(thumb(
                super::settings_color_picker_model::BAR_INSET - THUMB / 2.0
                    + travel * model.alpha().clamp(0.0, 1.0),
                opaque,
            ))
            .when(alpha_focused, |region| {
                region.child(focus_ring(BAR_HEIGHT / 2.0, palette.focus_border))
            });

        let old = argb_hsla(self.state.opened());
        let compare = rsx! {
            <div flex flex_none>
                {swatch(
                    COMPARE_SIZE[0],
                    COMPARE_SIZE[1],
                    4.0,
                    4.0,
                    old,
                    palette.control_border,
                )}
                {swatch(
                    COMPARE_SIZE[0],
                    COMPARE_SIZE[1],
                    4.0,
                    4.0,
                    draft,
                    palette.control_border,
                )}
            </div>
        };
        let hex_box = rsx! {
            <div flex flex_1 min_w_0 h={px(HEX_HEIGHT)} rounded={px(8.0)} border_1
                border_color={if hex_focused {
                    palette.focus_border
                } else {
                    palette.control_border
                }}
                bg={palette.control} overflow_hidden>
                {self.hex.clone()}
            </div>
        };
        let busy = self.eyedropper.is_busy();
        let now = Instant::now();
        let hover_progress = self.eyedropper_motion.value(
            (),
            if self.eyedropper_hovered && !busy {
                1.0
            } else {
                0.0
            },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if self.eyedropper_motion.any_active(now) {
            window.request_animation_frame();
        }
        // The native overlay is Windows-only; elsewhere the button is not shown.
        let eyedropper = cfg!(windows).then(|| {
            rsx! {
                <div id={"settings-color-eyedropper"} flex flex_none items_center justify_center
                    size={px(HEX_HEIGHT)} rounded={px(8.0)} border_1 />
            }
                .border_color(if busy {
                    palette.focus_border
                } else {
                    mix_hsla(
                        palette.control_border,
                        palette.control_hover_border,
                        hover_progress,
                    )
                })
                .bg(mix_hsla(palette.control, palette.control_hover, hover_progress))
                .when(!busy, |button| button.cursor_pointer())
                .tooltip(settings_help(
                    tr("屏幕取色", "Pick from screen").to_string(),
                    self.theme.clone(),
                ))
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if this.eyedropper_hovered != *hovered {
                        this.eyedropper_hovered = *hovered;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.start_eyedropper(window, cx);
                }))
                .child(
                    svg()
                        .path(EYEDROPPER_ASSET)
                        .size(px(EYEDROPPER_ICON))
                        .text_color(if busy { self.accent } else { self.text }),
                )
        });
        let value_row = rsx! {
            <div flex items_center gap={px(SECTION_GAP)}>
                {compare}
                {hex_box}
            </div>
        }
            .children(eyedropper);
        let eyedropper_error = self.eyedropper.error().map(|message| {
            rsx! {
                <div w_full text_size={px(11.0)} text_color={self.error}>
                    {SharedString::from(message.to_string())}
                </div>
            }
        });

        let current = self.state.draft();
        // Swatch hover wash: a 0..1 progress per swatch id, driven by `on_hover`.
        let preset_id = |index: usize| SharedString::from(format!("settings-color-preset-{index}"));
        let recent_id = |index: usize| SharedString::from(format!("settings-color-recent-{index}"));
        let ids = (0..PRESET_COLORS.len())
            .map(preset_id)
            .chain((0..self.recents.len().min(RECENT_VISIBLE)).map(recent_id))
            .collect::<Vec<_>>();
        let swatch_now = Instant::now();
        let swatch_progress = ids
            .into_iter()
            .map(|id| {
                let target = if self.swatch_hovered.as_ref() == Some(&id) {
                    1.0
                } else {
                    0.0
                };
                let progress =
                    self.swatch_motion
                        .value(id.clone(), target, motion::HOVER, motion::EASE, swatch_now);
                (id, progress)
            })
            .collect::<std::collections::HashMap<_, _>>();
        if self.swatch_motion.any_active(swatch_now) {
            window.request_animation_frame();
        }
        let swatch_button = |id: SharedString, color: ArgbColor, cx: &mut Context<Self>| {
            let progress = swatch_progress.get(&id).copied().unwrap_or(0.0);
            let rest = if color == current {
                palette.focus_border
            } else {
                gpui::transparent_black()
            };
            let hover_id = id.clone();
            rsx! {
                <div id={id} flex_none cursor_pointer rounded={px(4.0)} border_1
                    border_color={mix_hsla(rest, palette.control_hover_border, progress)} />
            }
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    let next = if *hovered {
                        Some(hover_id.clone())
                    } else if this.swatch_hovered.as_ref() == Some(&hover_id) {
                        None
                    } else {
                        return;
                    };
                    if this.swatch_hovered != next {
                        this.swatch_hovered = next;
                        cx.notify();
                    }
                }))
                .child(swatch(
                    SWATCH - 2.0,
                    SWATCH - 2.0,
                    3.0,
                    4.5,
                    argb_hsla(color),
                    palette.control_border,
                ))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.apply_swatch(color, window, cx)),
                )
        };
        let presets = div().flex().justify_between().w_full().children(
            PRESET_COLORS
                .iter()
                .enumerate()
                .filter_map(|(index, preset)| {
                    Some(swatch_button(
                        SharedString::from(format!("settings-color-preset-{index}")),
                        ArgbColor::parse(preset)?,
                        cx,
                    ))
                }),
        );
        let recents = (!self.recents.is_empty()).then(|| {
            div().flex().gap(px(4.0)).w_full().children(
                self.recents
                    .iter()
                    .take(RECENT_VISIBLE)
                    .enumerate()
                    .map(|(index, color)| {
                        swatch_button(
                            SharedString::from(format!("settings-color-recent-{index}")),
                            *color,
                            cx,
                        )
                    }),
            )
        });

        let enter_id = ElementId::from(SharedString::from(format!(
            "settings-color-popup-enter-{}",
            cx.entity_id().as_u64()
        )));
        let card = rsx! {
            <div id={"settings-color-popup"} relative
                w={px(CONTENT_WIDTH + 2.0 * (PADDING + 1.0))} flex flex_col gap={px(SECTION_GAP)}
                p={px(PADDING)} border_1 border_color={palette.control_border} rounded={px(8.0)}
                shadow_md bg={palette.card} text_color={self.text} occlude />
        }
            .on_key_down(cx.listener(Self::key_down))
            .on_any_mouse_down(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                // The trigger toggles the popup itself; its capture-phase click
                // must not close it before that same click is handled.
                if !this.anchor.contains(&event.position) {
                    this.confirm(ColorDismissal::Outside, window, cx);
                }
            }))
            .child(square)
            .child(hue_bar)
            .child(alpha_bar)
            .child(value_row)
            .child(presets)
            .children(recents)
            .children(eyedropper_error)
            // `super-menu-column-in`: opacity 0 -> 1 and 4px upward over POPUP_ENTER.
            .with_animation(
                enter_id,
                motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                |card, progress| {
                    card.opacity(progress)
                        .top(px(ENTER_OFFSET * (1.0 - progress)))
                },
            );
        // The parent renders this entity directly; a second `deferred` wrapper
        // would panic in GPUI's deferred draw pass.
        deferred(
            anchored()
                .anchor(if layout.above {
                    Corner::BottomLeft
                } else {
                    Corner::TopLeft
                })
                .position(point(px(layout.left), px(layout.anchor_y)))
                .snap_to_window_with_margin(px(8.0))
                .child(card),
        )
        .with_priority(2)
        .into_any_element()
    }
}
