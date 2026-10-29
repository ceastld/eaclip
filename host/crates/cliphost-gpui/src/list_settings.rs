//! Main-window footer popover. Forms and appearance writes keep their existing owners.
use gpui_rsx::rsx;
use super::*;
use gpui::{canvas, point, AnimationExt, AnyElement, PathBuilder};

#[derive(Clone, Copy, Debug)]
struct Geometry {
    left: f32,
    bottom: f32,
    width: f32,
    height: f32,
    arrow_left: f32,
}

#[derive(Clone, Copy)]
enum Destination {
    Close,
    More,
    Categories,
}

impl Geometry {
    fn new(width: f32, height: f32, tokens: &DesignTokens) -> Self {
        let p = tokens.list_settings_popover;
        let l = &tokens.layout;
        // The trigger is immediately before the last (settings) footer button.
        // Its center therefore remains stable when other footer actions change.
        let anchor_x = width - l.content_padding_x - l.icon_button_size * 1.5 - l.chrome_button_gap;
        let bottom =
            l.content_padding_y + (l.footer_height + l.icon_button_size) / 2.0 + p.anchor_gap;
        let panel_width = p.width.min((width - p.inset * 2.0).max(0.0));
        let left = width - p.inset - panel_width;
        Self {
            left,
            bottom,
            width: panel_width,
            height: p.height.min((height - bottom - p.inset).max(0.0)),
            arrow_left: (anchor_x - left - 5.0).clamp(12.0, (panel_width - 22.0).max(12.0)),
        }
    }
}

fn tail(surface: Hsla, border: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (inset, depth, fill) in [(0.0, 5.0, border), (1.0, 4.0, surface)] {
                let mut path = PathBuilder::fill();
                path.move_to(bounds.origin + point(px(inset), px(-1.0)));
                path.line_to(bounds.origin + point(px(5.0), px(depth)));
                path.line_to(bounds.origin + point(px(10.0 - inset), px(-1.0)));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, fill);
                }
            }
        },
    )
    .w(px(10.0))
    .h(px(6.0))
}

impl ClipboardPreview {
    pub(super) fn create_list_settings(
        &mut self,
        host: Arc<HostState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tokens, dark) = (self.clipboard_tokens(), self.dark);
        let controls =
            cx.new(|_| AppearanceControls::new(&self.configuration, tokens.clone(), dark));
        let subscription = cx.subscribe(&controls, |this, _, event, cx| {
            let AppearanceEvent::Changed(patch) = event;
            this.save_appearance(patch.clone(), cx);
        });
        self.appearance_controls = Some((controls, subscription));
        self.list_settings_appearance = true;
        self.list_settings_generation = self.list_settings_generation.wrapping_add(1);
        self.list_settings = Some(cx.new(|cx| {
            let mut form = SettingsForm::new(
                host,
                tokens,
                dark,
                Some(LIST_SETTINGS_KEYS.to_vec()),
                window,
                cx,
            );
            form.load("clip", window, cx);
            form
        }));
        self.refresh_clipboard_colors(cx);
        if !self.search.read(cx).is_composing() {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    pub(super) fn close_list_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_list_settings(Destination::Close, window, cx);
    }

    fn leave_list_settings(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.list_settings.clone() else {
            return;
        };
        if !form.read(cx).list_settings_can_close(cx) {
            return;
        }
        let id = form.entity_id();
        let Ok(ticket) = SettingsForm::prepare_leave(&[form], window, cx) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = ticket.wait().await;
            let _ = this.update_in(cx, |this, window, cx| {
                if result.finish(cx).is_ok()
                    && this
                        .list_settings
                        .as_ref()
                        .is_some_and(|form| form.entity_id() == id)
                {
                    this.list_settings = None;
                    this.appearance_controls = None;
                    match destination {
                        Destination::Close => window.focus(&this.focus),
                        Destination::More => this.open_settings_route("/main/settings/clip", cx),
                        Destination::Categories => this.open_tag_editor(window, cx),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn list_settings_panel(
        &self,
        form: Entity<SettingsForm>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let p = tokens.list_settings_popover;
        let size = window.viewport_size();
        let geometry = Geometry::new(size.width.into(), size.height.into(), &tokens);
        let hover = color(&t.neutral_active);
        let mut tabs = rsx! {
            <div h={px(p.tab_height - 1.0)} flex_1 min_w_0 flex items_center gap={px(20.0)} />
        };
        for (appearance, label) in [
            (true, tr("列表外观", "Appearance")),
            (false, tr("交互行为", "Behavior")),
        ] {
            let active = self.list_settings_appearance == appearance;
            let tab = div()
                .id(if appearance {
                    "list-settings-appearance"
                } else {
                    "list-settings-behavior"
                })
                .relative()
                .h_full()
                .flex_none()
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(p.font_size))
                .text_color(color(if active { &t.text } else { &t.text_muted }))
                .when(active, |tab| tab.font_weight(FontWeight::SEMIBOLD))
                .child(label)
                .when(active, |tab| {
                    tab.child(
                        rsx! {
                            <div absolute left_0 right_0 bottom={px(-1.0)} h={px(2.0)}
                                bg={color(&t.accent)} />
                        },
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    let changed = this.list_settings.clone().is_some_and(|form| {
                        form.update(cx, |form, cx| form.set_list_settings_page(appearance, cx))
                    });
                    if changed {
                        this.list_settings_appearance = appearance;
                        window.focus(&this.focus);
                        cx.notify();
                    }
                }));
            tabs = tabs.child(tab);
        }
        let close = rsx! {
            <div id={"list-settings-close"} size={px(24.0)} flex_none flex items_center
                justify_center rounded={px(5.0)} cursor_pointer text_color={color(&t.text_muted)} />
        }
            .hover(move |s| s.bg(hover))
            .tooltip(preference_tooltip(tr("关闭", "Close").into(), t.clone()))
            .child(self.icon("close", 16.0, color(&t.text_muted)))
            .on_click(cx.listener(|this, _, window, cx| this.close_list_settings(window, cx)));
        let card = rsx! {
            <div id={"list-settings-card"} relative w_full h_full min_h_0 flex flex_col
                bg={self.popover_surface()} border_1 border_color={color(&t.border_secondary)}
                rounded={px(p.radius)} shadow_lg />
        }
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                rsx! {
                    <div flex_none h={px(p.tab_height)} pl={px(p.body_padding_x)} pr={px(10.0)}
                        flex items_center border_b_1 border_color={color(&t.border)}>
                        {tabs}
                        {close}
                    </div>
                },
            )
            .child(
                rsx! {
                    <div id={"list-settings-scroll"} flex_1 min_h_0 flex flex_col />
                }
                    .overflow_y_scroll()
                    .smooth_scroll(("list-settings-scroll", form.entity_id()), cx)
                    .px(px(p.body_padding_x))
                    .pt(px(p.body_padding_top))
                    .pb(px(p.body_padding_bottom))
                    .when(self.list_settings_appearance, |body| {
                        body.children(
                            self.appearance_controls
                                .as_ref()
                                .map(|(controls, _)| controls.clone()),
                        )
                        .child(
                            rsx! {
                                <div flex_none h={px(1.0)} my={px(8.0)} bg={color(&t.border)} />
                            },
                        )
                    })
                    .child(form)
                    .when(self.list_settings_appearance, |body| {
                        body.child(
                            rsx! {
                                <div id={"list-settings-categories"} min_h={px(29.0)} mt={px(4.0)}
                                    flex_none flex items_center justify_between cursor_pointer
                                    text_size={px(p.font_size)} text_color={color(&t.text_muted)}>
                                    {tr("自定义分类标签", "Customize categories")}
                                    {self.icon("chevron-right", 14.0, color(&t.text_muted))}
                                </div>
                            }
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.leave_list_settings(Destination::Categories, window, cx)
                                })),
                        )
                    }),
            )
            .child(
                rsx! {
                    <div h={px(p.footer_height)} flex_none flex items_center justify_between
                        pl={px(p.body_padding_x)} pr={px(12.0)} border_t_1
                        border_color={color(&t.border)} />
                }
                    .child(
                        rsx! {
                            <div id={"list-settings-more"} flex items_center gap={px(2.0)}
                                cursor_pointer text_size={px(p.font_size)}
                                text_color={color(&t.text_muted)}>
                                {tr("更多设置", "More settings")}
                                {self.icon("chevron-right", 13.0, color(&t.text_muted))}
                            </div>
                        }
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.leave_list_settings(Destination::More, window, cx)
                            })),
                    )
                    .child(
                        rsx! {
                            <div id={"list-settings-done"} h={px(27.0)} min_w={px(54.0)}
                                px={px(12.0)} flex items_center justify_center rounded={px(5.0)}
                                border_1 border_color={color(&t.border_secondary)}
                                bg={color(&t.secondary)} text_size={px(p.font_size)}
                                text_color={color(&t.text)} cursor_pointer />
                        }
                            .hover(move |s| s.bg(hover))
                            .child(tr("完成", "Done"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_list_settings(window, cx)
                            })),
                    ),
            )
            .child(
                rsx! {
                    <div absolute left={px(geometry.arrow_left)} bottom={px(-5.0)}>
                        {tail(self.popover_surface(), color(&t.border_secondary))}
                    </div>
                },
            );
        // Keep this overlay in the main draw pass: SettingsForm choices own a
        // deferred anchored popup and GPUI forbids nesting deferred passes.
        div()
            .id("list-settings-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_list_settings(window, cx)),
            )
            .child(
                rsx! {
                    <div id={"list-settings"} absolute left={px(geometry.left)}
                        bottom={px(geometry.bottom)} w={px(geometry.width)}
                        h={px(geometry.height)} min_h_0 />
                }
                    .child(card.with_animation(
                        ("list-settings-enter", self.list_settings_generation),
                        crate::motion::enter(
                            crate::motion::LIST_SETTINGS_ENTER,
                            crate::motion::EASE_OUT,
                        ),
                        |card, progress| card.opacity(progress).top(px(5.0 * (1.0 - progress))),
                    )),
            )
            .into_any_element()
    }

    #[cfg(debug_assertions)]
    pub fn open_list_settings_visual(
        &mut self,
        host: Arc<HostState>,
        behavior: bool,
        custom: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        assert!(
            matches!(self.model, UiModel::Fixture(_)) && cliphost_core::is_headless(),
            "isolated debug fixture only"
        );
        self.configuration = host.config_get();
        if custom {
            self.configuration["clipListAppearanceSlot"] = json!("custom");
        }
        self.create_list_settings(host, window, cx);
        if behavior {
            if let Some(form) = &self.list_settings {
                form.update(cx, |form, cx| {
                    form.set_list_settings_page(false, cx);
                });
            }
            self.list_settings_appearance = false;
        }
    }

    #[cfg(debug_assertions)]
    pub fn open_list_settings_choice_visual(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        assert!(
            matches!(self.model, UiModel::Fixture(_)) && cliphost_core::is_headless(),
            "isolated debug fixture only"
        );
        self.list_settings.as_ref().is_some_and(|form| {
            form.update(cx, |form, cx| {
                form.open_list_settings_choice_visual(key, window, cx)
            })
        })
    }

    #[cfg(debug_assertions)]
    pub(super) fn list_settings_diagnostics(&self, window: &Window, cx: &gpui::App) -> Value {
        let size = window.viewport_size();
        let geometry = Geometry::new(
            size.width.into(),
            size.height.into(),
            &self.clipboard_tokens(),
        );
        json!({"open":self.list_settings.is_some(), "page":if self.list_settings_appearance {"appearance"} else {"behavior"},
            "titleVisible":false,"left":geometry.left,"bottom":geometry.bottom,"width":geometry.width,"height":geometry.height,"arrowLeft":geometry.arrow_left,
            "form":self.list_settings.as_ref().map(|form| form.read(cx).list_settings_diagnostics(cx))})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_preserves_footer_gap_and_stays_inside_small_windows() {
        let tokens = DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        for (width, height) in [
            (420.0, 640.0),
            (320.0, 420.0),
            (300.0, 250.0),
            (560.0, 720.0),
        ] {
            let geometry = Geometry::new(width, height, &tokens);
            let p = tokens.list_settings_popover;
            assert!((width - geometry.left - geometry.width - p.inset).abs() < 0.01);
            assert!(
                geometry.left >= p.inset && height - geometry.bottom - geometry.height >= p.inset
            );
            assert!(geometry.arrow_left >= 12.0 && geometry.arrow_left + 10.0 <= geometry.width);
            assert_eq!(
                geometry.height,
                p.height.min(height - geometry.bottom - p.inset)
            );
            assert_eq!(
                geometry.bottom,
                tokens.layout.content_padding_y
                    + (tokens.layout.footer_height + tokens.layout.icon_button_size) / 2.0
                    + p.anchor_gap
            );
        }
    }
}
