//! Native text-action settings, using the existing core action catalogue and
//! persistence. List refreshes never rebuild an open editor. Each editor owns
//! a session and draft revision, so a completed save cannot close a new draft.

use crate::smooth_wheel::SmoothWheelExt;
use gpui_rsx::rsx;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use cliphost_core::{host::HostState, i18n::tr};
use gpui::{
    div, prelude::*, px, AnyElement, App, Bounds, Context, Entity, FocusHandle, FontWeight, Pixels,
    SharedString, Subscription, Window,
};
use serde_json::{json, Value};

use crate::{
    input::{InputColors, SearchEvent, SearchInput},
    multiline::{MultilineEvent, MultilineInput},
    preview::{DesignTokens, ThemeTokens},
    view::color,
};

#[path = "text_actions_management.rs"]
mod management;

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn default_data(kind: &str) -> Value {
    match kind {
        "Internal" => json!({"ObjType": 1, "Type": "RemoveEmptyLine"}),
        "ZExpression" => json!({"ObjType": 4, "Expresstion": "$={text}"}),
        "QuickerAction" => json!({"ObjType": 5, "Id": "", "WaitResult": false}),
        "Pair" => {
            json!({"ObjType": 6, "Prefix": "", "Suffix": "", "MultiLines": false, "Trim": false})
        }
        "Send" => json!({"ObjType": 7, "Text": ""}),
        _ => json!({"ObjType": 2, "Pattern": "", "ReplaceMent": "", "EscapePattern": false,
            "EscapeReplaceMent": true, "UseRegex": false, "Options": 0}),
    }
}

fn editor_draft(action: Option<&Value>, group: &str) -> Value {
    let kind = action
        .and_then(|action| action["type"].as_str())
        .unwrap_or("SingleReplace");
    let mut data = default_data(kind);
    if let Some(existing) = action.and_then(|action| action["data"].as_object()) {
        data.as_object_mut().unwrap().extend(existing.clone());
    }
    let mut draft = json!({"id": action.map(|action| action["id"].clone()).unwrap_or(Value::Null),
        "title": action.map(|action| text(action, "title")).unwrap_or_default(),
        "description": action.map(|action| text(action, "description")).unwrap_or_default(),
        "group": action.map(|action| text(action, "group")).unwrap_or_else(|| group.to_string()),
        "type": kind, "simulate": action.is_some_and(|action| action["simulate"] == true),
        "simulateDelay": action.map(|action| action["simulateDelay"].as_u64().unwrap_or(0)).unwrap_or(0),
        "data": data});
    if let Some(token) = action.and_then(|action| action["ruleToken"].as_str()) {
        draft["expectedRuleToken"] = json!(token);
    }
    draft
}

fn regex_bit(id: &str) -> u64 {
    match id {
        "IgnoreCase" => 1,
        "Multiline" => 2,
        "Singleline" => 16,
        "RightToLeft" => 64,
        _ => 0,
    }
}

/// Only the explicit actionId/create part is consumed. Repeated refreshes and
/// localized rendering must not turn the same route into another editor reset.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RouteRequest {
    Edit(String),
    Create,
}

/// A consumed route is tied to an open editor, not to the lifetime of the
/// settings page. List/theme refreshes have no route to consume on their own.
#[derive(Default)]
struct EditorRoutes {
    pending: Option<RouteRequest>,
    consumed: Option<RouteRequest>,
}

impl EditorRoutes {
    fn accept(&mut self, request: Option<RouteRequest>) -> bool {
        if request.is_none() {
            self.retire();
            return false;
        }
        if request == self.consumed {
            return false;
        }
        self.pending = request;
        true
    }
    fn consume(&mut self) -> Option<RouteRequest> {
        let request = self.pending.take()?;
        self.consumed = Some(request.clone());
        Some(request)
    }
    fn retire(&mut self) {
        self.pending = None;
        self.consumed = None;
    }
}

fn valid_delay(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
        .filter(|delay| *delay <= 100)
}

fn action_needs_input(types: &[Value], kind: &str) -> bool {
    types
        .iter()
        .find(|option| option["id"] == kind)
        .is_none_or(|option| option["needInput"] != false)
}

fn delay_for_save(
    draft: &Value,
    types: &[Value],
    last_valid_delay: u64,
    control: &str,
) -> Result<u64, String> {
    if !action_needs_input(types, &text(draft, "type")) || draft["simulate"] != true {
        // The hidden unfinished field has no effect on this action's output.
        // Keep the last valid configured value instead of parsing it anyway.
        return Ok(last_valid_delay);
    }
    valid_delay(&json!(control)).ok_or_else(|| {
        tr(
            "输入延迟需要填写 0–100 的整数",
            "Input delay must be an integer from 0 to 100.",
        )
        .into()
    })
}

fn decode_query(value: &str) -> String {
    let mut bytes = Vec::new();
    let source = value.as_bytes();
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'%' && i + 2 < source.len() {
            if let Some(byte) = std::str::from_utf8(&source[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                bytes.push(byte);
                i += 3;
                continue;
            }
        }
        bytes.push(if source[i] == b'+' { b' ' } else { source[i] });
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn route_request(route: &str) -> Option<RouteRequest> {
    let query = route
        .split_once('?')?
        .1
        .split('#')
        .next()
        .unwrap_or_default();
    let values: Vec<_> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();
    if let Some((_, id)) = values.iter().find(|(key, _)| *key == "actionId") {
        return Some(RouteRequest::Edit(decode_query(id)));
    }
    values
        .iter()
        .any(|(key, value)| *key == "create" && *value == "1")
        .then_some(RouteRequest::Create)
}

struct Editor {
    session: u64,
    revision: u64,
    draft: Value,
    cache: HashMap<String, Value>,
    last_valid_delay: u64,
    saving: bool,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum SaveReceipt {
    Retired,
    Close,
    KeepDraft,
}

fn accept_saved(
    editor: Option<&mut Editor>,
    session: u64,
    revision: u64,
    saved: &Value,
    composing: bool,
) -> SaveReceipt {
    let Some(editor) = editor.filter(|editor| editor.session == session) else {
        return SaveReceipt::Retired;
    };
    editor.saving = false;
    // A continued new draft must use the ID returned by its first save.
    editor.draft["id"] = saved["id"].clone();
    editor.draft["expectedRuleToken"] = saved["ruleToken"].clone();
    if editor.revision == revision && !composing {
        SaveReceipt::Close
    } else {
        SaveReceipt::KeepDraft
    }
}

pub struct TextActionsPage {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    embedded: bool,
    actions: Vec<Value>,
    types: Vec<Value>,
    internal_types: Vec<Value>,
    regex_options: Vec<Value>,
    group: String,
    loaded: bool,
    loading: bool,
    load_generation: u64,
    next_session: u64,
    editor: Option<Editor>,
    routes: EditorRoutes,
    delete_target: Option<Value>,
    deleting: bool,
    message: Option<(bool, String)>,
    singles: HashMap<String, Entity<SearchInput>>,
    multiples: HashMap<String, Entity<MultilineInput>>,
    subscriptions: Vec<Subscription>,
    query: String,
    scope: String,
    selected_id: Option<String>,
    detail_open: bool,
    advanced: bool,
    create_open: bool,
    create_advanced: bool,
    create_selected: usize,
    common_pending: Option<String>,
    search_input: Option<Entity<SearchInput>>,
    preview_input: Option<Entity<MultilineInput>>,
    preview_output: Option<Entity<MultilineInput>>,
    preview_text: String,
    preview_result: Option<Value>,
    preview_error: Option<String>,
    preview_pending: bool,
    preview_generation: u64,
    manager_subscriptions: Vec<Subscription>,
    manager_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    create_trigger_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    manager_focus: FocusHandle,
    popup_focus: FocusHandle,
    hovered: HashSet<String>,
    tweens: crate::motion::Tweens<String>,
}

impl TextActionsPage {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut page = Self {
            host,
            tokens,
            dark,
            embedded: false,
            actions: Vec::new(),
            types: Vec::new(),
            internal_types: Vec::new(),
            regex_options: Vec::new(),
            group: "all".into(),
            loaded: false,
            loading: false,
            load_generation: 0,
            next_session: 0,
            editor: None,
            routes: EditorRoutes::default(),
            delete_target: None,
            deleting: false,
            message: None,
            singles: HashMap::new(),
            multiples: HashMap::new(),
            subscriptions: Vec::new(),
            query: String::new(),
            scope: "all".into(),
            selected_id: None,
            detail_open: false,
            advanced: false,
            create_open: false,
            create_advanced: false,
            create_selected: 0,
            common_pending: None,
            search_input: None,
            preview_input: None,
            preview_output: None,
            preview_text: String::new(),
            preview_result: None,
            preview_error: None,
            preview_pending: false,
            preview_generation: 0,
            manager_subscriptions: Vec::new(),
            manager_bounds: Arc::new(Mutex::new(None)),
            create_trigger_bounds: Arc::new(Mutex::new(None)),
            manager_focus: cx.focus_handle(),
            popup_focus: cx.focus_handle(),
            hovered: HashSet::new(),
            tweens: crate::motion::Tweens::default(),
        };
        management::initialize(&mut page, window, cx);
        page.refresh(window, cx);
        page
    }

    /// Let a continuous settings page own vertical scrolling. Switching this
    /// presentation mode keeps the current editor, controls and save session.
    pub fn set_embedded(&mut self, embedded: bool, cx: &mut Context<Self>) {
        if self.embedded == embedded {
            return;
        }
        self.embedded = embedded;
        cx.notify();
    }

    pub fn set_route(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.routes.accept(route_request(route)) {
            self.consume_route(window, cx);
        }
    }

    /// Settings can leave an ordinary unsaved draft without implicitly saving
    /// it. An accepted Save/Delete or provisional IME input must finish first.
    pub fn has_active_save_or_composition(&self, cx: &App) -> bool {
        self.deleting
            || self.common_pending.is_some()
            || self.editor.as_ref().is_some_and(|editor| editor.saving)
            || self.composing(cx)
    }

    fn composing(&self, cx: &App) -> bool {
        self.search_input
            .as_ref()
            .is_some_and(|input| input.read(cx).is_composing())
            || self
                .preview_input
                .as_ref()
                .is_some_and(|input| input.read(cx).is_composing())
            || self
                .singles
                .values()
                .any(|input| input.read(cx).is_composing())
            || self
                .multiples
                .values()
                .any(|input| input.read(cx).is_composing())
    }

    /// Refresh presentation colors without replacing controls or their IME state.
    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark == dark {
            return;
        }
        self.refresh_control_theme(dark, cx);
    }

    /// Apply custom colors even when the light/dark mode has not changed.
    /// Existing controls retain their draft, selection and IME composition.
    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.refresh_control_theme(dark, cx);
    }

    fn refresh_control_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        let colors = self.input_colors();
        for input in self.singles.values() {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        for input in self.multiples.values() {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        if let Some(input) = &self.search_input {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        for input in [&self.preview_input, &self.preview_output]
            .into_iter()
            .flatten()
        {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        cx.notify();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let generation = self.load_generation;
        let host = self.host.clone();
        self.loading = true;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.text_action_list() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.actions = result["actions"].as_array().cloned().unwrap_or_default();
                this.types = result["types"].as_array().cloned().unwrap_or_default();
                this.internal_types = result["internalTypes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                this.regex_options = result["regexOptions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                this.loaded = true;
                this.loading = false;
                if this.editor.is_none() {
                    this.invalidate_preview();
                }
                this.consume_route(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn consume_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loaded || self.loading {
            return;
        }
        let Some(request) = self.routes.consume() else {
            return;
        };
        match request {
            RouteRequest::Create => self.open_editor(None, window, cx),
            RouteRequest::Edit(id) => {
                let target = self
                    .actions
                    .iter()
                    .find(|action| action["id"] == id)
                    .cloned();
                if let Some(target) = target {
                    self.open_editor(Some(target), window, cx);
                } else {
                    self.message = Some((
                        true,
                        tr("未找到该文本操作", "Text action not found.").into(),
                    ));
                }
            }
        }
    }

    fn input_colors(&self) -> InputColors {
        let t = self.tokens.theme(self.dark);
        InputColors {
            background: color(&t.search_background),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: color(&t.border_secondary),
            selection: color(&t.tab_active),
            caret: color(&t.accent),
        }
    }

    fn open_editor(&mut self, action: Option<Value>, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() || self.composing(cx) || self.common_pending.is_some() {
            self.message = Some((
                true,
                tr(
                    "请先保存或取消当前编辑。",
                    "Save or cancel the current edit first.",
                )
                .into(),
            ));
            cx.notify();
            return;
        }
        if action
            .as_ref()
            .is_some_and(|action| text(action, "ruleToken").is_empty())
        {
            self.message = Some((
                true,
                tr(
                    "缺少有效的文本操作版本，请刷新列表。",
                    "A valid text-action version is required. Refresh the list.",
                )
                .into(),
            ));
            cx.notify();
            return;
        }
        self.next_session += 1;
        self.selected_id = action
            .as_ref()
            .and_then(|action| action["id"].as_str())
            .map(str::to_string);
        self.detail_open = true;
        self.advanced = false;
        self.create_open = false;
        self.invalidate_preview();
        let group = if matches!(self.group.as_str(), "all" | "common" | "ungrouped") {
            ""
        } else {
            self.group.strip_prefix("name:").unwrap_or(&self.group)
        };
        let draft = editor_draft(action.as_ref(), group);
        let kind = text(&draft, "type");
        let cache = HashMap::from([(kind, draft["data"].clone())]);
        let last_valid_delay = valid_delay(&draft["simulateDelay"]).unwrap_or(0);
        self.editor = Some(Editor {
            session: self.next_session,
            revision: 0,
            draft,
            cache,
            last_valid_delay,
            saving: false,
            error: None,
        });
        self.delete_target = None;
        self.message = None;
        self.rebuild_inputs(window, cx);
        if let Some(title) = self.singles.get("title") {
            window.focus(&title.read(cx).focus_handle());
        }
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        if self.has_active_save_or_composition(cx) {
            return;
        }
        self.editor = None;
        self.advanced = false;
        self.invalidate_preview();
        self.routes.retire();
        self.singles.clear();
        self.multiples.clear();
        self.subscriptions.clear();
        cx.notify();
    }

    fn change(&mut self, key: &str, value: Value, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if key == "simulateDelay" {
            if let Some(delay) = valid_delay(&value) {
                editor.last_valid_delay = delay;
            }
        }
        if let Some(key) = key.strip_prefix("data:") {
            editor.draft["data"][key] = value;
        } else {
            editor.draft[key] = value;
        }
        editor.revision += 1;
        editor.error = None;
        self.invalidate_preview();
        cx.notify();
    }

    fn rebuild_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.singles.clear();
        self.multiples.clear();
        self.subscriptions.clear();
        let Some(editor) = &self.editor else {
            return;
        };
        let draft = editor.draft.clone();
        let kind = text(&draft, "type");
        self.single("title", text(&draft, "title"), window, cx);
        self.single("group", text(&draft, "group"), window, cx);
        self.multiple("description", text(&draft, "description"), 64., window, cx);
        self.single(
            "simulateDelay",
            draft["simulateDelay"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| draft["simulateDelay"].as_u64().unwrap_or(0).to_string()),
            window,
            cx,
        );
        match kind.as_str() {
            "SingleReplace" => {
                self.multiple(
                    "data:Pattern",
                    text(&draft["data"], "Pattern"),
                    92.,
                    window,
                    cx,
                );
                self.multiple(
                    "data:ReplaceMent",
                    text(&draft["data"], "ReplaceMent"),
                    92.,
                    window,
                    cx,
                );
            }
            "ZExpression" => self.multiple(
                "data:Expresstion",
                text(&draft["data"], "Expresstion"),
                130.,
                window,
                cx,
            ),
            "QuickerAction" => self.single("data:Id", text(&draft["data"], "Id"), window, cx),
            "Pair" => {
                self.multiple(
                    "data:Prefix",
                    text(&draft["data"], "Prefix"),
                    64.,
                    window,
                    cx,
                );
                self.multiple(
                    "data:Suffix",
                    text(&draft["data"], "Suffix"),
                    64.,
                    window,
                    cx,
                );
            }
            "Send" => self.multiple("data:Text", text(&draft["data"], "Text"), 156., window, cx),
            _ => {}
        }
    }

    fn single(&mut self, key: &str, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let colors = self.input_colors();
        let input = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(30., 13., 8., cx);
            input.set_placeholder("", cx);
            input.set_double_click_clears(false);
            input.set_text(value, window, cx);
            input
        });
        let field = key.to_string();
        self.subscriptions.push(
            cx.subscribe_in(&input, window, move |this, _, event, _, cx| {
                if let SearchEvent::Changed(value) = event {
                    this.change(&field, json!(value), cx);
                }
            }),
        );
        self.singles.insert(key.into(), input);
    }

    fn multiple(
        &mut self,
        key: &str,
        value: String,
        height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let colors = self.input_colors();
        let input = cx.new(|cx| {
            let mut input = MultilineInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(height, 13., 8., cx);
            input.set_text(value, window, cx);
            input
        });
        let field = key.to_string();
        self.subscriptions.push(cx.subscribe_in(
            &input,
            window,
            move |this, _, event, window, cx| match event {
                MultilineEvent::Changed(value) => this.change(&field, json!(value), cx),
                MultilineEvent::Save => this.save(window, cx),
                MultilineEvent::Escape
                | MultilineEvent::Submit
                | MultilineEvent::PasteRequested(_) => {}
            },
        ));
        self.multiples.insert(key.into(), input);
    }

    fn change_type(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        let previous = text(&editor.draft, "type");
        if previous == kind {
            return;
        }
        editor.cache.insert(previous, editor.draft["data"].clone());
        editor.draft["type"] = json!(kind);
        editor.draft["data"] = editor
            .cache
            .get(kind)
            .cloned()
            .unwrap_or_else(|| default_data(kind));
        editor.revision += 1;
        self.invalidate_preview();
        self.rebuild_inputs(window, cx);
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Do not save provisional IME text or convert an unfinished number.
        if self.composing(cx) {
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.saving {
            return;
        }
        let title = text(&editor.draft, "title");
        if title.trim().is_empty() {
            editor.error = Some(tr("请填写标题", "Enter a title.").into());
            cx.notify();
            return;
        }
        let control = self
            .singles
            .get("simulateDelay")
            .map(|input| input.read(cx).text())
            .unwrap_or("0");
        let delay =
            match delay_for_save(&editor.draft, &self.types, editor.last_valid_delay, control) {
                Ok(delay) => delay,
                Err(error) => {
                    editor.error = Some(error);
                    cx.notify();
                    return;
                }
            };
        let mut payload = editor.draft.clone();
        payload["title"] = json!(title.trim());
        payload["group"] = json!(text(&payload, "group").trim());
        payload["description"] = json!(text(&payload, "description").trim());
        payload["simulateDelay"] = json!(delay);
        let kind = text(&payload, "type");
        payload["data"]["ObjType"] = default_data(&kind)["ObjType"].clone();
        let session = editor.session;
        let revision = editor.revision;
        editor.saving = true;
        editor.error = None;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.text_action_upsert(&payload) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let matching = this
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.session == session);
                match result {
                    Ok(saved) => {
                        if matching {
                            this.selected_id = saved["id"].as_str().map(str::to_string);
                        }
                        let composing = this.composing(cx);
                        match accept_saved(
                            this.editor.as_mut(),
                            session,
                            revision,
                            &saved,
                            composing,
                        ) {
                            SaveReceipt::Close => {
                                this.close_editor(cx);
                                this.message = Some((false, tr("已保存", "Saved.").into()));
                            }
                            SaveReceipt::KeepDraft => {
                                this.editor.as_mut().unwrap().error = Some(
                                    tr(
                                        "已保存提交时的内容；新的修改尚未保存。",
                                        "Submitted content saved; newer changes are still unsaved.",
                                    )
                                    .into(),
                                );
                            }
                            SaveReceipt::Retired => {}
                        }
                        this.refresh(window, cx);
                    }
                    Err(error) if matching => {
                        let editor = this.editor.as_mut().unwrap();
                        editor.saving = false;
                        editor.error = Some(error);
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.deleting {
            return;
        }
        let Some(target) = self.delete_target.clone() else {
            return;
        };
        self.deleting = true;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.text_action_delete(
                        &json!({"id": target["id"], "expectedRuleToken":target["ruleToken"]}),
                    )
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.deleting = false;
                match result {
                    Ok(_) => {
                        this.delete_target = None;
                        this.message = Some((false, tr("已删除", "Deleted.").into()));
                        this.refresh(window, cx);
                    }
                    Err(error) => this.message = Some((true, error)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        enabled: bool,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let hover = color(&t.hover);
        let id: SharedString = id.into();
        let label: SharedString = label.into();
        rsx! {
            <div id={id} px={px(10.)} py={px(5.)} rounded={px(5.)} border_1
                border_color={color(&t.border_secondary)} text_size={px(12.)}>
                {label}
            </div>
        }
        .when(enabled, |button| {
            button.cursor_pointer().hover(move |style| style.bg(hover))
        })
        .when(!enabled, |button| button.opacity(0.4))
    }

    fn field(&self, label: &'static str, key: &str) -> AnyElement {
        let field = rsx! {
            <div flex flex_col gap={px(5.)}>
                {label}
            </div>
        };
        if let Some(input) = self.singles.get(key) {
            field.child(input.clone()).into_any_element()
        } else if let Some(input) = self.multiples.get(key) {
            field.child(input.clone()).into_any_element()
        } else {
            field.into_any_element()
        }
    }

    fn check(
        &self,
        id: &str,
        label: &'static str,
        key: &str,
        checked: bool,
        t: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = key.to_string();
        self.button(
            id.to_string(),
            format!("{} {label}", if checked { "☑" } else { "☐" }),
            true,
            t,
        )
        .on_click(cx.listener(move |this, _, _, cx| this.change(&key, json!(!checked), cx)))
        .into_any_element()
    }

    fn render_editor(&self, t: &ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.editor.as_ref().unwrap();
        let draft = &editor.draft;
        let kind = text(draft, "type");
        let mut body = rsx! {
            <div id={"text-action-editor-body"} flex flex_col gap={px(10.)} px={px(20.)} py={px(17.)} />
        }.flex_1().min_h_0().min_w_0().overflow_y_scroll()
            .smooth_scroll(("text-action-editor-body", editor.session), cx);
        if let Some(error) = &editor.error {
            body = body.child(div().text_color(color(&t.error)).child(error.clone()));
        }
        body = body
            .child(self.field(tr("名称", "Name"), "title"))
            .child(self.field(tr("分组", "Group"), "group"))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(color(&t.text_muted))
                    .child(management::kind_label(&kind)),
            );
        match kind.as_str() {
            "Internal" => {
                body = body.child(div().flex().flex_wrap().gap(px(5.)).children(
                    self.internal_types.iter().map(|option| {
                        let id = text(option, "id");
                        let selected = draft["data"]["Type"] == id;
                        self.button(format!("internal:{id}"), text(option, "label"), true, t)
                            .when(selected, |button| button.bg(color(&t.tab_active)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change("data:Type", json!(id), cx)
                            }))
                    }),
                ));
            }
            "SingleReplace" => {
                body = body
                    .child(self.field(tr("查找内容", "Find"), "data:Pattern"))
                    .child(self.field(tr("替换为", "Replace with"), "data:ReplaceMent"));
                for (key, label) in [
                    (
                        "EscapePattern",
                        tr(
                            "转义查找内容（\\t \\r \\n）",
                            "Unescape find text (\\t \\r \\n)",
                        ),
                    ),
                    (
                        "EscapeReplaceMent",
                        tr(
                            "转义替换为（\\t \\r \\n）",
                            "Unescape replacement (\\t \\r \\n)",
                        ),
                    ),
                    ("UseRegex", tr("使用正则替换", "Use regular expressions")),
                ] {
                    if key != "UseRegex" && !self.advanced {
                        continue;
                    }
                    body = body.child(self.check(
                        key,
                        label,
                        &format!("data:{key}"),
                        draft["data"][key] == true,
                        t,
                        cx,
                    ));
                }
                if draft["data"]["UseRegex"] == true && self.advanced {
                    let flags = draft["data"]["Options"].as_u64().unwrap_or(0);
                    body = body.child(div().flex().flex_wrap().gap(px(5.)).children(
                        self.regex_options.iter().map(|option| {
                            let id = text(option, "id");
                            let bit = regex_bit(&id);
                            let checked = flags & bit != 0;
                            self.button(
                                format!("regex:{id}"),
                                format!(
                                    "{} {}",
                                    if checked { "☑" } else { "☐" },
                                    text(option, "label")
                                ),
                                true,
                                t,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.change(
                                        "data:Options",
                                        json!(if checked { flags & !bit } else { flags | bit }),
                                        cx,
                                    )
                                },
                            ))
                        }),
                    ));
                }
            }
            "ZExpression" => {
                body = body
                    .child(self.field(tr("表达式", "Expression"), "data:Expresstion"))
                    .child(div().text_color(color(&t.text_muted)).child(tr(
                        "可用变量：{text}、{list}",
                        "Available variables: {text}, {list}",
                    )))
            }
            "QuickerAction" => body = body.child(self.field(tr("动作 ID", "Action ID"), "data:Id")),
            "Pair" => {
                body = body
                    .child(self.field(tr("前缀", "Prefix"), "data:Prefix"))
                    .child(self.field(tr("后缀", "Suffix"), "data:Suffix"))
                    .child(self.check(
                        "pair-lines",
                        tr("多行模式", "Multiline mode"),
                        "data:MultiLines",
                        draft["data"]["MultiLines"] == true,
                        t,
                        cx,
                    ))
                    .child(self.check(
                        "pair-trim",
                        tr("去除前后空白符", "Trim whitespace"),
                        "data:Trim",
                        draft["data"]["Trim"] == true,
                        t,
                        cx,
                    ));
            }
            "Send" => body = body.child(self.field(tr("文本内容", "Text"), "data:Text")),
            _ => {}
        }
        body = body.child(
            self.button(
                "action-advanced-options",
                if self.advanced {
                    tr(
                        "⌄ 收起补充说明与输出选项",
                        "⌄ Hide description and output options",
                    )
                } else {
                    tr("› 补充说明与输出选项", "› Description and output options")
                },
                true,
                t,
            )
            .border_0()
            .text_size(px(11.))
            .text_color(color(&t.text_muted))
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.composing(cx) {
                    this.advanced = !this.advanced;
                    cx.notify();
                }
            })),
        );
        if self.advanced {
            body = body
                .child(self.field(tr("说明", "Description"), "description"))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(color(&t.text_muted))
                        .child(tr("操作类型（高级）", "Action type (advanced)")),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(5.))
                        .children(self.types.iter().map(|option| {
                            let id = text(option, "id");
                            self.button(
                                format!("text-action-kind:{id}"),
                                management::kind_label(&id),
                                true,
                                t,
                            )
                            .when(kind == id, |button| button.bg(color(&t.tab_active)))
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.change_type(&id, window, cx),
                            ))
                        })),
                );
        }
        let needs_input = action_needs_input(&self.types, &kind);
        if needs_input && self.advanced {
            body = body.child(self.check(
                "simulate",
                tr(
                    "模拟输入（不写入剪贴板）",
                    "Simulate input (without writing to the clipboard)",
                ),
                "simulate",
                draft["simulate"] == true,
                t,
                cx,
            ));
            if draft["simulate"] == true {
                body = body.child(self.field(
                    tr("输入延迟（毫秒）", "Input delay (milliseconds)"),
                    "simulateDelay",
                ));
            }
        }
        body = body.child(self.render_preview(t, cx));
        rsx! {
            <div flex flex_col size_full min_h_0 min_w_0 />
        }
        .child(
            div()
                .px(px(14.))
                .py(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(if draft["id"].is_null() {
                    tr("添加文本操作", "Add text action")
                } else {
                    tr("编辑文本操作", "Edit text action")
                }),
        )
        .child(body)
        .child(
            rsx! {
                <div flex justify_end gap={px(8.)} p={px(12.)} border_t_1
                    border_color={color(&t.border)} />
            }
            .child(
                self.button("cancel-action", tr("取消", "Cancel"), !editor.saving, t)
                    .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))),
            )
            .child(
                self.button(
                    "save-action",
                    if editor.saving {
                        tr("保存中…", "Saving…")
                    } else {
                        tr("保存", "Save")
                    },
                    !editor.saving,
                    t,
                )
                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            ),
        )
        .into_any_element()
    }
}

impl Render for TextActionsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        management::render(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_preserve_parameter_names_and_unknown_data() {
        let action = json!({"id": "3", "type": "ZExpression", "title": "表达式", "data": {"Expresstion": "$={text}", "future": [1, 2]}});
        let draft = editor_draft(Some(&action), "ignored");
        assert_eq!(draft["data"]["ObjType"], 4);
        assert_eq!(draft["data"]["future"], json!([1, 2]));
        assert_eq!(draft["id"], "3");
    }

    #[test]
    fn editor_snapshot_guard_is_carried_without_inventing_a_guard_for_new_rules() {
        let action =
            json!({"id":"3","type":"SingleReplace","ruleToken":"current-rule","common":true});
        let draft = editor_draft(Some(&action), "ignored");
        assert_eq!(draft["expectedRuleToken"], "current-rule");
        assert!(editor_draft(None, "").get("expectedRuleToken").is_none());
        let mut editor = Editor {
            session: 1,
            revision: 2,
            draft,
            cache: HashMap::new(),
            last_valid_delay: 0,
            saving: true,
            error: None,
        };
        assert_eq!(
            accept_saved(
                Some(&mut editor),
                1,
                1,
                &json!({"id":"3","ruleToken":"saved-rule"}),
                false
            ),
            SaveReceipt::KeepDraft
        );
        assert_eq!(editor.draft["expectedRuleToken"], "saved-rule");
    }

    #[test]
    fn route_decodes_explicit_editor_request_and_prioritizes_id() {
        assert_eq!(
            route_request("/main/settings/textAction?create=1&actionId=a%20b"),
            Some(RouteRequest::Edit("a b".into()))
        );
        assert_eq!(
            route_request("/main/settings/textAction?create=1&other=x"),
            Some(RouteRequest::Create)
        );
        assert_eq!(route_request("/main/settings/textAction"), None);
    }

    #[test]
    fn defaults_and_regex_bits_match_webview_payload() {
        for (kind, object_type) in [
            ("Internal", 1),
            ("SingleReplace", 2),
            ("ZExpression", 4),
            ("QuickerAction", 5),
            ("Pair", 6),
            ("Send", 7),
        ] {
            assert_eq!(default_data(kind)["ObjType"], object_type);
        }
        assert_eq!(regex_bit("RightToLeft"), 64);
        assert_eq!(default_data("SingleReplace")["EscapeReplaceMent"], true);
    }

    #[test]
    fn save_reply_cannot_close_or_overwrite_a_newer_session() {
        let mut editor = Editor {
            session: 2,
            revision: 0,
            draft: editor_draft(None, ""),
            cache: HashMap::new(),
            last_valid_delay: 0,
            saving: false,
            error: None,
        };
        assert_eq!(
            accept_saved(Some(&mut editor), 1, 0, &json!({"id": "7"}), false),
            SaveReceipt::Retired
        );
        assert!(editor.draft["id"].is_null());
        assert!(!editor.saving);
    }

    #[test]
    fn continued_create_preserves_draft_and_remembers_returned_id() {
        let mut editor = Editor {
            session: 1,
            revision: 2,
            draft: editor_draft(None, ""),
            cache: HashMap::new(),
            last_valid_delay: 0,
            saving: true,
            error: None,
        };
        editor.draft["title"] = json!("正在继续输入");
        assert_eq!(
            accept_saved(
                Some(&mut editor),
                1,
                1,
                &json!({"id": "7", "title": "旧标题"}),
                false,
            ),
            SaveReceipt::KeepDraft
        );
        assert_eq!(editor.draft["title"], "正在继续输入");
        assert_eq!(editor.draft["id"], "7");
        assert!(!editor.saving);
        assert_eq!(
            accept_saved(Some(&mut editor), 1, 2, &json!({"id": "7"}), false),
            SaveReceipt::Close
        );
    }

    #[test]
    fn same_external_editor_request_can_reopen_after_cancel_or_save_without_refresh_replay() {
        for request in [RouteRequest::Edit("a 中😀".into()), RouteRequest::Create] {
            let mut routes = EditorRoutes::default();
            assert!(routes.accept(Some(request.clone())));
            assert_eq!(routes.consume(), Some(request.clone()));
            // Background list/theme refresh has no new explicit navigation.
            assert!(routes.consume().is_none());
            assert!(!routes.accept(Some(request.clone())));
            // Both Cancel and a completed Save use the same close path.
            routes.retire();
            assert!(routes.consume().is_none());
            assert!(routes.accept(Some(request.clone())));
            assert_eq!(routes.consume(), Some(request));
        }
    }

    #[test]
    fn hidden_invalid_delay_keeps_last_valid_configuration_and_visible_delay_is_checked() {
        let types = vec![
            json!({"id":"SingleReplace","needInput":true}),
            json!({"id":"Send","needInput":false}),
        ];
        let mut draft = editor_draft(None, "");
        draft["simulate"] = json!(true);
        draft["simulateDelay"] = json!("unfinished");
        assert!(delay_for_save(&draft, &types, 25, "unfinished").is_err());
        assert_eq!(delay_for_save(&draft, &types, 25, " 83 ").unwrap(), 83);
        draft["simulate"] = json!(false);
        assert_eq!(
            delay_for_save(&draft, &types, 25, "unfinished").unwrap(),
            25
        );
        draft["type"] = json!("Send");
        draft["simulate"] = json!(true);
        assert_eq!(delay_for_save(&draft, &types, 25, "").unwrap(), 25);
        for value in [
            json!(""),
            json!("x"),
            json!("-1"),
            json!("1.5"),
            json!("101"),
        ] {
            assert!(valid_delay(&value).is_none());
        }
        assert_eq!(valid_delay(&json!(0)), Some(0));
        assert_eq!(valid_delay(&json!("100")), Some(100));
    }

    #[test]
    fn save_reply_retains_provisional_ime_even_before_a_new_revision_is_emitted() {
        let mut editor = Editor {
            session: 4,
            revision: 3,
            draft: editor_draft(None, ""),
            cache: HashMap::new(),
            last_valid_delay: 25,
            saving: true,
            error: None,
        };
        editor.draft["title"] = json!("原草稿");
        assert_eq!(
            accept_saved(Some(&mut editor), 4, 3, &json!({"id":"saved-id"}), true),
            SaveReceipt::KeepDraft
        );
        assert_eq!(editor.draft["title"], "原草稿");
        assert_eq!(editor.draft["id"], "saved-id");
        assert!(!editor.saving);
    }
}
