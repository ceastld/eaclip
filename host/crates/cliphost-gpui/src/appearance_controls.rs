//! Footer list appearance controls, following `GpuiListSettingsPopover`.
//! All edits still use the parent-owned narrow configuration patch stream.

use gpui_rsx::rsx;
use super::*;
use crate::motion::{self, Tweens};
use std::{cell::RefCell, collections::HashSet, time::Instant};

pub enum AppearanceEvent {
    Changed(Value),
}

pub struct AppearanceControls {
    settings: AppearanceSettings,
    tokens: DesignTokens,
    dark: bool,
    custom_expanded: bool,
    hovered: HashSet<SharedString>,
    motion: RefCell<Tweens<(SharedString, u8)>>,
}

impl EventEmitter<AppearanceEvent> for AppearanceControls {}

impl AppearanceControls {
    pub fn new(config: &Value, tokens: DesignTokens, dark: bool) -> Self {
        Self {
            settings: AppearanceSettings::from_config(config),
            tokens,
            dark,
            custom_expanded: true,
            hovered: HashSet::new(),
            motion: RefCell::new(Tweens::default()),
        }
    }

    pub fn settings(&self) -> &AppearanceSettings {
        &self.settings
    }

    pub fn set_config(&mut self, config: &Value, cx: &mut Context<Self>) {
        self.settings = AppearanceSettings::from_config(config);
        cx.notify();
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        cx.notify();
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.set_theme(dark, cx);
    }

    fn edit(&mut self, edit: AppearanceEdit, cx: &mut Context<Self>) {
        if matches!(edit, AppearanceEdit::Slot(AppearanceSlot::Custom)) {
            self.custom_expanded = true;
        }
        let patch = self.settings.apply_edit(edit);
        cx.emit(AppearanceEvent::Changed(patch));
        cx.notify();
    }

    fn progress(&self, id: &SharedString, track: u8, target: bool) -> f32 {
        self.motion.borrow_mut().value(
            (id.clone(), track),
            if target { 1.0 } else { 0.0 },
            if track == 0 || track == 2 {
                motion::HOVER
            } else {
                motion::SURFACE
            },
            motion::EASE,
            Instant::now(),
        )
    }

    fn hover_listener(
        id: SharedString,
        cx: &Context<Self>,
    ) -> Box<dyn Fn(&bool, &mut Window, &mut App)> {
        Box::new(cx.listener(move |this, hovered: &bool, _, cx| {
            let changed = if *hovered {
                this.hovered.insert(id.clone())
            } else {
                this.hovered.remove(&id)
            };
            if changed {
                cx.notify();
            }
        }))
    }

    fn row(&self, label: &'static str, control: AnyElement) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div w_full min_w_0 flex flex_none items_center justify_between gap={px(12.0)}
                min_h={px(self.tokens.list_settings_popover.row_height)}>
                <div min_w_0 text_color={color(&t.text_secondary)}>
                    {label}
                </div>
                {control}
            </div>
        }
    }

    fn segment(&self) -> gpui::Div {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div flex flex_none gap={px(2.0)} p={px(2.0)} rounded={px(6.0)} border_1
                border_color={color(&t.border)} bg={color(&t.background)} />
        }
    }

    fn choice(
        &self,
        id: &'static str,
        label: &'static str,
        selected: bool,
        compact: bool,
        edit: AppearanceEdit,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let id: SharedString = id.into();
        let hover = self.progress(&id, 0, self.hovered.contains(&id));
        let selected = self.progress(&id, 1, selected);
        div()
            .id(id.clone())
            .flex_none()
            .min_w(px(if compact { 36.0 } else { 48.0 }))
            .h(px(25.0))
            .flex()
            .items_center()
            .justify_center()
            .px(px(if compact { 5.0 } else { 8.0 }))
            .rounded(px(4.0))
            .bg(motion::mix_hsla(
                color(&t.neutral_active).opacity(0.45 * hover),
                color(&t.neutral_active),
                selected,
            ))
            .text_color(motion::mix_hsla(
                color(&t.text_muted),
                color(&t.text),
                selected,
            ))
            .cursor_pointer()
            .on_hover(Self::hover_listener(id, cx))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.edit(edit, cx)))
            .into_any_element()
    }

    fn preset(
        &self,
        id: &'static str,
        label: &'static str,
        slot: AppearanceSlot,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let id: SharedString = id.into();
        let selected = self.settings.slot == slot;
        let hover = self.progress(&id, 0, self.hovered.contains(&id));
        let selection = self.progress(&id, 1, selected);
        let background = motion::mix_hsla(
            motion::mix_hsla(color(&t.surface), color(&t.hover), hover),
            mix(&t.accent, &t.surface, 0.08),
            selection,
        );
        let mut mini = rsx! {
            <div w={px(48.0)} h={px(27.0)} flex_none flex flex_col items_center justify_center
                gap={px(4.0)} text_color={color(&t.text_muted)} />
        };
        if slot == AppearanceSlot::Custom {
            if let Some(path) = self.tokens.icons.get("sliders") {
                mini = mini.child(
                    svg()
                        .path(path.clone())
                        .size(px(20.0))
                        .flex_none()
                        .text_color(color(&t.text_muted)),
                );
            }
        } else {
            for _ in 0..2 {
                mini = mini.child(
                    rsx! {
                        <div relative w={px(42.0)} h={px(10.0)} flex_none />
                    }
                        .when(slot == AppearanceSlot::Standard, |row| {
                            row.border_1().rounded(px(3.0))
                        })
                        .when(slot == AppearanceSlot::Simple, |row| row.border_b_1())
                        .border_color(color(&t.border_secondary))
                        .child(
                            rsx! {
                                <div absolute left={px(5.0)} right={px(12.0)} top={px(4.0)}
                                    h={px(1.0)} bg={color(&t.text_muted).opacity(0.6)} />
                            },
                        ),
                );
            }
        }
        rsx! {
            <div id={id.clone()} relative flex_1 min_w_0
                h={px(self.tokens.list_settings_popover.preset_height)} flex flex_col
                items_center justify_center gap={px(4.0)} px={px(8.0)} rounded={px(7.0)} border_1
                border_color={motion::mix_hsla(
                motion::mix_hsla(color(&t.border), color(&t.border_secondary), hover),
                color(&t.accent),
                selection,
            )}
                bg={background}
                text_color={motion::mix_hsla(
                color(&t.text_secondary),
                color(&t.text),
                selection,
            )}
                cursor_pointer />
        }
            .on_hover(Self::hover_listener(id, cx))
            .child(mini)
            .child(label)
            .when(selected, |card| {
                card.child(
                    rsx! {
                        <div absolute right={px(6.0)} top={px(3.0)} text_size={px(10.0)}
                            text_color={color(&t.accent)}>
                            {"✓"}
                        </div>
                    },
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.edit(AppearanceEdit::Slot(slot), cx)))
            .into_any_element()
    }

    fn copy_preset(
        &self,
        id: &'static str,
        label: &'static str,
        slot: AppearanceSlot,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let id: SharedString = id.into();
        let hover = self.progress(&id, 0, self.hovered.contains(&id));
        rsx! {
            <div id={id.clone()} flex_none rounded={px(3.0)} bg={color(&t.hover).opacity(hover)}
                text_color={color(&t.accent)} text_size={px(11.0)} cursor_pointer />
        }
            .on_hover(Self::hover_listener(id, cx))
            .child(label)
            .on_click(
                cx.listener(move |this, _, _, cx| this.edit(AppearanceEdit::CopyPreset(slot), cx)),
            )
            .into_any_element()
    }

    fn toggle(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        edit: AppearanceEdit,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let s = self.tokens.list_settings_popover;
        let id: SharedString = id.into();
        let on = self.progress(&id, 2, checked);
        let hover = self.progress(&id, 0, self.hovered.contains(&id));
        let knob = s.switch_height - 4.0;
        self.row(
            label,
            rsx! {
                <div flex_none w={px(s.switch_width)} h={px(s.switch_height)} p={px(2.0)}
                    rounded_full
                    bg={motion::mix_hsla(
                    color(&t.border_secondary),
                    color(&t.accent),
                    on,
                )}>
                    <div relative left={px((s.switch_width - s.switch_height) * on)}
                        size={px(knob)} rounded_full bg={gpui::white()} />
                </div>
            }
                .into_any_element(),
        )
        .id(id.clone())
        .rounded(px(4.0))
        .bg(color(&t.hover).opacity(hover * 0.45))
        .cursor_pointer()
        .on_hover(Self::hover_listener(id, cx))
        .on_click(cx.listener(move |this, _, _, cx| this.edit(edit, cx)))
        .into_any_element()
    }

    fn action_checkbox(&self, action: HoverAction, cx: &Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let checked = self.settings.custom.hover_actions.contains(&action);
        let id: SharedString = format!("appearance-toggle-{}", action.as_str()).into();
        let hover = self.progress(&id, 0, self.hovered.contains(&id));
        let on = self.progress(&id, 2, checked);
        let label = if action == HoverAction::Primary {
            tr("复制 / 粘贴", "Copy / paste")
        } else {
            action_label(hover_command(action, 1), false)
        };
        rsx! {
            <div id={id.clone()} flex flex_none items_center gap={px(4.0)} rounded={px(3.0)}
                bg={color(&t.hover).opacity(hover)} text_size={px(11.0)} cursor_pointer />
        }
            .on_hover(Self::hover_listener(id, cx))
            .child(
                rsx! {
                    <div size={px(12.0)} flex_none border_1 rounded={px(2.0)}
                        border_color={motion::mix_hsla(
                        color(&t.border_secondary),
                        color(&t.accent),
                        on,
                    )}
                        bg={motion::mix_hsla(color(&t.background), color(&t.accent), on)} flex
                        items_center justify_center text_color={gpui::white()}
                        text_size={px(10.0)} />
                }
                    .child(if checked { "✓" } else { "" }),
            )
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.edit(AppearanceEdit::HoverAction(action, !checked), cx)
            }))
            .into_any_element()
    }
}

impl Render for AppearanceControls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark).clone();
        let mut slots = rsx! {
            <div w_full flex flex_none gap={px(self.tokens.list_settings_popover.control_gap)}
                mb={px(8.0)} />
        };
        for (id, label, slot) in [
            (
                "appearance-standard",
                tr("标准", "Standard"),
                AppearanceSlot::Standard,
            ),
            (
                "appearance-simple",
                tr("简约", "Simple"),
                AppearanceSlot::Simple,
            ),
            (
                "appearance-custom",
                tr("我的", "Mine"),
                AppearanceSlot::Custom,
            ),
        ] {
            slots = slots.child(self.preset(id, label, slot, cx));
        }
        let density = self
            .segment()
            .child(self.choice(
                "appearance-compact",
                tr("紧凑", "Compact"),
                self.settings.density == Density::Compact,
                false,
                AppearanceEdit::Density(Density::Compact),
                cx,
            ))
            .child(self.choice(
                "appearance-relaxed",
                tr("宽松", "Relaxed"),
                self.settings.density == Density::Relaxed,
                false,
                AppearanceEdit::Density(Density::Relaxed),
                cx,
            ));
        let mut editor = rsx! {
            <div w_full min_w_0 flex_none flex flex_col
                text_size={px(self.tokens.list_settings_popover.font_size)}
                line_height={relative(1.5)} text_color={color(&t.text_secondary)}>
                {slots}
                {self.row(tr("列表密度", "List density"), density.into_any_element())}
            </div>
        };

        if self.settings.slot == AppearanceSlot::Custom {
            let mut custom = rsx! {
                <div w_full min_w_0 flex_none flex flex_col border_1
                    border_color={color(&t.border)} rounded={px(6.0)} py={px(8.0)} px={px(10.0)}
                    mt={px(6.0)} mb={px(10.0)} bg={color(&t.background)} />
            }
                .child(
                    rsx! {
                        <div id={"appearance-custom-disclosure"} flex flex_none items_center
                            gap={px(5.0)} cursor_pointer />
                    }
                        .child(if self.custom_expanded { "▾" } else { "▸" })
                        .child(tr("自定义外观", "Custom appearance"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.custom_expanded = !this.custom_expanded;
                            cx.notify();
                        })),
                );
            if self.custom_expanded {
                let mut shells = self.segment();
                for (id, label, chrome) in [
                    (
                        "appearance-rounded",
                        tr("圆角", "Rounded"),
                        ItemChrome::Rounded,
                    ),
                    (
                        "appearance-hairline",
                        tr("横线", "Hairline"),
                        ItemChrome::Hairline,
                    ),
                    ("appearance-flush", tr("无框", "Flush"), ItemChrome::Flush),
                ] {
                    shells = shells.child(self.choice(
                        id,
                        label,
                        self.settings.custom.chrome == chrome,
                        true,
                        AppearanceEdit::Chrome(chrome),
                        cx,
                    ));
                }
                let layout = self
                    .segment()
                    .child(self.choice(
                        "appearance-standard-row",
                        tr("标准", "Standard"),
                        self.settings.custom.layout == RowLayout::Standard,
                        true,
                        AppearanceEdit::Layout(RowLayout::Standard),
                        cx,
                    ))
                    .child(self.choice(
                        "appearance-minimal-row",
                        tr("简洁", "Minimal"),
                        self.settings.custom.layout == RowLayout::Minimal,
                        true,
                        AppearanceEdit::Layout(RowLayout::Minimal),
                        cx,
                    ));
                let mut actions = rsx! {
                    <div flex flex_none flex_wrap gap_x={px(12.0)} gap_y={px(8.0)} py={px(3.0)} />
                };
                for action in HOVER_ACTIONS {
                    actions = actions.child(self.action_checkbox(action, cx));
                }
                custom = custom
                    .child(
                        rsx! {
                            <div flex flex_none gap={px(12.0)} my={px(8.0)}>
                                {self.copy_preset(
                                "appearance-copy-standard",
                                tr("从标准复制", "Copy standard"),
                                AppearanceSlot::Standard,
                                cx,
                            )}
                                {self.copy_preset(
                                "appearance-copy-simple",
                                tr("从简约复制", "Copy simple"),
                                AppearanceSlot::Simple,
                                cx,
                            )}
                            </div>
                        },
                    )
                    .child(self.row(tr("条目外壳", "Item shell"), shells.into_any_element()))
                    .child(self.row(tr("行布局", "Row layout"), layout.into_any_element()))
                    .child(self.toggle(
                        "appearance-process",
                        tr("显示来源进程", "Show source process"),
                        self.settings.custom.show_process,
                        AppearanceEdit::ShowProcess(!self.settings.custom.show_process),
                        cx,
                    ))
                    .child(self.toggle(
                        "appearance-time",
                        tr("显示时间", "Show time"),
                        self.settings.custom.show_time,
                        AppearanceEdit::ShowTime(!self.settings.custom.show_time),
                        cx,
                    ))
                    .child(
                        rsx! {
                            <div flex_none text_size={px(11.0)} text_color={color(&t.text_muted)}
                                mb={px(7.0)}>
                                {tr("悬停按键", "Hover actions")}
                            </div>
                        },
                    )
                    .child(actions);
            }
            editor = editor.child(custom);
        }
        if self.motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        editor
    }
}
