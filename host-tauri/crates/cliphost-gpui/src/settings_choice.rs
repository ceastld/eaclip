//! Settings enum listbox. Presentation emits intents; the parent owns saves,
//! trigger focus, lifetime and the single active popup for its form.

use gpui_rsx::rsx;
use cliphost_core::i18n::tr;
use crate::smooth_wheel::SmoothWheelExt;
use gpui::{
    anchored, deferred, div, point, prelude::*, px, relative, AnimationExt, Bounds,
    Context, Corner,
    EventEmitter, FocusHandle, Hsla, KeyDownEvent, KeyUpEvent, Pixels, ScrollHandle, SharedString,
    Subscription, Window,
};

use std::time::Instant;

use super::SettingsPalette;
use crate::{
    motion::{self, mix_hsla, Tweens},
    preview::ThemeTokens,
    view::color,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SettingsChoiceOption {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) des: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsChoiceDismissal {
    Escape,
    Outside,
    Tab { reverse: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SettingsChoiceEvent {
    Chosen { key: String, value: String },
    Dismissed(SettingsChoiceDismissal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChoiceMove {
    Previous,
    Next,
    First,
    Last,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmKey {
    Enter,
    Space,
}

impl ConfirmKey {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "enter" => Some(Self::Enter),
            "space" | " " => Some(Self::Space),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct ChoiceState {
    options: Vec<SettingsChoiceOption>,
    selected: Option<usize>,
    active: Option<usize>,
    open: bool,
    confirmation: Option<(ConfirmKey, usize)>,
}

impl ChoiceState {
    fn new(value: &str, options: Vec<SettingsChoiceOption>) -> Self {
        let selected = options
            .iter()
            .position(|option| option.key.eq_ignore_ascii_case(value));
        let active = selected.or_else(|| (!options.is_empty()).then_some(0));
        Self {
            options,
            selected,
            active,
            open: true,
            confirmation: None,
        }
    }

    fn navigate(&mut self, movement: ChoiceMove) -> bool {
        if !self.open || self.options.is_empty() {
            return false;
        }
        let last = self.options.len() - 1;
        let current = self.active.unwrap_or(0).min(last);
        let next = match movement {
            ChoiceMove::Previous => current.saturating_sub(1),
            ChoiceMove::Next => current.saturating_add(1).min(last),
            ChoiceMove::First => 0,
            ChoiceMove::Last => last,
        };
        let changed = self.active != Some(next);
        self.active = Some(next);
        if changed {
            self.cancel_confirmation();
        }
        changed
    }

    fn choose(&mut self, index: usize) -> Option<String> {
        if !self.open {
            return None;
        }
        let value = self.options.get(index)?.key.clone();
        self.cancel_confirmation();
        self.open = false;
        Some(value)
    }

    fn arm_confirmation(&mut self, key: ConfirmKey, held: bool) -> bool {
        if !self.open || held || self.confirmation.is_some() {
            return false;
        }
        let Some(index) = self.active.filter(|index| *index < self.options.len()) else {
            return false;
        };
        self.confirmation = Some((key, index));
        true
    }

    fn release_confirmation(&mut self, key: ConfirmKey, focused_and_plain: bool) -> Option<String> {
        let (pressed, index) = self.confirmation?;
        if pressed != key {
            return None;
        }
        self.cancel_confirmation();
        if !focused_and_plain || self.active != Some(index) {
            return None;
        }
        self.choose(index)
    }

    fn cancel_confirmation(&mut self) {
        self.confirmation = None;
    }

    fn close(&mut self) -> bool {
        self.cancel_confirmation();
        std::mem::replace(&mut self.open, false)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChoiceLayout {
    left: f32,
    anchor_y: f32,
    width: f32,
    max_height: f32,
    above: bool,
}

fn choice_layout(anchor: [f32; 4], viewport: [f32; 2], estimate: f32) -> ChoiceLayout {
    let [x, y, width, height] = anchor;
    let margin = 8.0_f32.min((viewport[0] / 2.0).max(0.0));
    let width = width.max(168.0).min((viewport[0] - 2.0 * margin).max(1.0));
    let left = (x + anchor[2] - width).clamp(margin, (viewport[0] - margin - width).max(margin));
    let below = (viewport[1] - y - height - 4.0 - 8.0).max(0.0);
    let above = (y - 4.0 - 8.0).max(0.0);
    let place_above = below < estimate.min(160.0) && below < above;
    ChoiceLayout {
        left,
        anchor_y: if place_above {
            y - 4.0
        } else {
            y + height + 4.0
        },
        width,
        max_height: if place_above { above } else { below }.clamp(1.0, 240.0),
        above: place_above,
    }
}

pub(crate) struct SettingsChoicePopup {
    field_key: String,
    state: ChoiceState,
    palette: SettingsPalette,
    text: Hsla,
    anchor: Bounds<Pixels>,
    list_settings: bool,
    focus: FocusHandle,
    scroll: ScrollHandle,
    confirmation_blur: Option<Subscription>,
    /// Option hover wash (`motion::HOVER`); display only. Dropped with the popup.
    hovered: Option<usize>,
    motion: Tweens<usize>,
    #[cfg(debug_assertions)]
    rendered_bounds: std::sync::Arc<std::sync::Mutex<Option<Bounds<Pixels>>>>,
    #[cfg(debug_assertions)]
    rendered_layout: Option<ChoiceLayout>,
}

impl EventEmitter<SettingsChoiceEvent> for SettingsChoicePopup {}

impl SettingsChoicePopup {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        field_key: String,
        value: String,
        options: Vec<SettingsChoiceOption>,
        theme: &ThemeTokens,
        palette: SettingsPalette,
        anchor: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            field_key,
            state: ChoiceState::new(&value, options),
            palette,
            text: color(&theme.text),
            anchor,
            list_settings: false,
            focus: cx.focus_handle().tab_stop(false),
            scroll: ScrollHandle::new(),
            confirmation_blur: None,
            hovered: None,
            motion: Tweens::default(),
            #[cfg(debug_assertions)]
            rendered_bounds: Default::default(),
            #[cfg(debug_assertions)]
            rendered_layout: None,
        }
    }

    pub(crate) fn with_list_settings(mut self, list_settings: bool) -> Self {
        self.list_settings = list_settings;
        self
    }

    #[cfg(debug_assertions)]
    pub(super) fn geometry_diagnostics(&self) -> serde_json::Value {
        let bounds = |bounds: Bounds<Pixels>| serde_json::json!({
            "x":f32::from(bounds.left()), "y":f32::from(bounds.top()),
            "width":f32::from(bounds.size.width), "height":f32::from(bounds.size.height),
        });
        serde_json::json!({
            "fieldKey":self.field_key,
            "anchor":bounds(self.anchor),
            "menu":self.rendered_bounds.lock().unwrap().map(bounds),
            "layout":self.rendered_layout.map(|layout| serde_json::json!({
                "above":layout.above, "anchorY":layout.anchor_y, "left":layout.left,
                "width":layout.width, "maxHeight":layout.max_height,
            })),
        })
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        if let Some(index) = self.state.active {
            self.scroll.scroll_to_item(index);
        }
        cx.notify();
    }

    /// Supply the trigger's fresh window bounds after scrolling or relayout.
    pub(crate) fn set_anchor(&mut self, anchor: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.anchor != anchor {
            self.anchor = anchor;
            cx.notify();
        }
    }

    pub(crate) fn update_colors(
        &mut self,
        theme: &ThemeTokens,
        palette: SettingsPalette,
        cx: &mut Context<Self>,
    ) {
        self.palette = palette;
        self.text = color(&theme.text);
        cx.notify();
    }

    pub(super) fn dismiss(&mut self, reason: SettingsChoiceDismissal, cx: &mut Context<Self>) {
        if self.state.close() {
            self.hovered = None;
            self.motion.clear();
            cx.emit(SettingsChoiceEvent::Dismissed(reason));
            cx.notify();
        }
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(value) = self.state.choose(index) {
            self.hovered = None;
            self.motion.clear();
            cx.emit(SettingsChoiceEvent::Chosen {
                key: self.field_key.clone(),
                value,
            });
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.open {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "tab" {
            window.prevent_default();
            cx.stop_propagation();
            if !event.is_held {
                self.dismiss(
                    SettingsChoiceDismissal::Tab {
                        reverse: modifiers.shift,
                    },
                    cx,
                );
            }
            return;
        }
        if modifiers.shift {
            return;
        }
        let movement = match key {
            "up" => Some(ChoiceMove::Previous),
            "down" => Some(ChoiceMove::Next),
            "home" => Some(ChoiceMove::First),
            "end" => Some(ChoiceMove::Last),
            _ => None,
        };
        if let Some(movement) = movement {
            window.prevent_default();
            cx.stop_propagation();
            if self.state.navigate(movement) {
                if let Some(index) = self.state.active {
                    self.scroll.scroll_to_item(index);
                }
                cx.notify();
            }
        } else if matches!(key, "enter" | "space" | " " | "escape") {
            window.prevent_default();
            cx.stop_propagation();
            if key == "escape" {
                if !event.is_held {
                    self.dismiss(SettingsChoiceDismissal::Escape, cx);
                }
            } else if self.focus.is_focused(window) {
                self.state
                    .arm_confirmation(ConfirmKey::from_key(key).unwrap(), event.is_held);
            }
        }
    }

    fn key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = ConfirmKey::from_key(&event.keystroke.key) else {
            return;
        };
        // Consume the release before emitting Chosen: the parent may restore
        // trigger focus synchronously, but this release must never activate it.
        window.prevent_default();
        cx.stop_propagation();
        let ready =
            self.focus.is_focused(window) && event.keystroke.modifiers.number_of_modifiers() == 0;
        if let Some(value) = self.state.release_confirmation(key, ready) {
            cx.emit(SettingsChoiceEvent::Chosen {
                key: self.field_key.clone(),
                value,
            });
            cx.notify();
        }
    }
}

impl Render for SettingsChoicePopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.confirmation_blur.is_none() {
            self.confirmation_blur = Some(cx.on_blur(&self.focus, window, |this, _, _| {
                this.state.cancel_confirmation();
            }));
        }
        if !self.state.open {
            return div().into_any_element();
        }
        let viewport = window.viewport_size();
        let estimate = self.state.options.iter().fold(10.0_f32, |height, option| {
            height + if option.des.is_empty() { 36.0 } else { 54.0 }
        });
        let layout = choice_layout(
            [
                f32::from(self.anchor.left()),
                f32::from(self.anchor.top()),
                f32::from(self.anchor.size.width),
                f32::from(self.anchor.size.height),
            ],
            [f32::from(viewport.width), f32::from(viewport.height)],
            estimate.min(240.0),
        );
        let palette = self.palette;
        #[cfg(debug_assertions)]
        { self.rendered_layout = Some(layout); }
        let now = Instant::now();
        let hover_progress = (0..self.state.options.len())
            .map(|index| {
                let target = if self.hovered == Some(index) { 1.0 } else { 0.0 };
                self.motion
                    .value(index, target, motion::HOVER, motion::EASE, now)
            })
            .collect::<Vec<_>>();
        if self.motion.any_active(now) {
            window.request_animation_frame();
        }
        let rows = self
            .state
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let base = if self.state.selected == Some(index) {
                    palette.selected
                } else {
                    gpui::transparent_black()
                };
                rsx! {
                    <div id={SharedString::from(format!(
                        "settings-choice-option-{index}"
                    ))}
                        w_full min_h={px(32.0)} flex_none flex flex_col gap={px(2.0)}
                        px={px(8.0)} py={px(6.0)} rounded={px(6.0)} cursor_pointer
                        text_size={px(12.0)} line_height={relative(1.35)}
                        bg={mix_hsla(base, palette.control_hover, hover_progress[index])} />
                }
                    .when(self.list_settings, |row| {
                        row.min_w_0().min_h(px(36.0)).flex_row().items_start()
                            .gap(px(8.0)).px(px(10.0)).py(px(8.0))
                            .border_1().border_color(gpui::transparent_black())
                    })
                    .when(self.state.active == Some(index)
                        && (!self.list_settings || self.state.selected != Some(index)), |row| {
                        row.border_1().border_color(palette.focus_border)
                    })
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        let next = if *hovered {
                            Some(index)
                        } else if this.hovered == Some(index) {
                            None
                        } else {
                            return;
                        };
                        if this.hovered != next {
                            this.hovered = next;
                            cx.notify();
                        }
                    }))
                    .when(self.list_settings, |row| {
                        row.child(
                            rsx! {
                                <div flex_1 min_w_0 flex flex_col gap={px(2.0)}>
                                    <div w_full min_w_0 flex_none text_size={px(12.0)}
                                        line_height={relative(1.4)}>
                                        {option.name.clone()}
                                    </div>
                                </div>
                            }
                                .when(!option.des.is_empty(), |content| {
                                    content.child(rsx! {
                                        <div w_full min_w_0 flex_none text_size={px(11.0)}
                                            line_height={relative(1.45)} text_color={palette.description}>
                                            {option.des.clone()}
                                        </div>
                                    })
                                }),
                        )
                            .child(
                                rsx! {
                                    <div w={px(16.0)} h={px(16.0)} flex_none flex items_center
                                        justify_center line_height={relative(1.0)}>
                                        {if self.state.selected == Some(index) { "✓" } else { "" }}
                                    </div>
                                },
                            )
                    })
                    .when(!self.list_settings, |row| {
                        row.child(option.name.clone()).when(!option.des.is_empty(), |row| row.child(
                            rsx! {
                                <div text_size={px(11.0)} text_color={palette.description}>
                                    {option.des.clone()}
                                </div>
                            },
                        ))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)))
            })
            .collect::<Vec<_>>();
        let card = rsx! {
            <div id={"settings-choice-popup"} w={px(layout.width)} max_h={px(layout.max_height)}
                flex flex_col p={px(4.0)} border_1 border_color={palette.control_border}
                rounded={px(8.0)} shadow_md bg={palette.card} text_color={self.text} />
        }
            .when(self.list_settings, |card| card.gap(px(2.0)))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .smooth_wheel("choice-options", &self.scroll, cx)
            .track_focus(&self.focus)
            .occlude()
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_any_mouse_down(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_down_out(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                // The trigger toggles the popup itself. Its capture-phase
                // outside click must not close before the same click toggles.
                if !this.anchor.contains(&event.position) {
                    this.dismiss(SettingsChoiceDismissal::Outside, cx);
                }
            }))
            .children(rows)
            .when(self.state.options.is_empty(), |card| {
                card.child(div().p(px(8.0)).child(tr("无可选项", "No options")))
            });
        #[cfg(debug_assertions)]
        let card = {
            let rendered_bounds = self.rendered_bounds.clone();
            let menu_scroll = self.scroll.clone();
            card.relative().child(
                gpui::canvas(move |mut bounds, _, _| {
                    // This child inherits the options' own scrolling; report
                    // the outer menu rather than its translated content.
                    bounds.origin -= menu_scroll.offset();
                    *rendered_bounds.lock().unwrap() = Some(bounds);
                }, |_, _, _, _| {}).absolute().inset_0().size_full(),
            )
        };
        // One entrance per opened popup entity (no frontend equivalent; the
        // shared popup token: opacity and a 4px vertical settle).
        let card = if motion::reduced_motion() {
            card.into_any_element()
        } else {
            card.relative()
                .with_animation(
                    ("settings-choice-in", cx.entity_id().as_u64()),
                    motion::enter(motion::POPUP_ENTER, motion::EASE_OUT),
                    |card, t| card.opacity(t).top(px(4.0 * (1.0 - t))),
                )
                .into_any_element()
        };
        // Parent renders this entity directly. A second deferred wrapper is
        // forbidden by GPUI's deferred draw pass and would panic.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<SettingsChoiceOption> {
        ["a", "b", "c"]
            .into_iter()
            .map(|key| SettingsChoiceOption {
                key: key.into(),
                name: format!("Name {key}"),
                des: format!("Description {key}"),
            })
            .collect()
    }

    #[test]
    fn navigation_starts_at_selected_and_never_changes_the_committed_selection() {
        assert_eq!(ChoiceState::new("B", options()).selected, Some(1));
        let mut state = ChoiceState::new("b", options());
        assert_eq!(state.active, Some(1));
        assert!(state.navigate(ChoiceMove::Last));
        assert!(!state.navigate(ChoiceMove::Next));
        assert_eq!(state.active, Some(2));
        assert!(state.navigate(ChoiceMove::First));
        assert!(!state.navigate(ChoiceMove::Previous));
        assert_eq!(state.active, Some(0));
        assert_eq!(state.selected, Some(1));
        assert_eq!(state.choose(state.active.unwrap()).as_deref(), Some("a"));
        assert_eq!(state.choose(2), None);
        assert!(!state.navigate(ChoiceMove::Last));
    }

    #[test]
    fn empty_unknown_and_dismissed_choices_cannot_emit_an_invalid_or_late_value() {
        let mut empty = ChoiceState::new("missing", Vec::new());
        assert_eq!(empty.active, None);
        assert!(!empty.navigate(ChoiceMove::Next));
        assert_eq!(empty.choose(0), None);
        let mut unknown = ChoiceState::new("missing", options());
        assert_eq!(unknown.selected, None);
        assert_eq!(unknown.active, Some(0));
        assert_eq!(unknown.choose(99), None);
        assert!(unknown.close());
        assert!(!unknown.close());
        assert_eq!(unknown.choose(0), None);
    }

    #[test]
    fn confirmation_only_commits_on_one_matching_release_and_close_clears_the_press() {
        for key in [ConfirmKey::Enter, ConfirmKey::Space] {
            let mut state = ChoiceState::new("b", options());
            assert!(state.arm_confirmation(key, false));
            assert!(state.open, "KeyDown must not choose or close the popup");
            assert!(!state.arm_confirmation(key, true));
            let other = if key == ConfirmKey::Enter {
                ConfirmKey::Space
            } else {
                ConfirmKey::Enter
            };
            assert_eq!(state.release_confirmation(other, true), None);
            assert_eq!(state.release_confirmation(key, true).as_deref(), Some("b"));
            assert_eq!(state.release_confirmation(key, true), None);

            let mut cancelled = ChoiceState::new("b", options());
            assert!(cancelled.arm_confirmation(key, false));
            assert!(cancelled.close(), "Escape, Tab and Outside all use close");
            assert_eq!(cancelled.confirmation, None);
            assert_eq!(cancelled.release_confirmation(key, true), None);

            let mut unfocused = ChoiceState::new("b", options());
            assert!(unfocused.arm_confirmation(key, false));
            assert_eq!(unfocused.release_confirmation(key, false), None);
            assert_eq!(unfocused.release_confirmation(key, true), None);
            assert!(unfocused.open);
            assert!(unfocused.arm_confirmation(key, false));
            unfocused.cancel_confirmation(); // The popup's blur callback.
            assert_eq!(unfocused.release_confirmation(key, true), None);
        }
    }

    #[test]
    fn popup_uses_separate_above_anchor_and_clamps_size_to_the_viewport() {
        let below = choice_layout([420.0, 100.0, 180.0, 32.0], [640.0, 600.0], 240.0);
        assert!(!below.above);
        assert_eq!(below.anchor_y, 136.0);
        assert_eq!(below.max_height, 240.0);
        let above = choice_layout([420.0, 550.0, 180.0, 32.0], [640.0, 600.0], 240.0);
        assert!(above.above);
        assert_eq!(above.anchor_y, 546.0);
        assert_eq!(above.max_height, 240.0);
        let narrow = choice_layout([80.0, 8.0, 200.0, 32.0], [100.0, 120.0], 240.0);
        assert_eq!(narrow.width, 84.0);
        assert_eq!(narrow.left, 8.0);
        assert!(narrow.max_height <= 120.0);
    }
}
