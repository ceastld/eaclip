//! A retained settings range control. The form owns the accepted value and saves;
//! this entity only projects it and emits lifetime-scoped interaction intents.

use gpui_rsx::rsx;
use gpui::{
    canvas, div, prelude::*, px, relative, Bounds, Context, DispatchPhase, EventEmitter,
    FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, ScrollWheelEvent, Subscription, Window,
};

use super::SettingsPalette;
use crate::{preview::ThemeTokens, view::color};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SliderDomain {
    min: f64,
    max: f64,
    step: f64,
}

impl SliderDomain {
    pub(super) fn new(min: f64, max: f64, step: f64) -> Option<Self> {
        let span = max - min;
        (min.is_finite()
            && max.is_finite()
            && step.is_finite()
            && step > 0.0
            && span.is_finite()
            && span >= 0.0
            && (span / step).is_finite())
        .then_some(Self { min, max, step })
    }

    fn clamp(self, value: f64) -> f64 {
        if value.is_finite() {
            value.clamp(self.min, self.max)
        } else {
            self.min
        }
    }

    fn snap(self, value: f64) -> f64 {
        let units = ((self.clamp(value) - self.min) / self.step).round();
        let last = ((self.max - self.min) / self.step + 1.0e-9).floor();
        self.clamp(self.min + units.min(last).max(0.0) * self.step)
    }

    fn at_ratio(self, ratio: f64) -> Option<f64> {
        ratio
            .is_finite()
            .then(|| self.snap(self.min + ratio.clamp(0.0, 1.0) * (self.max - self.min)))
    }

    fn ratio(self, value: f64) -> f32 {
        if self.max == self.min {
            0.0
        } else {
            ((self.clamp(value) - self.min) / (self.max - self.min)) as f32
        }
    }

    fn key_value(self, value: f64, key: &str) -> Option<f64> {
        let delta = match key {
            "left" | "down" => -self.step,
            "right" | "up" => self.step,
            "pageup" => self.step * 10.0,
            "pagedown" => -self.step * 10.0,
            "home" => return Some(self.min),
            "end" => return Some(self.snap(self.max)),
            _ => return None,
        };
        let value = self.clamp(value) + delta;
        value.is_finite().then(|| self.snap(value))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SettingsSliderIntent {
    pub(super) value: f64,
    pub(super) epoch: u64,
    pub(super) request_focus: bool,
}

#[derive(Debug)]
struct SliderInteraction {
    epoch: u64,
    enabled: bool,
    dragging: bool,
}

impl SliderInteraction {
    fn retire(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.dragging = false;
    }

    fn begin(&mut self) -> bool {
        if !self.enabled {
            return false;
        }
        self.retire();
        self.dragging = true;
        true
    }

    fn accepts(&self, intent: &SettingsSliderIntent) -> bool {
        self.enabled && self.epoch == intent.epoch && intent.value.is_finite()
    }

    fn pointer_move(&mut self, left_pressed: bool) -> bool {
        if !self.enabled || !self.dragging {
            return false;
        }
        if !left_pressed {
            self.retire();
            return false;
        }
        true
    }

    fn finish(&mut self) -> bool {
        std::mem::replace(&mut self.dragging, false)
    }
}

#[derive(Clone, Copy, PartialEq)]
struct SliderColors {
    rail: Hsla,
    accent: Hsla,
    text: Hsla,
    thumb: Hsla,
}

pub(super) struct SettingsSlider {
    domain: SliderDomain,
    value: f64,
    interaction: SliderInteraction,
    colors: SliderColors,
    focus: FocusHandle,
    bounds: Option<Bounds<Pixels>>,
    _blur: Subscription,
    _activation: Subscription,
}

impl EventEmitter<SettingsSliderIntent> for SettingsSlider {}

impl SettingsSlider {
    pub(super) fn new(
        domain: SliderDomain,
        value: f64,
        theme: &ThemeTokens,
        palette: SettingsPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle().tab_stop(true);
        let blur = cx.on_blur(&focus, window, |this, _, cx| this.retire(cx));
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.retire(cx);
            }
        });
        Self {
            domain,
            value: domain.clamp(value),
            interaction: SliderInteraction {
                epoch: 0,
                enabled: true,
                dragging: false,
            },
            colors: Self::colors(theme, palette),
            focus,
            bounds: None,
            _blur: blur,
            _activation: activation,
        }
    }

    fn colors(theme: &ThemeTokens, palette: SettingsPalette) -> SliderColors {
        SliderColors {
            rail: palette.control_border,
            accent: color(&theme.accent),
            text: color(&theme.text_secondary),
            thumb: color(&theme.surface),
        }
    }

    pub(super) fn set_colors(
        &mut self,
        theme: &ThemeTokens,
        palette: SettingsPalette,
        cx: &mut Context<Self>,
    ) {
        let colors = Self::colors(theme, palette);
        if colors != self.colors {
            self.colors = colors;
            cx.notify();
        }
    }

    pub(super) fn synchronize(&mut self, domain: SliderDomain, value: f64, cx: &mut Context<Self>) {
        let domain_changed = domain != self.domain;
        if domain_changed {
            self.interaction.retire();
            self.domain = domain;
        }
        let value = self.domain.clamp(value);
        if self.value != value || domain_changed {
            self.value = value;
            cx.notify();
        }
    }

    pub(super) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.interaction.enabled != enabled {
            self.interaction.retire();
            self.interaction.enabled = enabled;
            cx.notify();
        }
    }

    pub(super) fn retire(&mut self, cx: &mut Context<Self>) {
        self.interaction.retire();
        cx.notify();
    }

    pub(super) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    pub(super) fn is_interacting(&self) -> bool {
        self.interaction.dragging
    }
    pub(super) fn accepts(&self, intent: &SettingsSliderIntent) -> bool {
        self.interaction.accepts(intent)
    }

    fn publish(&mut self, value: f64, request_focus: bool, cx: &mut Context<Self>) {
        let value = self.domain.clamp(value);
        if value != self.value || request_focus {
            self.value = value;
            cx.emit(SettingsSliderIntent {
                value,
                epoch: self.interaction.epoch,
                request_focus,
            });
            cx.notify();
        }
    }

    fn value_at(&self, position: gpui::Point<Pixels>) -> Option<f64> {
        let bounds = self.bounds?;
        let width = f64::from(f32::from(bounds.size.width)) - 14.0;
        if !width.is_finite() || width <= 0.0 {
            return None;
        }
        let x = f64::from(f32::from(position.x - bounds.left())) - 7.0;
        self.domain.at_ratio(x / width)
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(value) = self.value_at(event.position) else {
            return;
        };
        if self.interaction.begin() {
            // The form validates composition and lifetime before granting focus.
            window.prevent_default();
            cx.stop_propagation();
            self.publish(value, true, cx);
        }
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.interaction.dragging && !self.focus.is_focused(window) {
            self.retire(cx);
            return;
        }
        if self
            .interaction
            .pointer_move(event.pressed_button == Some(MouseButton::Left))
        {
            if let Some(value) = self.value_at(event.position) {
                self.publish(value, false, cx);
            }
            cx.stop_propagation();
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.interaction.dragging && !self.focus.is_focused(window) {
            self.retire(cx);
            return;
        }
        if event.button == MouseButton::Left && self.interaction.finish() {
            if let Some(value) = self.value_at(event.position) {
                self.publish(value, false, cx);
            }
            // Normal release accepts its final sample; later moves need a new down.
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interaction.enabled
            || !self.focus.is_focused(window)
            || event.keystroke.modifiers.modified()
        {
            return;
        }
        if let Some(value) = self.domain.key_value(self.value, &event.keystroke.key) {
            window.prevent_default();
            cx.stop_propagation();
            self.publish(value, false, cx);
        }
    }

    fn wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interaction.enabled || !self.focus.is_focused(window) {
            return;
        }
        let delta = f64::from(f32::from(event.delta.pixel_delta(px(16.0)).y));
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        // GPUI reports upward wheel deltas as positive (DOM deltaY is reversed).
        let next = self.value + delta.signum() * self.domain.step;
        window.prevent_default();
        cx.stop_propagation();
        if next.is_finite() {
            self.publish(self.domain.clamp(next), false, cx);
        }
    }
}

impl Render for SettingsSlider {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = self.domain.ratio(self.value);
        let colors = self.colors;
        let focused = self.focus.is_focused(window);
        let enabled = self.interaction.enabled;
        let weak_bounds = cx.weak_entity();
        let weak_paint = cx.weak_entity();
        let value = if self.value.fract() == 0.0 {
            format!("{:.0}", self.value)
        } else {
            self.value.to_string()
        };
        rsx! {
            <div w_full min_w_0 flex items_center gap={px(10.0)} />
        }
            .child(
                rsx! {
                    <div id={"settings-slider-track"} relative flex_1 min_w={px(64.0)}
                        h={px(32.0)} rounded={px(8.0)} border_1 />
                }
                    .border_color(if focused {
                        colors.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .track_focus(&self.focus)
                    .cursor_pointer()
                    .when(!enabled, |track| track.opacity(0.55).cursor_default())
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_key_down(cx.listener(Self::key_down))
                    .on_scroll_wheel(cx.listener(Self::wheel))
                    .child(
                        rsx! {
                            <div absolute left={px(7.0)} right={px(7.0)} top={px(12.0)} h={px(6.0)}
                                rounded_full bg={colors.rail}>
                                <div h_full w={relative(progress)} rounded_full
                                    bg={colors.accent} />
                                <div absolute left={relative(progress)} top={px(-5.0)}
                                    ml={px(-8.0)} size={px(16.0)} rounded_full border_2
                                    border_color={colors.accent} bg={colors.thumb} />
                            </div>
                        },
                    )
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let _ =
                                    weak_bounds.update(cx, |this, _| this.bounds = Some(bounds));
                            },
                            move |_, _, window, _| {
                                let weak = weak_paint.clone();
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, window, cx| {
                                        if phase == DispatchPhase::Capture {
                                            let _ = weak.update(cx, |this, cx| {
                                                this.mouse_move(event, window, cx)
                                            });
                                        }
                                    },
                                );
                                let weak = weak_paint.clone();
                                window.on_mouse_event(
                                    move |event: &MouseUpEvent, phase, window, cx| {
                                        if phase == DispatchPhase::Capture {
                                            let _ = weak.update(cx, |this, cx| {
                                                this.mouse_up(event, window, cx)
                                            });
                                        }
                                    },
                                );
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .child(
                rsx! {
                    <div min_w={px(32.0)} flex_none text_right text_size={px(12.0)}
                        text_color={colors.text}>
                        {value}
                    </div>
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_domain_quantizes_from_min_and_keeps_reachable_endpoints() {
        let domain = SliderDomain::new(5.0, 100.0, 10.0).unwrap();
        assert_eq!(domain.at_ratio(-1.0), Some(5.0));
        assert_eq!(domain.at_ratio(0.5), Some(55.0));
        assert_eq!(domain.at_ratio(2.0), Some(95.0));
        assert_eq!(domain.key_value(55.0, "home"), Some(5.0));
        assert_eq!(domain.key_value(55.0, "end"), Some(95.0));
        assert_eq!(domain.key_value(55.0, "right"), Some(65.0));
        assert_eq!(domain.key_value(55.0, "pagedown"), Some(5.0));
    }

    #[test]
    fn invalid_domains_and_nonfinite_pointer_values_never_produce_intents() {
        for (min, max, step) in [
            (f64::NAN, 100.0, 1.0),
            (0.0, f64::INFINITY, 1.0),
            (0.0, 100.0, f64::NAN),
            (0.0, 100.0, 0.0),
            (10.0, 0.0, 1.0),
            (-f64::MAX, f64::MAX, 1.0),
            (0.0, 100.0, f64::MIN_POSITIVE),
        ] {
            assert!(SliderDomain::new(min, max, step).is_none());
        }
        let fixed = SliderDomain::new(3.0, 3.0, 1.0).unwrap();
        assert_eq!(fixed.at_ratio(0.7), Some(3.0));
        assert_eq!(fixed.ratio(3.0), 0.0);
        assert_eq!(fixed.at_ratio(f64::NAN), None);
    }

    #[test]
    fn retirement_rejects_queued_intents_and_requires_a_new_pointer_down() {
        let mut state = SliderInteraction {
            epoch: 0,
            enabled: true,
            dragging: false,
        };
        assert!(state.begin());
        let intent = SettingsSliderIntent {
            value: 70.0,
            epoch: state.epoch,
            request_focus: true,
        };
        assert!(state.accepts(&intent));
        assert!(state.pointer_move(true));
        state.retire(); // Blur, hidden projection, load, undo or leave.
        assert!(!state.accepts(&intent));
        assert!(!state.pointer_move(true));
        assert!(state.begin());
        assert!(!state.pointer_move(false)); // A lost button-up cannot revive a drag.
        assert!(!state.pointer_move(true));
        state.enabled = false;
        assert!(!state.begin());
    }

    #[test]
    fn release_keeps_its_final_intent_but_never_resumes_without_a_new_down() {
        let mut state = SliderInteraction {
            epoch: 0,
            enabled: true,
            dragging: false,
        };
        assert!(state.begin());
        let final_sample = SettingsSliderIntent {
            value: 100.0,
            epoch: state.epoch,
            request_focus: false,
        };
        assert!(state.finish());
        assert!(state.accepts(&final_sample));
        assert!(!state.finish());
        assert!(!state.pointer_move(true));
        state.retire(); // Window deactivation must also invalidate the final queued sample.
        assert!(!state.accepts(&final_sample));
    }
}
