//! Compact text-action use panel. The parent owns the external target and the
//! execution lease; this view emits an exact rule token and never reads a
//! selection or writes the system clipboard. Sample preview is local only.

use std::sync::Arc;

use cliphost_core::{host::HostState, i18n::tr, text_actions_ui};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FontWeight, KeyDownEvent,
    MouseButton, ScrollHandle, SharedString, Subscription, Window,
};
use gpui_rsx::rsx;
use serde_json::{json, Value};

use crate::{
    crisp_svg::svg,
    input::{InputColors, SearchEvent, SearchInput},
    multiline::{MultilineEvent, MultilineInput},
    preview::{DesignTokens, ThemeTokens},
    smooth_wheel::SmoothWheelExt,
    view::color,
};

pub const LAUNCHER_WIDTH: f32 = 560.0;
pub const LAUNCHER_HEIGHT: f32 = 500.0;
pub const LAUNCHER_MIN_WIDTH: f32 = 440.0;
const MAX_SAMPLE_CHARACTERS: usize = 4_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextActionLauncherEvent {
    Execute { id: String, rule_token: String },
    Manage,
    Dismiss,
}

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn selected_action<'a>(actions: &'a [Value], selected: Option<&str>) -> Option<&'a Value> {
    actions
        .iter()
        .find(|action| action["id"].as_str() == selected)
        .or_else(|| actions.first())
}

fn execute_event(action: &Value) -> Option<TextActionLauncherEvent> {
    let id = action["id"].as_str()?.to_string();
    let rule_token = action["ruleToken"].as_str()?.to_string();
    (!id.is_empty() && !rule_token.is_empty())
        .then_some(TextActionLauncherEvent::Execute { id, rule_token })
}

fn apply_label(action: &Value) -> &'static str {
    match action["type"].as_str() {
        Some("Send") => tr("插入这段文字", "Insert text"),
        Some("QuickerAction") => tr("运行这个动作", "Run action"),
        _ if action["needInput"] == false => tr("使用这个操作", "Use action"),
        _ => tr("替换选中文字", "Replace selection"),
    }
}

fn kind_mark(action: &Value) -> &'static str {
    match action["type"].as_str() {
        Some("SingleReplace") => "Aa",
        Some("Pair") => "[ ]",
        Some("Internal") => "≡",
        Some("ZExpression") => "ƒ",
        Some("Send") => "T",
        _ => "↗",
    }
}

pub struct TextActionLauncher {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    search: Entity<SearchInput>,
    sample: Entity<MultilineInput>,
    output: Entity<MultilineInput>,
    _subscriptions: Vec<Subscription>,
    actions: Vec<Value>,
    query: String,
    scope: &'static str,
    common_only: bool,
    selected: Option<String>,
    scroll: ScrollHandle,
    preview_scroll: ScrollHandle,
    preview_open: bool,
    preview_result: Option<Value>,
    preview_pending: bool,
    preview_generation: u64,
    sample_text: String,
    common_pending: Option<String>,
    message: Option<String>,
    generation: u64,
}

impl EventEmitter<TextActionLauncherEvent> for TextActionLauncher {}

impl TextActionLauncher {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| SearchInput::new(window, cx));
        let sample = cx.new(|cx| MultilineInput::new(window, cx));
        let output = cx.new(|cx| {
            let mut input = MultilineInput::new(window, cx);
            input.set_read_only(true, cx);
            input
        });
        let search_subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(query) => {
                    this.query = query.clone();
                    this.preview_open = false;
                    this.reconcile_selection(cx);
                }
                SearchEvent::Submit => this.execute(cx),
                SearchEvent::Escape => this.escape(window, cx),
            });
        let sample_subscription =
            cx.subscribe_in(&sample, window, |this, _, event, window, cx| match event {
                MultilineEvent::Changed(text) => {
                    this.sample_text = text.chars().take(MAX_SAMPLE_CHARACTERS).collect();
                    if this.sample_text != *text && !this.sample.read(cx).is_composing() {
                        let bounded = this.sample_text.clone();
                        this.sample
                            .update(cx, |input, cx| input.set_text(bounded, window, cx));
                    }
                    this.request_preview(cx);
                }
                MultilineEvent::Escape => this.escape(window, cx),
                _ => {}
            });
        let output_subscription = cx.subscribe_in(&output, window, |this, _, event, window, cx| {
            if matches!(event, MultilineEvent::Escape) {
                this.escape(window, cx);
            }
        });
        let mut this = Self {
            host,
            tokens,
            dark,
            search,
            sample,
            output,
            _subscriptions: vec![
                search_subscription,
                sample_subscription,
                output_subscription,
            ],
            actions: Vec::new(),
            query: String::new(),
            scope: "all",
            common_only: false,
            selected: None,
            scroll: ScrollHandle::new(),
            preview_scroll: ScrollHandle::new(),
            preview_open: false,
            preview_result: None,
            preview_pending: false,
            preview_generation: 0,
            sample_text: String::new(),
            common_pending: None,
            message: None,
            generation: 0,
        };
        this.configure_inputs(cx);
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.actions = self.host.text_action_list()["actions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        self.reconcile_selection(cx);
    }

    /// A new menu session never retains input from a previous external target.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.query.clear();
        self.scope = "all";
        self.common_only = false;
        self.selected = None;
        self.preview_open = false;
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_pending = false;
        self.preview_result = None;
        self.message = None;
        self.sample_text.clear();
        self.search
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.sample
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.output
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.reload(cx);
        self.focus_search(window, cx);
    }

    pub fn set_theme(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        self.configure_inputs(cx);
        cx.notify();
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.read(cx).focus_handle().focus(window);
    }

    pub fn has_composition(&self, cx: &App) -> bool {
        self.search.read(cx).is_composing()
            || self.sample.read(cx).is_composing()
            || self.output.read(cx).is_composing()
    }

    /// The parent calls this from capture phase. Enter in SearchInput remains
    /// owned by its Submit event, so the same physical key cannot run twice.
    pub fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.has_composition(cx) || event.keystroke.modifiers.number_of_modifiers() != 0 {
            return false;
        }
        let search_focused = self.search.read(cx).focus_handle().is_focused(window);
        let sample_focused = self.sample.read(cx).focus_handle().is_focused(window);
        let output_focused = self.output.read(cx).focus_handle().is_focused(window);
        match event.keystroke.key.as_str() {
            "down" if !self.preview_open => self.navigate(1, cx),
            "up" if !self.preview_open => self.navigate(-1, cx),
            "enter" if !search_focused && !sample_focused && !output_focused => {
                if !event.is_held {
                    self.execute(cx);
                }
            }
            "escape" => {
                if !event.is_held {
                    self.escape(window, cx);
                }
            }
            _ => return false,
        }
        window.prevent_default();
        cx.stop_propagation();
        true
    }

    fn configure_inputs(&mut self, cx: &mut Context<Self>) {
        let colors = input_colors(self.tokens.theme(self.dark));
        self.search.update(cx, |input, cx| {
            input.set_colors(colors, cx);
            input.set_metrics(30.0, 13.0, 0.0, cx);
            input.set_placeholder(
                tr("搜索名称、用途、类型…", "Search names, purpose, types…"),
                cx,
            );
        });
        self.sample.update(cx, |input, cx| {
            input.set_colors(colors, cx);
            input.set_metrics(88.0, 12.0, 10.0, cx);
            input.set_placeholder(tr("输入样例文字", "Enter sample text"), cx);
        });
        self.output.update(cx, |input, cx| {
            input.set_colors(colors, cx);
            input.set_metrics(88.0, 12.0, 10.0, cx);
            input.set_placeholder(tr("处理结果", "Result"), cx);
        });
    }

    fn filtered(&self) -> Vec<Value> {
        text_actions_ui::filter_actions(
            &self.actions,
            self.scope,
            None,
            self.common_only,
            &self.query,
        )
    }

    fn selected_action(&self) -> Option<Value> {
        selected_action(&self.filtered(), self.selected.as_deref()).cloned()
    }

    fn reconcile_selection(&mut self, cx: &mut Context<Self>) {
        let filtered = self.filtered();
        let previous = self.selected.clone();
        self.selected =
            selected_action(&filtered, self.selected.as_deref()).map(|action| string(action, "id"));
        if previous != self.selected {
            self.scroll.scroll_to_item(0);
        }
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_pending = false;
        self.preview_result = None;
        if self.preview_open {
            self.request_preview(cx);
        }
        cx.notify();
    }

    fn navigate(&mut self, delta: i32, cx: &mut Context<Self>) {
        let filtered = self.filtered();
        if filtered.is_empty() {
            return;
        }
        let current = filtered
            .iter()
            .position(|action| action["id"].as_str() == self.selected.as_deref())
            .unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, filtered.len() as i32 - 1) as usize;
        self.selected = Some(string(&filtered[next], "id"));
        self.scroll.scroll_to_item(next);
        cx.notify();
    }

    fn select(&mut self, action: &Value, cx: &mut Context<Self>) {
        self.selected = Some(string(action, "id"));
        self.message = None;
        cx.notify();
    }

    fn execute(&mut self, cx: &mut Context<Self>) {
        if self.has_composition(cx) {
            return;
        }
        if let Some(event) = self.selected_action().as_ref().and_then(execute_event) {
            cx.emit(event);
        }
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_composition(cx) {
            return;
        }
        if self.preview_open {
            self.preview_open = false;
            self.preview_generation = self.preview_generation.wrapping_add(1);
            self.preview_pending = false;
            self.focus_search(window, cx);
            cx.notify();
        } else {
            cx.emit(TextActionLauncherEvent::Dismiss);
        }
    }

    fn toggle_common(&mut self, action: Value, cx: &mut Context<Self>) {
        if self.common_pending.is_some() {
            return;
        }
        let id = string(&action, "id");
        let token = string(&action, "ruleToken");
        if id.is_empty() || token.is_empty() {
            return;
        }
        self.common_pending = Some(id.clone());
        let common = action["common"] != true;
        let host = self.host.clone();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.text_action_set_common(
                        &json!({"id":id, "common":common, "expectedRuleToken":token}),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.common_pending = None;
                if generation == this.generation {
                    this.message = result.err();
                }
                // Read actual persisted state; an old reply must not replace
                // a newer rule occupying the same positional ID.
                this.reload(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn open_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.selected_action() else {
            return;
        };
        self.preview_open = true;
        self.message = None;
        self.output
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        self.request_preview(cx);
        if action["needInput"] != false
            && !matches!(
                action["type"].as_str(),
                Some("QuickerAction" | "ZExpression")
            )
        {
            self.sample.read(cx).focus_handle().focus(window);
        }
        cx.notify();
    }

    fn request_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        if !self.preview_open {
            return;
        }
        let Some(action) = self.selected_action() else {
            return;
        };
        let generation = self.preview_generation;
        let host = self.host.clone();
        let input = self.sample_text.clone();
        self.preview_pending = true;
        self.preview_result = None;
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        host.text_action_preview(&json!({"action":action, "input":input}))
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                if !this.preview_open || this.preview_generation != generation {
                    return;
                }
                this.preview_pending = false;
                this.preview_result = Some(match result {
                    Ok(value) => value,
                    Err(error) => json!({"supported":false, "error":error, "output":""}),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn icon(&self, name: &str, size: f32, foreground: gpui::Hsla) -> impl IntoElement {
        svg()
            .path(self.tokens.icons.get(name).cloned().unwrap_or_default())
            .size(px(size))
            .flex_none()
            .text_color(foreground)
    }

    fn button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = self.tokens.theme(self.dark);
        let foreground = color(if primary {
            &theme.accent
        } else {
            &theme.text_secondary
        });
        let background = color(if primary {
            &theme.tab_active
        } else {
            &theme.secondary
        });
        let hover = color(&theme.hover);
        let id: gpui::ElementId = id.into();
        let label: SharedString = label.into();
        rsx! { <div id={id} flex_none min_h={px(28.0)} px={px(10.0)}
            flex items_center justify_center gap={px(5.0)} rounded={px(5.0)}
            text_size={px(11.0)} text_color={foreground} bg={background} cursor_pointer>
            {label}
        </div> }
        .hover(move |style| style.bg(hover))
    }

    fn row(&self, action: Value, cx: &Context<Self>) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let selected = action["id"].as_str() == self.selected.as_deref();
        let id = string(&action, "id");
        let title = string(&action, "title");
        let title = if title.is_empty() {
            string(&action, "typeLabel")
        } else {
            title
        };
        let description = string(&action, "description");
        let description = if description.is_empty() {
            string(&action, "summary")
        } else {
            description
        };
        let common = action["common"] == true;
        let pending = self.common_pending.as_deref() == Some(id.as_str());
        let select_action = action.clone();
        let star_action = action.clone();
        let star_tip: SharedString = if common {
            tr("取消常用", "Remove from common")
        } else {
            tr("设为常用", "Mark as common")
        }
        .into();
        let tip_tokens = self.tokens.clone();
        let tip_dark = self.dark;
        let hover = color(&theme.hover);
        let content = rsx! {
            <div id={SharedString::from(format!("text-launcher-rule-{id}"))} w_full min_h={px(60.0)}
                flex items_center rounded={px(6.0)}
                bg={if selected { color(&theme.selected) } else { gpui::transparent_black() }}>
                <div id={SharedString::from(format!("text-launcher-select-{id}"))} flex_1 min_w_0
                    flex items_center gap={px(11.0)} pl={px(11.0)} pr={px(4.0)} py={px(9.0)} cursor_pointer>
                    <div size={px(30.0)} flex_none flex items_center justify_center
                        rounded={px(6.0)} bg={color(&theme.secondary)} text_color={color(&theme.accent)}
                        text_size={px(14.0)}>{kind_mark(&action)}</div>
                    <div flex_1 min_w_0 flex flex_col gap={px(3.0)}>
                        <div text_size={px(13.0)} font_weight={FontWeight::MEDIUM}
                            text_ellipsis>{SharedString::from(title)}</div>
                        <div text_size={px(11.0)} text_color={color(&theme.text_muted)}
                            text_ellipsis>{SharedString::from(description)}</div>
                    </div>
                </div>
            </div>
        };
        // Preserve independent hit targets: a star click must never select or
        // execute a different rule as a side effect of persistence.
        content
            .child(
                rsx! { <div id={SharedString::from(format!("text-launcher-star-{id}"))}
                size={px(30.0)} mr={px(6.0)} flex_none flex items_center justify_center
                rounded={px(5.0)} cursor_pointer opacity={if pending { 0.45 } else { 1.0 }} /> }
                .hover(move |style| style.bg(hover))
                .child(self.icon(
                    "star",
                    14.0,
                    color(if common {
                        &theme.accent
                    } else {
                        &theme.text_muted
                    }),
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_common(star_action.clone(), cx);
                }))
                .tooltip(move |_, cx| {
                    cx.new(|_| LauncherTip {
                        text: star_tip.clone(),
                        tokens: tip_tokens.clone(),
                        dark: tip_dark,
                    })
                    .into()
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.select(&select_action, cx)),
            )
            .into_any_element()
    }

    fn preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let Some(action) = self.selected_action() else {
            return div().into_any_element();
        };
        let supported = self
            .preview_result
            .as_ref()
            .is_none_or(|value| value["supported"] != false);
        let local = !matches!(
            action["type"].as_str(),
            Some("ZExpression" | "QuickerAction")
        );
        let input_needed = action["needInput"] != false && local;
        let reason = self
            .preview_result
            .as_ref()
            .map(|value| {
                value["error"]
                    .as_str()
                    .or_else(|| value["unsupportedReason"].as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default();
        let back = self
            .button(
                "text-launcher-back",
                tr("‹ 返回操作列表", "‹ Back to actions"),
                false,
            )
            .on_click(cx.listener(|this, _, window, cx| this.escape(window, cx)));
        let mut body = rsx! {
            <div id="text-launcher-preview" flex_1 min_h_0 overflow_y_scroll
                px={px(18.0)} pb={px(14.0)} flex flex_col gap={px(9.0)}>
                <div text_size={px(15.0)} font_weight={FontWeight::SEMIBOLD}>
                    {SharedString::from(string(&action, "title"))}
                </div>
                <div text_size={px(11.0)} text_color={color(&theme.text_muted)}>
                    {SharedString::from(string(&action, "description"))}
                </div>
                <div flex items_center justify_between mt={px(6.0)}>
                    <div text_size={px(12.0)}>{tr("看看处理结果", "Try the result")}</div>
                    <div text_size={px(10.0)} text_color={color(&theme.text_muted)}>
                        {tr("最多 4000 字符", "Up to 4,000 characters")}
                    </div>
                </div>
            </div>
        }
        .track_scroll(&self.preview_scroll)
        .smooth_wheel("text-launcher-preview-wheel", &self.preview_scroll, cx);
        if input_needed && supported {
            body = body.child(rsx! {
                <div flex_none flex flex_col gap={px(5.0)}>
                    <div text_size={px(11.0)} text_color={color(&theme.text_secondary)}>{tr("输入", "Input")}</div>
                    {self.sample.clone()}
                </div>
            });
        }
        if local && supported {
            body = body.child(rsx! {
                <div flex_none flex flex_col gap={px(5.0)}>
                    <div flex items_center justify_between text_size={px(11.0)} text_color={color(&theme.text_secondary)}>
                        {tr("输出", "Output")}
                        <div text_size={px(10.0)} text_color={color(&theme.text_muted)}>
                            {if self.preview_pending { SharedString::from(tr("处理中…", "Processing…")) }
                             else { SharedString::from(format!("{} {}", self.preview_result.as_ref().map(|value| string(value, "output").chars().count()).unwrap_or(0), tr("字符", "characters"))) }}
                        </div>
                    </div>
                    {self.output.clone()}
                </div>
            });
        }
        if !reason.is_empty() {
            let error = self
                .preview_result
                .as_ref()
                .is_some_and(|value| value["error"].is_string());
            body = body.child(rsx! {
                <div text_size={px(11.0)} text_color={color(if error { &theme.error } else { &theme.text_muted })}>
                    {SharedString::from(reason)}
                </div>
            });
        }
        body = body.child(rsx! {
            <div text_size={px(10.0)} text_color={color(&theme.text_muted)}>
                {tr("仅处理这里的样例文字，不读取选中内容或写入剪贴板。", "Only this sample is processed. Selection and clipboard stay unchanged.")}
            </div>
        });
        rsx! { <div flex_1 min_h_0 flex flex_col>
            <div flex_none px={px(14.0)} py={px(7.0)}>{back}</div>
            {body}
        </div> }
        .into_any_element()
    }
}

fn input_colors(theme: &ThemeTokens) -> InputColors {
    InputColors {
        background: color(&theme.surface),
        foreground: color(&theme.text),
        placeholder: color(&theme.text_muted),
        border: color(&theme.border),
        selection: color(&theme.selected),
        caret: color(&theme.accent),
    }
}

struct LauncherTip {
    text: SharedString,
    tokens: DesignTokens,
    dark: bool,
}

impl Render for LauncherTip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let theme = self.tokens.theme(self.dark);
        rsx! { <div px={px(8.0)} py={px(5.0)} rounded={px(5.0)} text_size={px(11.0)}
        bg={color(&theme.surface)} text_color={color(&theme.text)} border_1
        border_color={color(&theme.border)}>{self.text.clone()}</div> }
    }
}

impl Render for TextActionLauncher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.tokens.theme(self.dark).clone();
        let filtered = self.filtered();
        let selected = selected_action(&filtered, self.selected.as_deref()).cloned();
        // Output is presentation-only and is never passed back to execution.
        let output_text = self
            .preview_result
            .as_ref()
            .map(|value| string(value, "output"))
            .unwrap_or_default();
        if !self.output.read(cx).is_composing() && self.output.read(cx).text() != output_text {
            self.output
                .update(cx, |input, cx| input.set_text(output_text, window, cx));
        }
        let close = self
            .button("text-launcher-close", "×", false)
            .on_click(cx.listener(|_, _, _, cx| cx.emit(TextActionLauncherEvent::Dismiss)));
        let header = rsx! { <div flex_none h={px(34.0)} px={px(14.0)} flex items_center gap={px(7.0)}
            border_b_1 border_color={color(&theme.border)}>
            {self.icon("text", 14.0, color(&theme.accent))}
            <div flex_1 text_size={px(12.0)} font_weight={FontWeight::SEMIBOLD}>{tr("文本操作", "Text actions")}</div>
            {close}
        </div> };
        let search = rsx! { <div flex_none h={px(46.0)} px={px(14.0)} flex items_center gap={px(8.0)}>
            {self.icon("search", 14.0, color(&theme.text_muted))}
            <div flex_1 min_w_0>{self.search.clone()}</div>
        </div> };
        let mut scopes = rsx! { <div flex_none min_h={px(32.0)} px={px(12.0)} pb={px(5.0)}
        flex items_center gap={px(4.0)} flex_wrap /> };
        for (scope, label) in [
            ("common", tr("★ 常用", "★ Common")),
            ("all", tr("全部", "All")),
            ("transform", tr("处理选中文字", "Transform selection")),
            ("insert", tr("插入文本", "Insert text")),
            ("external", tr("高级扩展", "Advanced")),
        ] {
            let active = if scope == "common" {
                self.common_only
            } else {
                !self.common_only && self.scope == scope
            };
            scopes = scopes.child(
                self.button(
                    SharedString::from(format!("text-launcher-scope-{scope}")),
                    label,
                    active,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.common_only = scope == "common" && !this.common_only;
                    this.scope = if scope == "common" { "all" } else { scope };
                    this.preview_open = false;
                    this.reconcile_selection(cx);
                    this.focus_search(window, cx);
                })),
            );
        }
        let body = if self.preview_open {
            self.preview(cx)
        } else {
            let heading = if self.common_only {
                tr("常用操作", "Common actions")
            } else {
                tr("全部操作", "Actions")
            };
            let mut list = rsx! { <div id="text-launcher-list" flex_1 min_h_0 overflow_y_scroll
            px={px(9.0)} pb={px(6.0)} flex flex_col gap={px(3.0)} /> }
            .track_scroll(&self.scroll)
            .smooth_wheel("text-launcher-list-wheel", &self.scroll, cx);
            for action in filtered.iter().cloned() {
                list = list.child(self.row(action, cx));
            }
            if filtered.is_empty() {
                let empty = if self.actions.is_empty() {
                    tr("还没有文本操作", "No text actions yet")
                } else {
                    tr("没有找到操作", "No matching actions")
                };
                list = list.child(rsx! { <div flex_1 min_h={px(160.0)} flex flex_col items_center justify_center gap={px(9.0)} text_color={color(&theme.text_muted)}>
                    <div text_size={px(13.0)}>{empty}</div>
                    <div text_size={px(11.0)}>{if self.actions.is_empty() { tr("在管理中添加自己的文字规则。", "Add your text rules in Manage.") } else { tr("换个关键词，或查看全部操作。", "Try another query or show all actions.") }}</div>
                </div> });
            }
            rsx! { <div flex_1 min_h_0 flex flex_col>
                <div flex_none px={px(20.0)} py={px(7.0)} flex items_center justify_between text_size={px(10.0)} text_color={color(&theme.text_muted)}>
                    {heading}<div>{SharedString::from(format!("{}", filtered.len()))}</div>
                </div>
                {list}
            </div> }.into_any_element()
        };
        let manage = self
            .button("text-launcher-manage", tr("管理操作", "Manage"), false)
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.has_composition(cx) {
                    cx.emit(TextActionLauncherEvent::Manage);
                }
            }));
        let mut footer = rsx! { <div flex_none min_h={px(57.0)} px={px(14.0)} py={px(9.0)}
            flex items_center gap={px(8.0)} border_t_1 border_color={color(&theme.border)} bg={color(&theme.surface)}>
            {manage}<div flex_1 min_w_0 />
        </div> };
        if let Some(action) = selected {
            if !self.preview_open {
                footer = footer.child(
                    self.button(
                        "text-launcher-show-preview",
                        tr("查看效果", "Preview"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.open_preview(window, cx))),
                );
            }
            footer = footer.child(
                self.button("text-launcher-apply", apply_label(&action), true)
                    .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
            );
        }
        let message = self.message.clone().unwrap_or_else(|| {
            tr(
                "↑↓ 选择 · Enter 使用 · Esc 关闭",
                "↑↓ Select · Enter Use · Esc Close",
            )
            .into()
        });
        rsx! { <div size_full min_w_0 min_h_0 flex flex_col overflow_hidden rounded={px(8.0)}
            border_1 border_color={color(&theme.border)} bg={color(&theme.background)}
            text_color={color(&theme.text)} font_family={SharedString::from(self.tokens.typography.font_family.clone())}>
            {header}{search}{scopes}{body}{footer}
            <div flex_none min_h={px(26.0)} px={px(14.0)} py={px(5.0)} text_size={px(10.0)}
                border_t_1 border_color={color(&theme.border)}
                text_color={color(if self.message.is_some() { &theme.error } else { &theme.text_muted })}>
                {SharedString::from(message)}
            </div>
        </div> }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_always_resolved_against_current_filtered_results() {
        let actions = vec![
            json!({"id":"1","ruleToken":"current"}),
            json!({"id":"4","ruleToken":"other"}),
        ];
        assert_eq!(selected_action(&actions, Some("hidden")), actions.first());
        assert_eq!(selected_action(&actions, Some("4")), actions.get(1));
        assert!(selected_action(&[], Some("1")).is_none());
    }

    #[test]
    fn execute_request_keeps_exact_rule_token_and_rejects_unversioned_rows() {
        assert_eq!(
            execute_event(&json!({"id":"3","ruleToken":"sha256:original"})),
            Some(TextActionLauncherEvent::Execute {
                id: "3".into(),
                rule_token: "sha256:original".into()
            })
        );
        assert!(execute_event(&json!({"id":"3"})).is_none());
        assert!(execute_event(&json!({"id":"3","ruleToken":""})).is_none());
    }
}
