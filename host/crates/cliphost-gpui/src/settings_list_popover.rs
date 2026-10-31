//! The accepted footer list-settings proposal. This is a projection of the
//! existing form: page changes never reload fields or replace input entities.

use gpui_rsx::rsx;
use super::*;

const APPEARANCE_KEYS: [&str; 3] = [
    "ClipListSeparatePinnedUi",
    "ClipListImageHoverPreviewEnabled",
    "ClipListImagePreviewMaxHeightPx",
];
const BEHAVIOR_KEYS: [&str; 5] = [
    "EnableNoFocusMode",
    "DismissBehavior",
    "ClipListExecuteMode",
    "AutoScrollToTop",
    "AutoScrollToTopIdleSeconds",
];

pub(super) fn is_list_settings_popover(only: Option<&[&str]>) -> bool {
    only.is_some_and(|keys| {
        keys.len() == APPEARANCE_KEYS.len() + BEHAVIOR_KEYS.len()
            && APPEARANCE_KEYS
                .iter()
                .chain(BEHAVIOR_KEYS.iter())
                .all(|key| keys.contains(key))
    })
}

pub(super) fn page_keys(appearance: bool) -> HashSet<String> {
    (if appearance {
        APPEARANCE_KEYS.as_slice()
    } else {
        BEHAVIOR_KEYS.as_slice()
    })
    .iter().copied()
    .map(str::to_string)
    .collect()
}

impl SettingsForm {
    /// Returns false while an input is composing or the form is leaving.
    /// Callers switch their tab only after this accepts the projection change.
    pub fn set_list_settings_page(&mut self, appearance: bool, cx: &mut Context<Self>) -> bool {
        if !self.list_settings_popover || self.leave_locked || self.composition_active(cx) {
            return false;
        }
        self.set_search_keys(Some(page_keys(appearance)), None, cx);
        true
    }

    /// Outer dismissal must let the focused input and its nested choice popup
    /// consume Escape first. The caller retains the existing save/close path.
    pub fn list_settings_can_close(&self, cx: &App) -> bool {
        !self.composition_active(cx)
            && !self.leave_locked
            && self.choice_popup.is_none()
            && self.color_popup.is_none()
    }

    /// Explicit fixture diagnostics expose projection and stable input identity,
    /// without serializing editable text or stored setting values.
    pub fn list_settings_diagnostics(&self, cx: &App) -> Value {
        let visible: Vec<_> = self
            .fields
            .iter()
            .filter_map(|field| {
                let key = field["key"].as_str()?;
                self.search_keys
                    .as_ref()
                    .is_none_or(|keys| keys.contains(key))
                    .then_some(key)
            })
            .collect();
        let inputs: Map<String, Value> = self
            .inputs
            .iter()
            .map(|(key, input)| (key.clone(), json!(input.entity_id().as_u64())))
            .collect();
        json!({
            "listPopover": self.list_settings_popover,
            "page": if self.search_keys.as_ref().is_some_and(|keys| keys.contains(APPEARANCE_KEYS[0])) {
                "appearance"
            } else { "behavior" },
            "visibleFieldKeys": visible,
            "inputEntityIds": inputs,
            "composing": self.composition_active(cx),
            "pendingWrites": self.pending_writes(),
            "canClose": self.list_settings_can_close(cx),
        })
    }

    pub(super) fn list_settings_caption(&self, text: &str, t: &ThemeTokens) -> gpui::Div {
        rsx! {
            <div min_w_0 flex_none mb={px(7.0)} text_size={px(11.0)}
                text_color={color(&t.text_muted)}>
                {text.to_string()}
            </div>
        }
    }

    pub(super) fn list_settings_divider(&self, t: &ThemeTokens) -> gpui::Div {
        rsx! {
            <div w_full h={px(1.0)} flex_none my={px(8.0)} bg={color(&t.border)} />
        }
    }

    pub(super) fn list_settings_switch(
        &self,
        field: &Value,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = self.tokens.list_settings_popover;
        let key = field["key"].as_str().unwrap_or_default().to_string();
        let on = field["value"].as_bool().unwrap_or(false);
        let id = SharedString::from(format!("switch-{key}"));
        let target = if on { 1.0 } else { 0.0 };
        let progress = self.tween(&id, Track::On, target, motion::HOVER);
        let thumb = self.tween(&id, Track::Thumb, target, motion::HOVER);
        let knob = (s.switch_height - 4.0).max(1.0);
        let travel = (s.switch_width - 4.0 - knob).max(0.0);
        rsx! {
            <div id={id.clone()} w={px(s.switch_width)} h={px(s.switch_height)} flex_none flex
                p={px(2.0)} rounded_full
                bg={mix_hsla(
                color(&t.border_secondary),
                color(&t.accent),
                progress,
            )}
                cursor_pointer />
        }
            .on_hover(Self::hover_listener(id, cx))
            .child(
                rsx! {
                    <div relative flex_none left={px(lerp(0.0, travel, thumb))} size={px(knob)}
                        rounded_full bg={gpui::white()} />
                },
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.save(&key, json!(!on), window, cx);
            }))
    }

    pub(super) fn list_settings_number(
        &self,
        field: &Value,
        t: &ThemeTokens,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let s = self.tokens.list_settings_popover;
        let palette = self.palette(t);
        let key = field["key"].as_str().unwrap_or_default().to_string();
        let input = self.inputs.get(&key);
        let focused = input.is_some_and(|input| input.read(cx).focus_handle().is_focused(window));
        let draft = input
            .map(|input| input.read(cx).text().to_string())
            .unwrap_or_else(|| text_value(field));
        let current = draft
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite());
        let stepper = |direction: f64, label: &'static str| {
            let key = key.clone();
            let at_limit = current.is_some_and(|current| {
                if direction < 0.0 {
                    field["min"].as_f64().is_some_and(|min| current <= min)
                } else {
                    field["max"].as_f64().is_some_and(|max| current >= max)
                }
            });
            let disabled = self.leave_locked || at_limit;
            let id = SharedString::from(format!("step-{key}-{label}"));
            let hover = if disabled {
                0.0
            } else {
                self.hover_progress(&id, motion::HOVER)
            };
            self.small_button(id, label, t, cx)
                .size(px(s.number_height - 2.0))
                .border_0()
                .rounded(px(5.0))
                .justify_center()
                .p_0()
                .bg(mix_hsla(
                    gpui::transparent_black(),
                    palette.control_hover,
                    hover,
                ))
                .text_size(px(14.0))
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .when(disabled, |button| button.opacity(0.3).cursor_default())
                .when(!disabled, |button| {
                    button.on_click(cx.listener(move |this, _, window, cx| {
                        this.step_number(&key, direction, window, cx);
                    }))
                })
        };
        rsx! {
            <div flex items_center h={px(s.number_height)} rounded={px(6.0)} border_1 />
        }
            .border_color(if focused {
                color(&t.accent)
            } else {
                color(&t.border)
            })
            .bg(palette.page)
            .overflow_hidden()
            .child(stepper(-1.0, "−"))
            .child(
                div()
                    .w(px(35.0))
                    .min_w_0()
                    .flex_none()
                    .when_some(input, |middle, input| middle.child(input.clone()))
                    .when(input.is_none(), |middle| {
                        middle
                            .text_size(px(s.font_size))
                            .text_color(color(&t.text))
                            .child(text_value(field))
                    }),
            )
            .child(
                rsx! {
                    <div ml={px(3.0)} mr={px(6.0)} text_size={px(10.0)}
                        text_color={color(&t.text_muted)}>
                        {"px"}
                    </div>
                },
            )
            .child(stepper(1.0, "+"))
    }

    pub(super) fn list_settings_row(
        &self,
        field: &Value,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = self.tokens.list_settings_popover;
        let key = field["key"].as_str().unwrap_or_default();
        let stacked = kind(field) == Kind::Choice;
        let label = match key {
            "ClipListSeparatePinnedUi" => tr("置顶项单独分栏", "Separate pinned tab"),
            "ClipListImagePreviewMaxHeightPx" => tr("图片预览高度", "Image preview height"),
            "ClipListExecuteMode" => tr("粘贴触发方式", "Paste trigger"),
            _ => field["label"].as_str().unwrap_or_default(),
        };
        let description = if key == "ClipListImagePreviewMaxHeightPx" {
            tr(
                "列表内图片的最大显示高度，范围 40–640 px。",
                "Maximum image height in the list, from 40–640 px.",
            )
            .to_string()
        } else {
            field["description"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let label_width = crate::view::label_width(
            label,
            s.font_size,
            &self.tokens.typography.font_family,
            window,
        )
        .ceil()
            + 1.0;
        let title = rsx! {
            <div min_w_0 flex items_center gap={px(5.0)} text_size={px(s.font_size)}
                line_height={relative(1.5)} text_color={color(&t.text_secondary)} />
        }
            .when(stacked, |title| title.w_full().flex_none())
            .when(!stacked, |title| title.flex_1())
            .child(
                rsx! {
                    <div min_w_0 max_w={px(label_width)}>
                        {label.to_string()}
                    </div>
                },
            )
            .when(!description.is_empty(), |title| {
                title.child(
                    rsx! {
                        <div id={SharedString::from(format!("list-field-help-{key}"))}
                            size={px(13.0)} flex_none flex items_center justify_center rounded_full
                            border_1 border_color={color(&t.border_secondary)} text_size={px(10.0)}
                            line_height={relative(1.0)} text_color={color(&t.text_muted)}
                            cursor_pointer>
                            {"?"}
                        </div>
                    }
                        .tooltip(settings_help(description, t.clone())),
                )
            });
        let control = self.control(field, t, stacked, window, cx);
        rsx! {
            <div id={SharedString::from(format!(
                "settings-row-{}-{key}",
                self.config_id
            ))}
                w_full min_w_0 min_h={px(s.row_height)} flex_none flex />
        }
            .gap(px(if stacked { s.control_gap } else { 12.0 }))
            .when(stacked, |row| row.flex_col().pt(px(5.0)).pb(px(8.0)))
            .when(!stacked, |row| row.items_center().justify_between())
            .child(title)
            .child(
                div()
                    .min_w_0()
                    .flex_none()
                    .when(stacked, |control| control.w_full())
                    .child(control),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_popover_projection_is_limited_to_its_complete_field_set() {
        let all: Vec<_> = APPEARANCE_KEYS.into_iter().chain(BEHAVIOR_KEYS).collect();
        assert!(is_list_settings_popover(Some(&all)));
        assert!(!is_list_settings_popover(Some(&APPEARANCE_KEYS)));
        assert!(!is_list_settings_popover(None));
        let mut duplicate = all.clone();
        duplicate[5] = duplicate[0];
        assert!(!is_list_settings_popover(Some(&duplicate)));
        let appearance = page_keys(true);
        let behavior = page_keys(false);
        assert!(appearance.is_disjoint(&behavior));
        assert_eq!(appearance.len() + behavior.len(), all.len());
    }
}
