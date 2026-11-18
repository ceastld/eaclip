//! One focused text editor for both the favorites page and the clipboard column.
use super::*;

impl ClipboardPreview {
    pub(in crate::view) fn wait_for_phrase_save_on_exit(&mut self) -> bool {
        if let Some(draft) = self.phrases.draft.as_mut().filter(|draft| draft.saving) {
            draft.exit_after_save = true;
            return true;
        }
        false
    }

    pub(in crate::view) fn open_phrase_draft(
        &mut self,
        target: String,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.phrases.draft.is_some() || self.history_dialog || self.exit_pending {
            return;
        }
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&t.surface),
            foreground: color(&t.text),
            border: color(&t.border_secondary),
            placeholder: color(&t.text_muted),
            selection: color(&t.selected),
            caret: color(&t.accent),
        };
        let title = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(32., 12., 8., cx);
            input.set_double_click_clears(false);
            input.set_placeholder(tr("默认使用正文首行", "Defaults to the first line"), cx);
            input
        });
        let body = cx.new(|cx| {
            let mut input = MultilineInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(96., 13., 8., cx);
            input.set_placeholder(
                tr("输入或粘贴要收藏的文字…", "Type or paste text to save…"),
                cx,
            );
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(&title, window, |this, _, event, window, cx| match event {
                SearchEvent::Submit => this.save_phrase_draft(window, cx),
                SearchEvent::Escape => this.close_phrase_draft(window, cx),
                _ => cx.notify(),
            }),
            cx.subscribe_in(&body, window, |this, _, event, window, cx| match event {
                MultilineEvent::Save => this.save_phrase_draft(window, cx),
                MultilineEvent::Escape => this.close_phrase_draft(window, cx),
                _ => cx.notify(),
            }),
        ];
        if let Some(host) = self.model.host() {
            if let Err(error) = NativeWindow::request_search_focus(window, &host) {
                self.error = Some(error);
                cx.notify();
                return;
            }
            host.apply_window_action("setSpaModalOpen", true);
        }
        body.read(cx).focus_handle().focus(window);
        self.phrases.draft = Some(PhraseDraft {
            title,
            body,
            target,
            scope: compact.then(|| self.phrases.source.clone()),
            picking: false,
            saving: false,
            exit_after_save: false,
            error: None,
            _subscriptions: subscriptions,
        });
        self.sync_overlay_guard();
        cx.notify();
    }

    pub(in crate::view) fn close_phrase_draft(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = &self.phrases.draft else {
            return;
        };
        if draft.saving || draft.title.read(cx).is_composing() || draft.body.read(cx).is_composing()
        {
            return;
        }
        self.phrases.draft = None;
        if let Some(host) = self.model.host() {
            host.apply_window_action("setSpaModalOpen", crate::editor::has_active_input_windows());
        }
        self.sync_overlay_guard();
        if self.favorites_active {
            if let Some(favorites) = &self.favorites {
                favorites.read(cx).focus_list(window);
            }
        } else {
            self.focus.focus(window);
        }
        self.restore_no_focus(window);
        cx.notify();
    }

    fn save_phrase_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.model.host() else {
            return;
        };
        let Some(draft) = &mut self.phrases.draft else {
            return;
        };
        if draft.saving || draft.title.read(cx).is_composing() || draft.body.read(cx).is_composing()
        {
            return;
        }
        let body = draft.body.read(cx).text().to_owned();
        if body.trim().is_empty()
            || !self.phrases.ready
            || !self.phrases.tree.folder_exists(&draft.target)
        {
            return;
        }
        let (target, title, compact) = (
            draft.target.clone(),
            draft.title.read(cx).text().to_owned(),
            draft.scope.is_some(),
        );
        draft.saving = true;
        draft
            .body
            .update(cx, |input, cx| input.set_read_only(true, cx));
        draft
            .title
            .update(cx, |input, cx| input.set_read_only(true, cx));
        cx.spawn_in(window, async move |this, cx| {
            let folder = target.clone();
            let result = cx
                .background_executor()
                .spawn(async move { host.save_text_favorite(&folder, &title, &body) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(draft) = this.phrases.draft.as_mut() else {
                    return;
                };
                draft.saving = false;
                let exit_after_save = std::mem::take(&mut draft.exit_after_save);
                match result {
                    Ok(value) => {
                        if compact {
                            if !this.phrases.group.is_empty() {
                                this.phrases.group = this
                                    .phrases
                                    .tabs
                                    .iter()
                                    .find(|(id, _)| {
                                        this.phrases.tree.contents(id).0.contains(&target)
                                    })
                                    .map(|(id, _)| id.clone())
                                    .unwrap_or_default();
                            }
                            this.phrases.reveal =
                                value["createdIds"][0].as_str().map(str::to_owned);
                        }
                        this.close_phrase_draft(window, cx);
                        this.invalidate_phrases(cx);
                        if exit_after_save {
                            this.request_exit(window, cx);
                        } else if !compact && this.favorites_active {
                            this.open_phrase_folder(
                                &target,
                                value["createdIds"][0].as_str(),
                                window,
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        draft.error = Some(error);
                        draft
                            .body
                            .update(cx, |input, cx| input.set_read_only(false, cx));
                        draft
                            .title
                            .update(cx, |input, cx| input.set_read_only(false, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::view) fn phrase_editor(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let draft = self.phrases.draft.as_ref()?;
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let enabled = !draft.saving
            && self.phrases.ready
            && self.phrases.tree.folder_exists(&draft.target)
            && !draft.body.read(cx).text().trim().is_empty();
        let card_width = (f32::from(window.viewport_size().width) - 24.).clamp(100., 392.);
        let card_height = (f32::from(window.viewport_size().height) - 24.).max(130.);
        let caption = tr("添加文本收藏", "Add text");
        let close = self
            .tool_button("phrase-close", "close", color(&t.text_muted), false, t)
            .on_click(cx.listener(|this, _, window, cx| this.close_phrase_draft(window, cx)));
        let picker = self.phrase_folder_button(true, t, cx);
        let field = |focused| {
            div()
                .flex_none()
                .flex()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .rounded_md()
                .border_1()
                .border_color(color(if focused {
                    &t.accent
                } else {
                    &t.border_secondary
                }))
        };
        let fields = div()
            .id("phrase-fields")
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(16.))
            .text_size(px(12.))
            .child(tr("正文", "Text"))
            .child(
                field(draft.body.read(cx).focus_handle().is_focused(window))
                    .child(draft.body.clone()),
            )
            .child(tr("标题（选填）", "Title (optional)"))
            .child(
                field(draft.title.read(cx).focus_handle().is_focused(window))
                    .child(draft.title.clone()),
            )
            .child(tr("保存到", "Save to"))
            .child(div().flex().child(picker))
            .when(draft.picking, |e| {
                e.child(self.phrase_folder_choices(true, t, cx))
            })
            .children(
                draft
                    .error
                    .as_ref()
                    .or(self.phrases.error.as_ref())
                    .map(|error| div().text_color(color(&t.error)).child(error.clone())),
            );
        let buttons = div()
            .flex_none()
            .flex()
            .justify_end()
            .gap_2()
            .px(px(18.))
            .py(px(10.))
            .text_size(px(12.))
            .bg(color(&t.background))
            .border_t_1()
            .border_color(color(&t.border))
            .child(
                div()
                    .id("phrase-cancel")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(color(&t.border_secondary))
                    .bg(color(&t.surface))
                    .cursor_pointer()
                    .child(tr("取消", "Cancel"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_phrase_draft(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("phrase-save")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(color(&t.accent))
                    .text_color(gpui::white())
                    .cursor_pointer()
                    .when(!enabled, |e| e.opacity(0.5))
                    .tooltip(preference_tooltip("Ctrl+Enter".into(), t.clone()))
                    .child(if draft.saving {
                        tr("保存中…", "Saving…")
                    } else {
                        tr("保存到收藏", "Save")
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.save_phrase_draft(window, cx)),
                    ),
            );
        Some(rsx! { <div absolute inset_0 flex items_center justify_center occlude bg={gpui::rgba(0x161c2838)}>
            <div w={px(card_width)} max_h={px(card_height)} flex flex_col bg={color(&t.surface)} border_1 border_color={color(&t.border)}
                rounded={px(12.)} shadow_lg overflow_hidden>
                <div flex_none flex items_center gap_2 pl={px(16.)} pr={px(10.)} pt={px(14.)}>
                    <div size={px(29.)} flex items_center justify_center rounded={px(8.)} bg={color(&t.pin).opacity(0.12)}>
                        {self.icon("star", 17., color(&t.pin))}
                    </div>
                    <div flex_1 font_weight={FontWeight::SEMIBOLD} text_size={px(15.)}>{caption}</div>{close}
                </div>{fields}{buttons}
            </div>
        </div> }.capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter" && event.keystroke.modifiers.control && event.keystroke.modifiers.number_of_modifiers() == 1 {
                if !event.is_held { this.save_phrase_draft(window, cx); }
                cx.stop_propagation();
            }
        })).on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).into_any_element())
    }
}
