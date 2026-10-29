//! Clipboard sync has one shared switch and optional text/image content.
//! Paired devices show status rather than another set of policy controls.
use super::*;

fn content_enabled(status: &Value, field: &str) -> bool {
    status[field].as_bool().unwrap_or(true)
}

fn content_can_toggle(status: &Value, field: &str) -> bool {
    !content_enabled(status, field)
        || content_enabled(
            status,
            if field == "textEnabled" {
                "imageEnabled"
            } else {
                "textEnabled"
            },
        )
}

fn sync_error_summary(error: &str) -> (&'static str, bool) {
    if error.contains("Receiving this clipboard type")
        || error.contains("Both device receive actions are disabled")
    {
        (
            tr("对方未开启接收", "Receiving is off on the other device"),
            false,
        )
    } else if error.contains("Device clipboard synchronization is disabled") {
        (
            tr("对方已暂停同步", "Sync is paused on the other device"),
            false,
        )
    } else if error.contains("LAN sharing is disabled") {
        (tr("等待对方连接", "Waiting for the other device"), false)
    } else if error.contains("Device is not paired") {
        (tr("需要重新配对", "Pair again to sync"), true)
    } else {
        (tr("暂时无法同步", "Sync unavailable"), true)
    }
}

fn setting_row(
    title: &'static str,
    detail: Option<SharedString>,
    control: AnyElement,
    t: &ThemeTokens,
) -> gpui::Div {
    let label = rsx! {
        <div flex_1 min_w_0 flex flex_col gap={px(2.)}>
            <div text_size={px(12.)} line_height={px(18.)} text_color={color(&t.text)}>{title}</div>
        </div>
    }.children(detail.map(|detail| rsx! {
        <div text_size={px(11.)} line_height={px(16.)} text_color={color(&t.text_muted)}>{detail}</div>
    }));
    rsx! {
        <div w_full min_w_0 flex_none min_h={px(28.)} flex items_center gap={px(14.)}>
            {label}{control}
        </div>
    }
}

impl LanManagementPage {
    pub(super) fn sync_toggle_value(&self, id: &str) -> Option<bool> {
        Some(match id.strip_prefix("overlay:sync:")? {
            "enabled" => self.device_sync["enabled"] == true,
            "receivedFilesToHistory" => self.status["receivedFilesToHistory"] == true,
            _ => return None,
        })
    }

    fn sync_switch(
        &mut self,
        id: String,
        on: bool,
        ready: bool,
        action: Action,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.control(
            id,
            "",
            ready && !self.busy,
            t,
            window,
            cx,
            move |this, window, cx| this.run(action.clone(), window, cx),
        )
        .w(px(29.))
        .h(px(16.))
        .min_h(px(16.))
        .p(px(2.))
        .rounded_full()
        .flex_none()
        .flex()
        .items_center()
        .when(on, |view| view.justify_end())
        .tooltip(tooltip(
            if on {
                tr("已开启", "On")
            } else {
                tr("已关闭", "Off")
            }
            .into(),
            self.tokens.clone(),
            self.dark,
        ))
        .child(div().size(px(12.)).rounded_full().bg(color(&t.surface)))
        .into_any_element()
    }

    fn sync_content_choice(
        &mut self,
        field: &'static str,
        label: &'static str,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on = content_enabled(&self.device_sync, field);
        let allowed = content_can_toggle(&self.device_sync, field);
        let mut update = json!({});
        update[field] = json!(!on);
        let check = rsx! {
            <div size={px(14.)} flex_none rounded={px(3.)} border_1
                border_color={color(if on { &t.accent } else { &t.border })}
                bg={if on { color(&t.accent) } else { gpui::transparent_black() }}
                flex items_center justify_center />
        }
        .children(on.then(|| self.icon_with_color("check", 10., color(&t.surface))));
        self.control(
            format!("overlay:sync-content:{field}"),
            "",
            self.device_sync.is_object() && allowed && !self.busy,
            t,
            window,
            cx,
            move |this, window, cx| this.run(Action::DeviceSync(update.clone()), window, cx),
        )
        .flex_none()
        .min_h(px(28.))
        .px(px(3.))
        .py(px(0.))
        .border_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .tooltip(tooltip(
            if allowed {
                label
            } else {
                tr("至少保留一种同步内容", "Keep at least one content type")
            }
            .into(),
            self.tokens.clone(),
            self.dark,
        ))
        .child(check)
        .child(div().whitespace_nowrap().text_size(px(12.)).child(label))
        .into_any_element()
    }

    pub(super) fn sync_global_card(
        &mut self,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let on = self.device_sync["enabled"] == true;
        let control = self.sync_switch(
            "overlay:sync:enabled".into(),
            on,
            self.device_sync.is_object(),
            Action::DeviceSync(json!({"enabled": !on})),
            t,
            window,
            cx,
        );
        let text_choice =
            self.sync_content_choice("textEnabled", tr("文本", "Text"), t, window, cx);
        let image_choice =
            self.sync_content_choice("imageEnabled", tr("图片", "Images"), t, window, cx);
        let detail = if !self.device_sync.is_object() {
            tr("正在读取…", "Loading…")
        } else {
            tr(
                "与已配对设备自动同步",
                "Sync automatically with paired devices",
            )
        };
        let mut settings = rsx! {
            <div w_full min_w_0 flex_none p={px(12.)} flex flex_col gap={px(8.)}>
                {setting_row(tr("自动同步", "Automatic sync"), Some(detail.into()), control, t)}
                <div w_full min_w_0 flex_none flex items_center gap={px(16.)}>{text_choice}{image_choice}</div>
            </div>
        };
        if let Some(error) = self.device_sync["error"]
            .as_str()
            .filter(|error| !error.is_empty())
        {
            settings = settings.child(
                div()
                    .id("overlay:sync:error")
                    .w_full()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(color(&t.error))
                    .child(tr("同步暂不可用", "Sync unavailable"))
                    .tooltip(tooltip(
                        error.to_owned().into(),
                        self.tokens.clone(),
                        self.dark,
                    )),
            );
        }
        section(
            heading(tr("剪贴板同步", "Clipboard sync"), None, t),
            card(t).w_full().min_w_0().child(settings),
        )
    }

    pub(super) fn received_files_card(
        &mut self,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let on = self.status["receivedFilesToHistory"] == true;
        let control = self.sync_switch(
            "overlay:sync:receivedFilesToHistory".into(),
            on,
            self.status.is_object(),
            Action::Config(json!({"receivedFilesToHistory": !on})),
            t,
            window,
            cx,
        );
        section(heading(tr("收到的文件", "Received files"), None, t), card(t).w_full().min_w_0().child(
            rsx! { <div w_full min_w_0 flex_none p={px(12.)}>
                {setting_row(tr("加入剪贴板列表", "Add to clipboard history"),
                    Some(tr("手动互传的文件可在列表直接打开", "Open received files directly from history").into()), control, t)}
            </div> }))
    }

    pub(super) fn sync_peer_status(&self, device_id: &str, t: &ThemeTokens) -> AnyElement {
        let peer = self.device_sync["peers"]
            .as_array()
            .and_then(|peers| peers.iter().find(|peer| peer["deviceId"] == device_id));
        let last_error = peer
            .and_then(|peer| peer["lastError"].as_str())
            .filter(|error| !error.is_empty());
        let (summary, failed) = if !self.device_sync.is_object() {
            (tr("正在读取…", "Loading…"), false)
        } else if self.device_sync["enabled"] != true {
            (tr("已暂停", "Paused"), false)
        } else if self.status["running"] != true {
            (tr("等待连接", "Waiting for connection"), false)
        } else if let Some(error) = last_error {
            sync_error_summary(error)
        } else if peer.is_some_and(|peer| peer["online"] == true) {
            (tr("已开启", "On"), false)
        } else {
            (tr("等待设备上线", "Waiting for device"), false)
        };
        let row = rsx! {
            <div w_full min_w_0 flex_none min_h={px(32.)} px={px(12.)}
                border_t_1 border_color={color(&t.border)} flex items_center gap={px(7.)}>
                {self.icon("clipboard", 14., t)}
                <div flex_none whitespace_nowrap text_size={px(11.)}>{tr("剪贴板同步", "Clipboard sync")}</div>
                <div flex_1 min_w_0 text_ellipsis text_size={px(10.)}
                    text_color={color(if failed { &t.error } else { &t.text_muted })}>{summary}</div>
            </div>
        }.id(SharedString::from(format!("overlay:sync-status:{device_id}")));
        if let Some(error) = last_error {
            row.tooltip(tooltip(
                error.to_owned().into(),
                self.tokens.clone(),
                self.dark,
            ))
            .into_any_element()
        } else {
            row.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_choices_keep_the_last_type_and_preserve_explicit_choices() {
        let defaults = json!({"enabled": true});
        assert!(content_enabled(&defaults, "textEnabled"));
        assert!(content_enabled(&defaults, "imageEnabled"));
        let text_only = json!({"textEnabled":true,"imageEnabled":false});
        assert!(!content_can_toggle(&text_only, "textEnabled"));
        assert!(content_can_toggle(&text_only, "imageEnabled"));
        assert!(!content_enabled(&text_only, "imageEnabled"));
        let image_only = json!({"textEnabled":false,"imageEnabled":true});
        assert!(content_can_toggle(&image_only, "textEnabled"));
        assert!(!content_can_toggle(&image_only, "imageEnabled"));
    }

    #[test]
    fn remote_policy_refusals_are_short_nonfailure_statuses() {
        let refusal = r#"局域网设备返回 409 Internal Server Error: {"status":409,"detail":"Receiving this clipboard type from the device is disabled."}"#;
        assert!(!sync_error_summary(refusal).1);
        assert!(!sync_error_summary("Device clipboard synchronization is disabled.").1);
        assert!(sync_error_summary("Device is not paired.").1);
        assert!(sync_error_summary("Connection timed out").1);
        assert!(!sync_error_summary(refusal).0.contains("409"));
    }
}
