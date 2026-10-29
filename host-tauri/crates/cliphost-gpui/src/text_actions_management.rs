//! Text-rule management presentation. All records and saves belong to HostState;
//! sample preview is a pure core operation and never executes an external action.

use super::*;
use crate::motion;
use gpui::{anchored, deferred, point, Corner, KeyDownEvent, MouseButton};
use std::{collections::BTreeSet, time::Instant};

pub(super) fn initialize(
    page: &mut TextActionsPage,
    window: &mut Window,
    cx: &mut Context<TextActionsPage>,
) {
    let colors = page.input_colors();
    let search = cx.new(|cx| {
        let mut input = SearchInput::new(window, cx);
        input.set_colors(colors, cx);
        input.set_metrics(33., 12., 10., cx);
        input.set_placeholder(
            tr("搜索名称、用途、类型…", "Search names, purposes, types…"),
            cx,
        );
        input.set_double_click_clears(false);
        input
    });
    page.manager_subscriptions
        .push(cx.subscribe_in(&search, window, |this, _, event, _, cx| {
            if let SearchEvent::Changed(query) = event {
                this.query = query.clone();
                if this.editor.is_none() {
                    this.detail_open = false;
                }
                this.invalidate_preview();
                cx.notify();
            }
        }));
    page.search_input = Some(search);
    let input = cx.new(|cx| {
        let mut input = MultilineInput::new(window, cx);
        input.set_colors(colors, cx);
        input.set_metrics(78., 12., 9., cx);
        input.set_placeholder(
            tr(
                "输入要试用的文字，不会读取系统选区",
                "Enter sample text; system selection is not read",
            ),
            cx,
        );
        input
    });
    page.manager_subscriptions
        .push(cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let MultilineEvent::Changed(text) = event {
                this.preview_text = text.clone();
                this.invalidate_preview();
                cx.notify();
            }
        }));
    page.preview_input = Some(input);
    page.preview_output = Some(cx.new(|cx| {
        let mut input = MultilineInput::new(window, cx);
        input.set_colors(colors, cx);
        input.set_metrics(90., 12., 9., cx);
        input.set_read_only(true, cx);
        input
    }));
}

fn actual_group(group: &str) -> (Option<&str>, bool) {
    match group {
        "all" => (None, false),
        "common" => (None, true),
        "ungrouped" => (Some(""), false),
        _ => (Some(group.strip_prefix("name:").unwrap_or(group)), false),
    }
}

fn selected_from_visible(visible: &[Value], selected_id: Option<&str>) -> Option<Value> {
    visible
        .iter()
        .find(|action| action["id"].as_str() == selected_id)
        .or_else(|| visible.first())
        .cloned()
}

fn editor_focus_keys(kind: &str, advanced: bool, delay_visible: bool) -> Vec<&'static str> {
    let mut keys = vec!["title", "group"];
    keys.extend_from_slice(match kind {
        "SingleReplace" => &["data:Pattern", "data:ReplaceMent"][..],
        "ZExpression" => &["data:Expresstion"][..],
        "QuickerAction" => &["data:Id"][..],
        "Pair" => &["data:Prefix", "data:Suffix"][..],
        "Send" => &["data:Text"][..],
        _ => &[],
    });
    if advanced {
        keys.push("description");
        if delay_visible {
            keys.push("simulateDelay");
        }
    }
    keys
}

fn cycle_focus_index(current: Option<usize>, len: usize, backwards: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, backwards) {
        (Some(index), true) => (index + len - 1) % len,
        (Some(index), false) => (index + 1) % len,
        (None, true) => len - 1,
        (None, false) => 0,
    })
}

pub(super) fn kind_label(kind: &str) -> &'static str {
    match kind {
        "Internal" => tr("清理／格式化", "Clean / format"),
        "SingleReplace" => tr("查找替换", "Find and replace"),
        "Pair" => tr("添加前后缀", "Add prefix / suffix"),
        "Send" => tr("插入文本", "Insert text"),
        "ZExpression" => tr("表达式", "Expression"),
        "QuickerAction" => tr("启动 Quicker 动作", "Launch Quicker action"),
        _ => tr("文本操作", "Text action"),
    }
}

impl TextActionsPage {
    fn manager_visible(&self) -> Vec<Value> {
        let (group, common) = actual_group(&self.group);
        cliphost_core::text_actions_ui::filter_actions(
            &self.actions,
            &self.scope,
            group,
            common,
            &self.query,
        )
    }

    fn manager_selected(&self) -> Option<Value> {
        // Selection is derived from the current result set synchronously. A
        // new query or filter cannot leave a hidden rule as the current target.
        let visible = self.manager_visible();
        selected_from_visible(&visible, self.selected_id.as_deref())
    }

    pub(super) fn invalidate_preview(&mut self) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_pending = false;
        self.preview_result = None;
        self.preview_error = None;
    }

    fn manager_select(&mut self, action: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() || self.composing(cx) {
            self.message = Some((
                true,
                tr(
                    "请先保存或取消当前编辑；草稿已保留。",
                    "Save or cancel the current edit; the draft is retained.",
                )
                .into(),
            ));
            self.detail_open = true;
            cx.notify();
            return;
        }
        self.selected_id = Some(text(&action, "id"));
        self.detail_open = true;
        self.message = None;
        self.invalidate_preview();
        self.manager_focus.focus(window);
        cx.notify();
    }

    fn manager_filter(
        &mut self,
        scope: Option<&str>,
        group: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.composing(cx) {
            return;
        }
        if let Some(scope) = scope {
            self.scope = scope.into();
            self.group = "all".into();
        }
        if let Some(group) = group {
            self.group = group;
        }
        if self.editor.is_none() {
            self.detail_open = false;
        }
        self.invalidate_preview();
        cx.notify();
    }

    fn manager_create(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some()
            || self.has_active_save_or_composition(cx)
            || self.common_pending.is_some()
        {
            return;
        }
        self.open_editor(None, window, cx);
        if self.editor.is_some() && kind != "SingleReplace" {
            self.change_type(kind, window, cx);
        }
        self.scope = "all".into();
        self.group = "all".into();
        self.create_open = false;
        self.create_advanced = false;
        cx.notify();
    }

    fn manager_common(&mut self, action: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.common_pending.is_some() || self.deleting || self.loading {
            return;
        }
        let id = text(&action, "id");
        let token = text(&action, "ruleToken");
        let common = action["common"] != true;
        self.common_pending = Some(id.clone());
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.text_action_set_common(
                        &json!({"id":id,"expectedRuleToken":token,"common":common}),
                    )
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.common_pending = None;
                match result {
                    Ok(saved) => {
                        if let Some(action) = this.actions.iter_mut().find(|action| {
                            action["id"] == saved["id"] && action["ruleToken"] == saved["ruleToken"]
                        }) {
                            *action = saved;
                        } else {
                            this.refresh(window, cx);
                        }
                    }
                    Err(error) => {
                        this.message = Some((true, error));
                        this.refresh(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn manager_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        let action = self
            .editor
            .as_ref()
            .map(|editor| editor.draft.clone())
            .or_else(|| self.manager_selected());
        let Some(action) = action else {
            return;
        };
        self.invalidate_preview();
        let generation = self.preview_generation;
        let input = self.preview_text.clone();
        let host = self.host.clone();
        self.preview_pending = true;
        cx.spawn_in(window, async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        host.text_action_preview(&json!({"action":action,"input":input}))
                    })
                    .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.preview_generation != generation {
                    return;
                }
                this.preview_pending = false;
                match result {
                    Ok(result) => {
                        this.preview_error = result["error"]
                            .as_str()
                            .or_else(|| result["unsupportedReason"].as_str())
                            .map(str::to_string);
                        if let Some(output) = &this.preview_output {
                            let value = text(&result, "output");
                            output.update(cx, |output, cx| output.set_text(value, window, cx));
                        }
                        this.preview_result = Some(result);
                    }
                    Err(error) => this.preview_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn manager_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.to_lowercase();
        let modifiers = event.keystroke.modifiers;
        let tab = key == "tab" && modifiers.number_of_modifiers() == u8::from(modifiers.shift);
        if self.composing(cx) {
            if tab {
                cx.stop_propagation();
            }
            return;
        }
        if tab && event.is_held {
            cx.stop_propagation();
            return;
        }
        if self.create_open
            && matches!(
                key.as_str(),
                "up" | "down" | "home" | "end" | "enter" | "space"
            )
        {
            let count = if self.create_advanced { 7 } else { 5 };
            match key.as_str() {
                "up" => self.create_selected = (self.create_selected + count - 1) % count,
                "down" => self.create_selected = (self.create_selected + 1) % count,
                "home" => self.create_selected = 0,
                "end" => self.create_selected = count - 1,
                _ if self.create_selected == 4 => self.create_advanced = !self.create_advanced,
                _ => {
                    let kind = [
                        "Internal",
                        "SingleReplace",
                        "Pair",
                        "Send",
                        "",
                        "ZExpression",
                        "QuickerAction",
                    ][self.create_selected];
                    self.manager_create(kind, window, cx);
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.create_open && matches!(key.as_str(), "escape" | "tab") {
            self.create_open = false;
            self.manager_focus.focus(window);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if tab {
            let width = self
                .manager_bounds
                .lock()
                .unwrap()
                .map(|bounds| f32::from(bounds.size.width))
                .unwrap_or(f32::from(window.viewport_size().width));
            let narrow = width < 760.;
            let mut handles = Vec::new();
            if let Some(input) = &self.search_input {
                handles.push(input.read(cx).focus_handle());
            }
            if !narrow || !self.detail_open {
                handles.push(self.manager_focus.clone());
            }
            if !narrow || self.detail_open {
                if let Some(editor) = &self.editor {
                    let kind = text(&editor.draft, "type");
                    let delay_visible =
                        action_needs_input(&self.types, &kind) && editor.draft["simulate"] == true;
                    for key in editor_focus_keys(&kind, self.advanced, delay_visible) {
                        if let Some(input) = self.singles.get(key) {
                            handles.push(input.read(cx).focus_handle());
                        } else if let Some(input) = self.multiples.get(key) {
                            handles.push(input.read(cx).focus_handle());
                        }
                    }
                }
                if self.editor.is_some() || self.manager_selected().is_some() {
                    if let Some(input) = &self.preview_input {
                        handles.push(input.read(cx).focus_handle());
                    }
                    if self.preview_result.as_ref().is_some_and(|result| {
                        result["supported"] == true && result["error"].is_null()
                    }) {
                        if let Some(output) = &self.preview_output {
                            handles.push(output.read(cx).focus_handle());
                        }
                    }
                }
            }
            let current = handles.iter().position(|handle| handle.is_focused(window));
            if let Some(next) = cycle_focus_index(current, handles.len(), modifiers.shift) {
                handles[next].focus(window);
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.control && key == "f" {
            if let Some(search) = &self.search_input {
                search.read(cx).focus_handle().focus(window);
            }
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.control && key == "s" && self.editor.is_some() {
            self.save(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.editor.is_none()
            && self.manager_focus.is_focused(window)
            && matches!(key.as_str(), "up" | "down")
        {
            let visible = self.manager_visible();
            let selected = self.manager_selected();
            let index = selected
                .as_ref()
                .and_then(|selected| {
                    visible
                        .iter()
                        .position(|action| action["id"] == selected["id"])
                })
                .unwrap_or(0);
            let next = if key == "down" {
                index.saturating_add(1).min(visible.len().saturating_sub(1))
            } else {
                index.saturating_sub(1)
            };
            if let Some(action) = visible.get(next) {
                self.manager_select(action.clone(), window, cx);
            }
            cx.stop_propagation();
        }
    }

    fn manager_button(
        &mut self,
        id: String,
        label: impl Into<SharedString>,
        selected: bool,
        enabled: bool,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let now = Instant::now();
        let progress = self.tweens.value(
            id.clone(),
            if enabled && self.hovered.contains(&id) {
                1.
            } else {
                0.
            },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if self.tweens.any_active(now) {
            window.request_animation_frame();
        }
        let label = label.into();
        div()
            .id(SharedString::from(id.clone()))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(5.))
            .min_h(px(29.))
            .rounded(px(5.))
            .text_size(px(12.))
            .bg(motion::mix_hsla(
                if selected {
                    color(&t.tab_active)
                } else {
                    gpui::transparent_black()
                },
                color(&t.hover),
                progress,
            ))
            .when(enabled, |button| button.cursor_pointer())
            .when(!enabled, |button| button.opacity(0.4))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hovered.insert(id.clone());
                } else {
                    this.hovered.remove(&id);
                }
                cx.notify();
            }))
            .when(!label.is_empty(), |button| button.child(label))
    }

    pub(super) fn render_preview(&self, t: &ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let result = self.preview_result.as_ref();
        rsx! { <div flex flex_col gap={px(8.)} mt={px(15.)} pt={px(15.)} border_t_1 border_color={color(&t.border)} /> }
            .child(div().flex().items_center().justify_between()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(tr("看看处理结果", "Try the result")))
                .child(self.button("text-action-preview", if self.preview_pending { tr("计算中…", "Computing…") } else { tr("预览结果", "Preview result") }, !self.preview_pending, t)
                    .on_click(cx.listener(|this, _, window, cx| this.manager_preview(window, cx)))))
            .child(div().text_size(px(11.)).text_color(color(&t.text_muted)).child(tr("输入", "Input")))
            .children(self.preview_input.clone())
            .child(div().text_size(px(11.)).text_color(color(&t.text_muted)).child(tr("输出", "Output")))
            .when(result.is_some_and(|result| result["supported"] == true && result["error"].is_null()), |body| body.children(self.preview_output.clone()))
            .when(self.preview_error.is_some(), |body| body.child(div().text_size(px(11.)).text_color(color(&t.text_muted)).child(self.preview_error.clone().unwrap_or_default())))
            .when(result.is_none() && self.preview_error.is_none(), |body| body.child(div().min_h(px(64.)).p(px(9.)).rounded(px(5.)).border_1().border_color(color(&t.border))
                .text_size(px(11.)).text_color(color(&t.text_muted)).child(tr("输入样例后预览；修改规则或输入会清除旧结果。", "Enter a sample and preview it. Changing the rule or input clears the old result."))))
            .child(div().text_size(px(10.)).text_color(color(&t.text_muted)).child(tr("只处理这里的文字，不读取选区或写入剪贴板。", "Only this sample is processed; selection and clipboard are untouched.")))
            .into_any_element()
    }

    fn render_manager_detail(
        &mut self,
        action: Value,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = text(&action, "title");
        let description = text(&action, "description");
        let edit = action.clone();
        let common = action.clone();
        let delete = action.clone();
        let enabled = !self.loading && !self.deleting && self.common_pending.is_none();
        let edit_button = self
            .manager_button(
                "manager-edit".into(),
                tr("编辑", "Edit"),
                false,
                enabled,
                t,
                window,
                cx,
            )
            .border_1()
            .border_color(color(&t.border))
            .on_click(cx.listener(move |this, _, window, cx| {
                if !this.loading && !this.deleting && this.common_pending.is_none() {
                    this.open_editor(Some(edit.clone()), window, cx);
                }
            }));
        let common_button = self
            .manager_button(
                "manager-common".into(),
                if action["common"] == true {
                    tr("★ 已设为常用", "★ Common")
                } else {
                    tr("☆ 设为常用", "☆ Mark common")
                },
                false,
                enabled,
                t,
                window,
                cx,
            )
            .text_size(px(11.))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.manager_common(common.clone(), window, cx)
            }));
        let delete_button = self
            .manager_button(
                "manager-delete".into(),
                tr("删除", "Delete"),
                false,
                enabled,
                t,
                window,
                cx,
            )
            .text_color(color(&t.text_muted))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.deleting && !this.loading {
                    this.delete_target = Some(delete.clone());
                    cx.notify();
                }
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .id("text-action-detail-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .smooth_scroll("text-action-detail-scroll", cx)
                    .px(px(20.))
                    .py(px(17.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(color(&t.text_muted))
                                            .child(kind_label(&text(&action, "type"))),
                                    )
                                    .child(
                                        div()
                                            .mt(px(3.))
                                            .text_size(px(18.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(if title.is_empty() {
                                                kind_label(&text(&action, "type")).to_string()
                                            } else {
                                                title
                                            }),
                                    ),
                            )
                            .child(edit_button),
                    )
                    .child(
                        div()
                            .mt(px(8.))
                            .text_size(px(11.))
                            .text_color(color(&t.text_muted))
                            .child(if description.is_empty() {
                                text(&action, "summary")
                            } else {
                                description
                            }),
                    )
                    .child(
                        div()
                            .mt(px(9.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(color(&t.text_muted))
                                    .child(if text(&action, "group").trim().is_empty() {
                                        tr("未分组", "Ungrouped").to_string()
                                    } else {
                                        text(&action, "group")
                                    }),
                            )
                            .child(common_button),
                    )
                    .child(
                        div()
                            .mt(px(18.))
                            .text_size(px(11.))
                            .text_color(color(&t.text_muted))
                            .child(tr("处理方式", "Operation")),
                    )
                    .child(
                        div()
                            .mt(px(6.))
                            .text_size(px(12.))
                            .child(text(&action, "summary")),
                    )
                    .child(self.render_preview(t, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(52.))
                    .px(px(16.))
                    .border_t_1()
                    .border_color(color(&t.border))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(delete_button)
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(color(&t.text_muted))
                            .child(tr("用于处理所选文本", "For selected text")),
                    ),
            )
            .into_any_element()
    }

    fn render_create_popup(
        &mut self,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bounds = *self.manager_bounds.lock().unwrap();
        let position = bounds
            .map(|bounds| point(bounds.right() - px(14.), bounds.top() + px(48.)))
            .unwrap_or_else(|| point(window.viewport_size().width - px(24.), px(100.)));
        let mut card = div()
            .id("text-rule-create-popup")
            .w(px(235.))
            .p(px(7.))
            .rounded(px(7.))
            .border_1()
            .border_color(color(&t.border))
            .bg(color(&t.surface))
            .shadow_md()
            .flex()
            .flex_col()
            .occlude()
            .track_focus(&self.popup_focus)
            .on_key_down(cx.listener(Self::manager_key_down))
            .on_mouse_down_out(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                if !this
                    .create_trigger_bounds
                    .lock()
                    .unwrap()
                    .is_some_and(|bounds| bounds.contains(&event.position))
                {
                    this.create_open = false;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(8.))
                    .py(px(5.))
                    .text_size(px(11.))
                    .text_color(color(&t.text_muted))
                    .child(tr("你想对文字做什么？", "What should this action do?")),
            );
        for (index, (kind, description)) in [
            (
                "Internal",
                tr("去空行、整理 JSON", "Remove empty lines, format JSON"),
            ),
            (
                "SingleReplace",
                tr("替换文字，可选正则", "Replace text, optionally with regex"),
            ),
            (
                "Pair",
                tr(
                    "引号、括号、Markdown 标记",
                    "Quotes, brackets, Markdown markers",
                ),
            ),
            (
                "Send",
                tr(
                    "发送预设文字，不需要选区",
                    "Insert preset text without a selection",
                ),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            card = card.child(
                self.manager_button(
                    format!("create-task:{kind}"),
                    "",
                    self.create_selected == index,
                    true,
                    t,
                    window,
                    cx,
                )
                .w_full()
                .flex_col()
                .items_start()
                .gap(px(1.))
                .child(div().child(kind_label(kind)))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(color(&t.text_muted))
                        .child(description),
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.manager_create(kind, window, cx)),
                ),
            );
        }
        card = card.child(
            self.manager_button(
                "create-advanced".into(),
                tr("高级扩展  ›", "Advanced extensions  ›"),
                self.create_selected == 4,
                true,
                t,
                window,
                cx,
            )
            .mt(px(5.))
            .border_t_1()
            .border_color(color(&t.border))
            .text_size(px(11.))
            .text_color(color(&t.text_muted))
            .on_click(cx.listener(|this, _, _, cx| {
                this.create_advanced = !this.create_advanced;
                cx.notify();
            })),
        );
        if self.create_advanced {
            for (index, kind) in ["ZExpression", "QuickerAction"].into_iter().enumerate() {
                card =
                    card.child(
                        self.manager_button(
                            format!("create-task:{kind}"),
                            kind_label(kind),
                            self.create_selected == index + 5,
                            true,
                            t,
                            window,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.manager_create(kind, window, cx),
                        )),
                    );
            }
        }
        deferred(
            anchored()
                .anchor(Corner::TopRight)
                .position(position)
                .snap_to_window_with_margin(px(8.))
                .child(card),
        )
        .with_priority(2)
        .into_any_element()
    }
}

pub(super) fn render(
    page: &mut TextActionsPage,
    window: &mut Window,
    cx: &mut Context<TextActionsPage>,
) -> AnyElement {
    let t = page.tokens.theme(page.dark).clone();
    let width = page
        .manager_bounds
        .lock()
        .unwrap()
        .map(|bounds| f32::from(bounds.size.width))
        .unwrap_or(f32::from(window.viewport_size().width));
    let narrow = width < 760.;
    let visible = page.manager_visible();
    let selected = page.manager_selected();
    let enabled = !page.loading && !page.deleting;
    let mut root = div()
        .id("text-actions-manager")
        .relative()
        .flex()
        .flex_col()
        .min_w_0()
        .min_h_0()
        .text_size(px(12.))
        .text_color(color(&t.text))
        .bg(color(&t.background))
        .when(page.embedded, |page| page.w_full().h(px(520.)))
        .when(!page.embedded, |page| page.size_full())
        .track_focus(&page.manager_focus)
        // Capture Tab before MultilineInput treats it as literal text. Only
        // controls rendered by this manager participate in its focus cycle.
        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key.eq_ignore_ascii_case("tab") {
                this.manager_key_down(event, window, cx);
            }
        }))
        .on_key_down(cx.listener(TextActionsPage::manager_key_down));
    let trigger_bounds = page.create_trigger_bounds.clone();
    let refresh = page
        .manager_button(
            "manager-refresh".into(),
            tr("刷新", "Refresh"),
            false,
            enabled && page.common_pending.is_none(),
            &t,
            window,
            cx,
        )
        .text_color(color(&t.text_muted))
        .text_size(px(11.))
        .on_click(cx.listener(|this, _, window, cx| {
            if !this.loading && !this.deleting && this.common_pending.is_none() {
                this.refresh(window, cx);
            }
        }));
    let add = page
        .manager_button(
            "manager-add".into(),
            tr("＋ 添加操作", "+ Add action"),
            false,
            enabled && page.editor.is_none(),
            &t,
            window,
            cx,
        )
        .relative()
        .child(
            gpui::canvas(
                move |bounds, _, _| {
                    *trigger_bounds.lock().unwrap() = Some(bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
            .size_full(),
        )
        .bg(color(&t.accent))
        .text_color(color(&t.background))
        .on_click(cx.listener(|this, _, window, cx| {
            if this.loading || this.deleting || this.editor.is_some() || this.composing(cx) {
                return;
            }
            this.create_open = !this.create_open;
            if this.create_open {
                this.create_selected = 0;
                this.popup_focus.focus(window);
            }
            cx.notify();
        }));
    root = root.child(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .py(px(11.))
            .border_b_1()
            .border_color(color(&t.border))
            .child(
                div()
                    .flex_1()
                    .max_w(px(460.))
                    .min_w_0()
                    .children(page.search_input.clone()),
            )
            .child(div().flex_1())
            .child(refresh)
            .child(add),
    );
    let mut scopes = div()
        .flex_none()
        .flex()
        .flex_wrap()
        .gap(px(5.))
        .px(px(16.))
        .py(px(7.))
        .border_b_1()
        .border_color(color(&t.border));
    for (id, label) in [
        ("all", tr("全部", "All")),
        ("transform", tr("处理选中文字", "Transform text")),
        ("insert", tr("插入文本", "Insert text")),
        ("external", tr("高级扩展", "Advanced")),
    ] {
        scopes = scopes.child(
            page.manager_button(
                format!("manager-scope:{id}"),
                label,
                page.scope == id,
                enabled,
                &t,
                window,
                cx,
            )
            .text_size(px(11.))
            .min_h(px(25.))
            .on_click(cx.listener(move |this, _, _, cx| this.manager_filter(Some(id), None, cx))),
        );
    }
    root = root.child(scopes);
    if let Some((error, message)) = &page.message {
        root = root.child(
            div()
                .flex_none()
                .px(px(16.))
                .py(px(6.))
                .text_size(px(11.))
                .text_color(color(if *error { &t.error } else { &t.accent }))
                .child(message.clone()),
        );
    }
    if let Some(target) = page.delete_target.clone() {
        root = root.child(
            div()
                .flex_none()
                .px(px(16.))
                .py(px(8.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .border_b_1()
                .border_color(color(&t.error))
                .child(div().flex_1().child(format!(
                    "{}「{}」{}",
                    tr("确认删除", "Delete "),
                    text(&target, "title"),
                    tr("？此操作不可恢复。", "? This cannot be undone.")
                )))
                .child(
                    page.button(
                        "manager-cancel-delete",
                        tr("取消", "Cancel"),
                        !page.deleting,
                        &t,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.deleting {
                            this.delete_target = None;
                            cx.notify();
                        }
                    })),
                )
                .child(
                    page.button(
                        "manager-confirm-delete",
                        tr("确认删除", "Delete"),
                        !page.deleting,
                        &t,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
                ),
        );
    }
    let groups: BTreeSet<_> = page
        .actions
        .iter()
        .map(|action| text(action, "group"))
        .filter(|group| !group.is_empty())
        .collect();
    let mut group_rows = vec![
        (
            "all".to_string(),
            tr("全部操作", "All actions").to_string(),
            page.actions.len(),
        ),
        (
            "common".to_string(),
            tr("★ 常用", "★ Common").to_string(),
            page.actions
                .iter()
                .filter(|action| action["common"] == true)
                .count(),
        ),
        (
            "ungrouped".to_string(),
            tr("未分组", "Ungrouped").to_string(),
            page.actions
                .iter()
                .filter(|action| text(action, "group").is_empty())
                .count(),
        ),
    ];
    group_rows.extend(groups.into_iter().map(|group| {
        (
            format!("name:{group}"),
            group.clone(),
            page.actions
                .iter()
                .filter(|action| text(action, "group") == group)
                .count(),
        )
    }));
    let mut sidebar = div()
        .id("text-rule-groups")
        .flex_none()
        .w(px(if narrow { 112. } else { 132. }))
        .min_h_0()
        .overflow_y_scroll()
        .smooth_scroll("text-rule-groups", cx)
        .px(px(8.))
        .py(px(13.))
        .border_r_1()
        .border_color(color(&t.border));
    for (id, label, count) in group_rows {
        let selected = page.group == id;
        sidebar = sidebar.child(
            page.manager_button(
                format!("manager-group:{id}"),
                "",
                selected,
                enabled,
                &t,
                window,
                cx,
            )
            .w_full()
            .justify_between()
            .px(px(8.))
            .min_h(px(32.))
            .child(div().flex_1().min_w_0().text_size(px(12.)).child(label))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(color(&t.text_muted))
                    .child(count.to_string()),
            )
            .on_click(
                cx.listener(move |this, _, _, cx| this.manager_filter(None, Some(id.clone()), cx)),
            ),
        );
    }
    let mut body = div().flex_1().min_h_0().min_w_0().flex().child(sidebar);
    if !narrow || !page.detail_open {
        let mut list = div()
            .id("text-action-list")
            .min_h_0()
            .flex_1()
            .overflow_y_scroll()
            .smooth_scroll("text-action-list", cx)
            .px(px(6.))
            .pb(px(8.));
        if page.loading {
            list = list.child(
                div()
                    .p(px(10.))
                    .text_color(color(&t.text_muted))
                    .child(tr("加载中…", "Loading…")),
            );
        }
        if !page.loading && visible.is_empty() {
            list = list.child(div().p(px(14.)).text_color(color(&t.text_muted)).child(tr(
                "没有找到操作。试试其它用途、分组或关键词。",
                "No actions found. Try another purpose, group or query.",
            )));
        }
        for action in &visible {
            let id = text(action, "id");
            let active = selected
                .as_ref()
                .is_some_and(|selected| selected["id"] == action["id"]);
            let choose = action.clone();
            let star = action.clone();
            let common_button = page
                .manager_button(
                    format!("row-common:{id}"),
                    if action["common"] == true {
                        "★"
                    } else {
                        "☆"
                    },
                    false,
                    enabled && page.common_pending.is_none(),
                    &t,
                    window,
                    cx,
                )
                .px(px(3.))
                .min_h(px(24.))
                .text_size(px(12.))
                .text_color(color(&t.text_muted))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.manager_common(star.clone(), window, cx);
                }));
            let title = text(action, "title");
            let description = text(action, "description");
            let row = page
                .manager_button(
                    format!("manager-row:{id}"),
                    "",
                    active,
                    enabled,
                    &t,
                    window,
                    cx,
                )
                .w_full()
                .items_start()
                .px(px(10.))
                .py(px(10.))
                .mb(px(1.))
                .rounded(px(6.))
                .border_l_2()
                .border_color(if active {
                    color(&t.accent)
                } else {
                    gpui::transparent_black()
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(if title.is_empty() {
                                    kind_label(&text(action, "type")).to_string()
                                } else {
                                    title
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(color(&t.text_muted))
                                .text_ellipsis()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(if description.is_empty() {
                                    text(action, "summary")
                                } else {
                                    description
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(color(&t.text_muted))
                                .child(format!(
                                    "{} · {}",
                                    if text(action, "group").is_empty() {
                                        tr("未分组", "Ungrouped").to_string()
                                    } else {
                                        text(action, "group")
                                    },
                                    kind_label(&text(action, "type"))
                                )),
                        ),
                )
                .child(common_button)
                .on_click(cx.listener(move |this, _, window, cx| {
                    if !this.loading && !this.deleting {
                        this.manager_select(choose.clone(), window, cx);
                    }
                }));
            list = list.child(row);
        }
        body = body.child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .bg(color(&t.surface))
                .border_r_1()
                .border_color(color(&t.border))
                .when(narrow, |pane| pane.flex_1())
                .when(!narrow, |pane| pane.flex_none().w(px(278.)))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .justify_between()
                        .px(px(14.))
                        .py(px(10.))
                        .text_size(px(11.))
                        .child(tr("操作列表", "Actions"))
                        .child(visible.len().to_string()),
                )
                .child(list),
        );
    }
    if !narrow || page.detail_open {
        let mut detail = div().flex_1().min_w_0().min_h_0().flex().flex_col();
        if narrow {
            detail = detail.child(
                page.manager_button(
                    "manager-back".into(),
                    if page.editor.is_some() {
                        tr("‹ 返回列表 · 草稿保留", "‹ Back to list · draft retained")
                    } else {
                        tr("‹ 返回列表", "‹ Back to list")
                    },
                    false,
                    true,
                    &t,
                    window,
                    cx,
                )
                .rounded(px(0.))
                .border_b_1()
                .border_color(color(&t.border))
                .on_click(cx.listener(|this, _, _, cx| {
                    // A draft remains retained while the list is shown.
                    if this.composing(cx) {
                        return;
                    }
                    this.detail_open = false;
                    cx.notify();
                })),
            );
        }
        detail = detail.child(if page.editor.is_some() {
            page.render_editor(&t, cx)
        } else if let Some(action) = selected {
            page.render_manager_detail(action, &t, window, cx)
        } else {
            div()
                .flex_1()
                .p(px(20.))
                .text_color(color(&t.text_muted))
                .child(tr(
                    "选择一个操作，查看规则与效果。",
                    "Select an action to inspect its rule and try a sample.",
                ))
                .into_any_element()
        });
        body = body.child(detail);
    }
    let manager_bounds = page.manager_bounds.clone();
    let weak = cx.weak_entity();
    root = root.child(body).child(
        gpui::canvas(
            move |bounds, window, _| {
                let changed = {
                    let mut current = manager_bounds.lock().unwrap();
                    let changed =
                        current.is_none_or(|previous| previous.size.width != bounds.size.width);
                    *current = Some(bounds);
                    changed
                };
                if changed {
                    let weak = weak.clone();
                    window.on_next_frame(move |_, cx| {
                        let _ = weak.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
        .size_full(),
    );
    if page.create_open {
        root = root.child(page.render_create_popup(&t, window, cx));
    }
    root.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn group_names_do_not_collide_with_reserved_views() {
        assert_eq!(actual_group("common"), (None, true));
        assert_eq!(actual_group("name:common"), (Some("common"), false));
        assert_eq!(actual_group("name:all"), (Some("all"), false));
        assert_eq!(actual_group("ungrouped"), (Some(""), false));
    }

    #[test]
    fn filtered_selection_never_keeps_a_hidden_rule_or_empty_target() {
        let visible = vec![json!({"id":"4","title":"Visible"}), json!({"id":"7"})];
        assert_eq!(
            selected_from_visible(&visible, Some("old")),
            Some(visible[0].clone())
        );
        assert_eq!(
            selected_from_visible(&visible, Some("7")),
            Some(visible[1].clone())
        );
        assert_eq!(selected_from_visible(&[], Some("old")), None);
    }

    #[test]
    fn editor_focus_order_excludes_collapsed_and_unrelated_controls() {
        assert_eq!(
            editor_focus_keys("SingleReplace", false, true),
            ["title", "group", "data:Pattern", "data:ReplaceMent"]
        );
        assert_eq!(
            editor_focus_keys("Send", true, false),
            ["title", "group", "data:Text", "description"]
        );
        assert_eq!(
            editor_focus_keys("Pair", true, true),
            [
                "title",
                "group",
                "data:Prefix",
                "data:Suffix",
                "description",
                "simulateDelay"
            ]
        );
    }

    #[test]
    fn focus_cycle_wraps_both_ways_and_accepts_no_current_target() {
        assert_eq!(cycle_focus_index(Some(0), 3, true), Some(2));
        assert_eq!(cycle_focus_index(Some(2), 3, false), Some(0));
        assert_eq!(cycle_focus_index(None, 3, true), Some(2));
        assert_eq!(cycle_focus_index(None, 3, false), Some(0));
        assert_eq!(cycle_focus_index(None, 0, false), None);
    }
}
