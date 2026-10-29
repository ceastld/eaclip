//! Clipboard sync controls embedded once in the unified Cross-device settings.
//! Pairing and the device list belong to the surrounding LAN settings view.

use super::*;

pub(super) struct DeviceSyncSettingsPage {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    palette: Option<SettingsPalette>,
    status: Value,
    loading: bool,
    saving: bool,
    leave_locked: bool,
    request: u64,
    /// Only a rejected configuration write blocks leaving; status refresh
    /// errors remain presentation-only in the returned status DTO.
    error: Option<String>,
    switch_focus: FocusHandle,
    text_focus: FocusHandle,
    image_focus: FocusHandle,
}

impl DeviceSyncSettingsPage {
    pub(super) fn new(
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
            palette: None,
            status: Value::Null,
            loading: false,
            saving: false,
            leave_locked: false,
            request: 0,
            error: None,
            switch_focus: cx.focus_handle(),
            text_focus: cx.focus_handle(),
            image_focus: cx.focus_handle(),
        };
        page.refresh(window, cx);
        page
    }

    pub(super) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        self.loading = true;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { host.device_sync_status() })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.request != request {
                    return;
                }
                this.loading = false;
                this.status = status;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn on_event(
        &mut self,
        name: &str,
        data: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if name == "deviceSyncChanged"
            && !self.saving
            && data["enabled"].is_boolean()
            && data["peers"].is_array()
        {
            self.request = self.request.wrapping_add(1);
            self.loading = false;
            self.status = data.clone();
            cx.notify();
        } else if matches!(name, "deviceSyncChanged" | "lan.peers.changed") {
            self.refresh(window, cx);
        }
    }

    pub(super) fn pending_writes(&self) -> bool {
        self.saving
    }
    pub(super) fn persistence_error(&self) -> Option<String> {
        self.error.clone()
    }
    pub(super) fn ready(&self) -> bool {
        !self.loading && self.status["enabled"].is_boolean()
    }
    pub(super) fn diagnostics(&self) -> Value {
        json!({"ready":self.ready(),"saving":self.saving,"enabled":self.status["enabled"],
            "textEnabled":self.status["textEnabled"],"imageEnabled":self.status["imageEnabled"],
            "pairedCount":self.status["peers"].as_array().map_or(0, Vec::len),"error":self.error})
    }
    pub(super) fn set_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.leave_locked = locked;
        cx.notify();
    }
    pub(super) fn focus_handles(&self) -> [FocusHandle; 3] {
        [
            self.switch_focus.clone(),
            self.text_focus.clone(),
            self.image_focus.clone(),
        ]
    }
    pub(super) fn set_design_tokens(
        &mut self,
        tokens: DesignTokens,
        dark: bool,
        palette: Option<SettingsPalette>,
        cx: &mut Context<Self>,
    ) {
        self.tokens = tokens;
        self.dark = dark;
        self.palette = palette;
        cx.notify();
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let enabled = self.status["enabled"] != true;
        self.save("enabled", enabled, window, cx);
    }

    fn save(
        &mut self,
        field: &'static str,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving || self.loading || self.leave_locked || !self.status["enabled"].is_boolean()
        {
            return;
        }
        if !value && matches!(field, "textEnabled" | "imageEnabled") {
            let other = if field == "textEnabled" {
                "imageEnabled"
            } else {
                "textEnabled"
            };
            if !self.status[other].as_bool().unwrap_or(true) {
                return;
            }
        }
        self.saving = true;
        self.request = self.request.wrapping_add(1);
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.device_sync_config_update(&json!({(field): value})) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(status) => {
                        this.status = status;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error),
                }
                // A pairing event may have arrived while the write was running.
                this.refresh(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn content_choice(
        &self,
        field: &'static str,
        label: &'static str,
        focus: &FocusHandle,
        t: &ThemeTokens,
        palette: SettingsPalette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = self.status[field].as_bool().unwrap_or(true);
        let other = if field == "textEnabled" {
            "imageEnabled"
        } else {
            "textEnabled"
        };
        let last_selected = selected && !self.status[other].as_bool().unwrap_or(true);
        let ready = !self.saving
            && !self.loading
            && !self.leave_locked
            && self.status["enabled"].is_boolean()
            && !last_selected;
        div()
            .id(SharedString::from(format!("settings-device-sync-{field}")))
            .track_focus(focus)
            .flex_none()
            .min_h(px(28.0))
            .px(px(4.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .text_size(px(12.0))
            .whitespace_nowrap()
            .focus(|style| style.bg(palette.control_hover))
            .child(
                div()
                    .size(px(14.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(if selected {
                        color(&t.accent)
                    } else {
                        palette.control_border
                    })
                    .bg(if selected {
                        color(&t.accent)
                    } else {
                        palette.control
                    })
                    .text_size(px(11.0))
                    .text_color(gpui::white())
                    .children(selected.then_some("✓")),
            )
            .child(label)
            .when(ready, |choice| {
                choice
                    .cursor_pointer()
                    .hover(|style| style.bg(palette.control_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.save(field, !selected, window, cx)
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "space" | "enter") {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.save(field, !selected, window, cx);
                        }
                    }))
            })
            .when(last_selected, |choice| {
                choice.tooltip(settings_help(
                    tr("至少选择一种内容", "Select at least one content type").into(),
                    t.clone(),
                ))
            })
            .into_any_element()
    }
}

impl Render for DeviceSyncSettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark);
        let s = &self.tokens.settings;
        let palette = self.palette.unwrap_or_else(|| SettingsPalette::new(t));
        let enabled = self.status["enabled"] == true;
        let ready = !self.loading
            && !self.saving
            && !self.leave_locked
            && self.status["enabled"].is_boolean();
        let knob = (s.switch_height - 6.0).max(1.0);
        let travel = (s.switch_width - 6.0 - knob).max(0.0);
        let switch = div()
            .id("settings-device-sync-enabled")
            .track_focus(&self.switch_focus)
            .flex_none()
            .w(px(s.switch_width))
            .h(px(s.switch_height))
            .rounded_full()
            .border_1()
            .border_color(if enabled {
                color(&t.accent)
            } else {
                palette.control_border
            })
            .p(px(2.0))
            .flex()
            .bg(color(if enabled { &t.accent } else { &t.tertiary }))
            .focus(|style| style.border_color(palette.focus_border))
            .child(
                div()
                    .relative()
                    .left(px(if enabled { travel } else { 0.0 }))
                    .flex_none()
                    .size(px(knob))
                    .rounded_full()
                    .bg(if enabled {
                        gpui::white()
                    } else {
                        color(&t.text_muted)
                    }),
            )
            .when(!ready, |control| control.opacity(0.5))
            .when(ready, |control| {
                control
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "space" | "enter") {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.toggle(window, cx);
                        }
                    }))
            });
        let toggle_row = widgets::row(false, false, s)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .whitespace_nowrap()
                    .child(tr("剪贴板同步", "Clipboard sync")),
            )
            .child(switch);
        let types_row = widgets::row(false, false, s)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.0))
                    .whitespace_nowrap()
                    .child(tr("同步内容", "Content")),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(16.0))
                    .child(self.content_choice(
                        "textEnabled",
                        tr("文本", "Text"),
                        &self.text_focus,
                        t,
                        palette,
                        cx,
                    ))
                    .child(self.content_choice(
                        "imageEnabled",
                        tr("图片", "Images"),
                        &self.image_focus,
                        t,
                        palette,
                        cx,
                    )),
            );
        let mut content = div()
            .w_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(
                widgets::section("", s, t)
                    .child(widgets::card(
                        [toggle_row.into_any_element(), types_row.into_any_element()],
                        s,
                        palette,
                    ))
                    .child(widgets::note(
                        tr(
                            "配对后自动同步。",
                            "Sync starts automatically after pairing.",
                        ),
                        palette,
                    )),
            );
        if let Some(error) = self
            .error
            .as_deref()
            .or_else(|| self.status["error"].as_str())
        {
            content = content
                .child(widgets::note(error.to_string(), palette).text_color(color(&t.error)));
        }
        content
    }
}
