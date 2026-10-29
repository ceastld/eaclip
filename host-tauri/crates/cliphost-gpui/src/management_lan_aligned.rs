//! Confirmed LAN workbench layout, rendered against the existing transport DTOs.
use gpui_rsx::rsx;
use super::*;
use crate::crisp_svg::svg;
use crate::favorite_drag::ContentDrag;
use crate::scrollbar::{ScrollbarStyle, ScrollbarTarget};
use gpui::ClipboardItem;
use std::rc::Rc;

#[path = "management_device_sync.rs"]
mod device_sync;

const SIDEBAR: f32 = 190.;
const NARROW: f32 = 680.;
const FILE_WIDTH: f32 = 304.;

#[derive(Clone, Copy, PartialEq)]
enum ButtonKind { Secondary, Primary, Flat, Unpair, Compact, FileAction, Icon, Remove, Send, Image, Switch, Conversation, Segment, Copy, Latest, Danger }

fn button_kind(id: &str) -> ButtonKind {
    if id == "lan-send" { ButtonKind::Send }
    else if matches!(id, "lan-sharing-switch" | "overlay:sharing-switch") || id.starts_with("overlay:sync:") { ButtonKind::Switch }
    else if id.starts_with("lan-image:") { ButtonKind::Image }
    else if id.starts_with("overlay:forget-confirm:") { ButtonKind::Danger }
    else if id == "overlay:enable-sharing" || id.starts_with("overlay:confirm-pair:") { ButtonKind::Primary }
    else if id.starts_with("overlay:forget:") { ButtonKind::Unpair }
    else if id.starts_with("overlay:kind:") { ButtonKind::Segment }
    else if id.starts_with("conversation:") || id.starts_with("overlay:conversation:") { ButtonKind::Conversation }
    else if id.starts_with("lan-remove-recent:") || id.starts_with("lan-remove-file:") { ButtonKind::Remove }
    else if matches!(id, "lan-add-recent" | "lan-window-close" | "overlay:close" | "overlay:preview-close") { ButtonKind::Icon }
    else if id.starts_with("lan-copy:") { ButtonKind::Copy }
    else if id.starts_with("lan-open:") || id.starts_with("lan-reveal:") { ButtonKind::FileAction }
    else if id.starts_with("lan-cancel:") || id.starts_with("lan-retry:") || id.starts_with("lan-history-retry:") || id.starts_with("lan-cancel-queued:") || id.starts_with("lan-deliveries:") { ButtonKind::Compact }
    else if id == "lan-latest" { ButtonKind::Latest }
    else if matches!(id, "lan-connections" | "lan-session-switch" | "add-device" | "overlay:add-device" | "overlay:scan" | "overlay:advanced" | "overlay:cancel-pair")
        || id.starts_with("overlay:forget-cancel:") || id.starts_with("overlay:sync-content:") { ButtonKind::Flat }
    else { ButtonKind::Secondary }
}

#[cfg(debug_assertions)]
fn record_composer_layout(
    probe: &Arc<Mutex<Value>>,
    names: &[&str],
    bounds: &[gpui::Bounds<gpui::Pixels>],
) {
    if let Ok(mut layout) = probe.lock() {
        for (name, bounds) in names.iter().zip(bounds) {
            layout[*name] = json!({
                "x":f32::from(bounds.left()), "y":f32::from(bounds.top()),
                "width":f32::from(bounds.size.width), "height":f32::from(bounds.size.height),
                "right":f32::from(bounds.right()), "bottom":f32::from(bounds.bottom())
            });
        }
    }
}

impl LanManagementPage {
    fn lan_scrollbar_style(&self, t: &ThemeTokens, engaged: bool) -> ScrollbarStyle {
        let mut metrics = self.tokens.scrollbar;
        metrics.hit_width = 6.;
        metrics.thumb_width = 4.;
        metrics.thumb_expanded_width = 4.;
        metrics.thumb_inset_right = 1.;
        metrics.thumb_alpha = 0.18;
        metrics.thumb_hover_alpha = 0.32;
        metrics.thumb_drag_alpha = 0.32;
        metrics.track_alpha = 0.;
        ScrollbarStyle { ink: color(&t.text), metrics, always_visible: engaged }
    }
    fn transition(&mut self, id: String, target: bool, duration: Duration, window: &Window) -> f32 {
        let now = Instant::now();
        let value = self.control_tweens.value(
            id,
            if target { 1. } else { 0. },
            duration,
            motion::EASE,
            now,
        );
        if self.control_tweens.any_active(now) {
            window.request_animation_frame();
        }
        value
    }
    fn composer_view(
        &mut self,
        t: &ThemeTokens,
        narrow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let modal = self.devices || self.sessions || self.preview.is_some();
        let read_only = self.exit_pending || modal;
        #[cfg(debug_assertions)]
        let read_only = read_only || self.fixture_keyboard_read_only;
        self.composer.update(cx, |input, cx| {
            input.set_metrics(24., 13., 0., cx);
            input.set_auto_grow(24., 124., cx);
            input.set_placeholder(
                tr(
                    "输入消息…",
                    "Message…",
                ),
                cx,
            );
            input.set_read_only(read_only, cx);
        });
        let entry_width = (f32::from(window.viewport_size().width)
            - if narrow { 0. } else { SIDEBAR }
            - 24. - 10. - 56. - 12.).max(1.);
        self.composer.update(cx, |input, cx| {
            input.auto_grow_height(entry_width, window, cx);
        });
        let mut chips = rsx! {
            <div id={"lan-compose-chips"} w_full flex_none min_h={px(44.)} max_h={px(94.)}
                min_w_0 />
        }
            .overflow_y_scroll()
            .flex()
            .flex_wrap()
            .items_start()
            .gap(px(6.));
        let chip_width = (f32::from(window.viewport_size().width)
            - if narrow { 0. } else { SIDEBAR }
            - if narrow { 48. } else { 60. })
        .max(100.);
        // Draft payload text is used when sending; chips only need metadata.
        let recent_chips = self.recent_items.iter().map(|item|
            (item.id.clone(), item.label.clone(), item.kind.clone(), item.files.clone()))
            .collect::<Vec<_>>();
        for (id, label, kind, files) in recent_chips {
            let remove = self
                .control(
                    format!("lan-remove-recent:{id}"),
                    "",
                    !self.busy,
                    t,
                    window,
                    cx,
                    move |this, _, cx| this.remove_recent(&id, cx),
                )
                .size(px(20.))
                .flex_none()
                .min_h(px(20.))
                .p(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .child(self.icon("close", 12., t));
            let preview = if kind == "image" {
                files
                    .first()
                    .and_then(|path| {
                        self.image(path.clone(), String::new(), ImagePurpose::Chip, cx)
                    })
                    .map(|(image, _, _)| {
                        img(image)
                            .size(px(30.))
                            .object_fit(gpui::ObjectFit::Cover)
                            .rounded(px(4.))
                            .into_any_element()
                    })
                    .unwrap_or_else(|| self.icon("image", 24., t))
            } else {
                self.icon(
                    if kind == "file" {
                        "file"
                    } else {
                        "clipboard"
                    },
                    24.,
                    t,
                )
            };
            chips = chips.child(
                rsx! {
                    <div w={px(300f32.min(chip_width))} h={px(44.)} flex_none max_w_full min_w_0
                        px={px(6.)} py={px(5.)} rounded={px(9.)} border_1
                        border_color={color(&t.border_secondary)} bg={color(&t.surface)} flex
                        items_center gap={px(7.)}>
                        <div size={px(30.)} flex_none overflow_hidden rounded={px(4.)} flex
                            items_center justify_center>
                            {preview}
                        </div>
                    </div>
                }
                    .child(
                        rsx! {
                            <div flex_1 flex flex_col justify_center gap={px(2.)}
                                max_w={px(240.)} min_w_0 />
                        }
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .line_height(px(12.))
                                    .w_full()
                                    .line_clamp(1)
                                    .text_color(color(&t.text_muted))
                                    .child(format!(
                                        "{} · {}",
                                        tr("最近复制", "Recent copy"),
                                        match kind.as_str() {
                                            "image" => tr("图片", "Image"),
                                            "file" => tr("文件", "File"),
                                            _ => tr("文本", "Text"),
                                        }
                                    )),
                            )
                            .child(
                                rsx! {
                                    <div w_full text_size={px(11.)} line_height={px(14.)}
                                        text_ellipsis>
                                        {label}
                                    </div>
                                },
                            ),
                    )
                    .child(remove),
            );
        }
        for path in self.files.clone() {
            if self
                .recent_items
                .iter()
                .any(|item| item.files.contains(&path))
            {
                continue;
            }
            let name = file_name(&path).to_string();
            let remove_path = path.clone();
            let remove = self
                .control(
                    format!("lan-remove-file:{path}"),
                    "",
                    !self.busy,
                    t,
                    window,
                    cx,
                    move |this, _, cx| this.remove_file(&remove_path, cx),
                )
                .size(px(20.))
                .flex_none()
                .min_h(px(20.))
                .p(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .child(self.icon("close", 12., t));
            let preview = if image_path(&path) {
                self.image(path.clone(), String::new(), ImagePurpose::Chip, cx)
                    .map(|(image, _, _)| {
                        img(image)
                            .size(px(30.))
                            .object_fit(gpui::ObjectFit::Cover)
                            .rounded(px(4.))
                            .into_any_element()
                    })
                    .unwrap_or_else(|| self.icon("image", 24., t))
            } else {
                self.icon("file", 24., t)
            };
            chips = chips.child(
                rsx! {
                    <div w={px(240f32.min(chip_width))} h={px(44.)} flex_none max_w_full min_w_0
                        px={px(6.)} py={px(5.)} rounded={px(9.)} border_1
                        border_color={color(&t.border_secondary)} bg={color(&t.surface)} flex
                        items_center gap={px(7.)}>
                        <div size={px(30.)} flex_none overflow_hidden rounded={px(4.)} flex
                            items_center justify_center>
                            {preview}
                        </div>
                        <div flex_1 flex flex_col justify_center max_w={px(180.)} min_w_0>
                            <div w_full text_size={px(11.)} line_height={px(14.)} text_ellipsis>
                                {name}
                            </div>
                        </div>
                        {remove}
                    </div>
                },
            );
        }
        let add = self
            .control(
                "lan-add-recent",
                "",
                !self.busy,
                t,
                window,
                cx,
                |this, window, cx| this.begin_compose(true, window, cx),
            )
            .size(px(28.))
            .flex_none()
            .min_h(px(28.))
            .p(px(5.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .tooltip(tooltip(
                tr("添加最近复制的一条", "Add the most recent copied item").into(),
                self.tokens.clone(),
                self.dark,
            ))
            .child(self.icon("plus", 18., t));
        let send_enabled = self.status["running"] == true
            && self.has_target()
            && !self.busy
            && (!self.composer.read(cx).text().trim().is_empty()
                || !self.files.is_empty()
                || !self.recent_items.is_empty());
        let send = self
            .control(
                "lan-send",
                "",
                send_enabled,
                t,
                window,
                cx,
                |this, window, cx| this.send(window, cx),
            )
            .size(px(28.))
            .flex_none()
            .min_h(px(28.))
            .p(px(0.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .opacity(if send_enabled { 1. } else { 0.3 })
            .tooltip(tooltip(
                tr("Enter 发送 · Shift+Enter 换行", "Enter to send · Shift+Enter for a new line").into(),
                self.tokens.clone(),
                self.dark,
            ))
            .child(self.icon_with_color("arrow-up", 16., color(&t.background)));
        let composer_focus = self.composer.read(cx).focus_handle().is_focused(window);
        let focus_progress = self.transition(
            "focus:composer".into(),
            composer_focus,
            motion::FOCUS,
            window,
        );
        let mut box_view = rsx! {
            <div w_full min_w_0 flex flex_col p={px(4.)}
                rounded={px(12.)} border_1
                border_color={motion::mix_hsla(
                color(&t.border).opacity(0.7),
                color(&t.border_secondary),
                focus_progress,
            )}
                bg={motion::mix_hsla(
                color(&t.surface),
                color(&t.neutral_active),
                0.24,
            )} />
        };
        let has_chips = !self.files.is_empty() || !self.recent_items.is_empty();
        if has_chips {
            box_view = box_view.child(rsx! {
                <div w_full flex_none pb={px(8.)}>
                    {chips}
                </div>
            });
        }
        let mut input = rsx! {
            <div flex_1 min_w_0>
                {self.composer.clone()}
            </div>
        };
        input.style().align_self = Some(gpui::AlignSelf::Center);
        let tools = rsx! {
            <div w_full min_w_0 flex_none flex items_end gap={px(6.)}>
                {add}
                {input}
                {send}
            </div>
        };
        #[cfg(debug_assertions)]
        let tools = if self.fixture {
            let probe = self.fixture_composer_layout.clone();
            tools.on_children_prepainted(move |bounds, _, _| {
                record_composer_layout(&probe, &["add", "input", "send"], &bounds);
            })
        } else {
            tools
        };
        box_view = box_view.child(tools);
        #[cfg(debug_assertions)]
        if self.fixture {
            let probe = self.fixture_composer_layout.clone();
            box_view = box_view.on_children_prepainted(move |bounds, _, _| {
                if let Some(bounds) = bounds.get(usize::from(has_chips)..) {
                    record_composer_layout(&probe, &["tools"], bounds);
                }
            });
        }
        let pending = self
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message["status"].as_str(),
                    Some("queued" | "partial")
                )
            })
            .count();
        let status = if self.status["running"] != true {
            Some(
                tr(
                    "互传未开启；草稿已保留。",
                    "Sharing is off; your draft is saved.",
                )
                .to_string(),
            )
        } else if !self.has_target() {
            Some(
                tr(
                    "添加并配对设备后即可发送。",
                    "Add and pair a device to send content.",
                )
                .to_string(),
            )
        } else if pending > 0 {
            Some(format!(
                "{pending} {}",
                tr("条内容等待送达", "items awaiting delivery")
            ))
        } else {
            None
        };
        let mut root = rsx! {
            <div w_full flex_none min_w_0 flex flex_col />
        }
            .px(px(12.))
            .pt(px(8.))
            .pb(px(10.))
            .child(box_view);
        #[cfg(debug_assertions)]
        if self.fixture {
            let probe = self.fixture_composer_layout.clone();
            root = root.on_children_prepainted(move |bounds, _, _| {
                record_composer_layout(&probe, &["box"], &bounds);
            });
        }
        if let Some(status) = status {
            root = root.child(
                rsx! {
                    <div min_h={px(16.)} mt={px(5.)} mx={px(9.)} text_size={px(11.)}
                        text_color={color(&t.text_muted)}>
                        {status}
                    </div>
                },
            );
        }
        root.into_any_element()
    }

    fn capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_visible && matches!(event.keystroke.key.as_str(), "tab" | "left" | "right" | "up" | "down" | "home" | "end") {
            self.focus_visible = true;
            cx.notify();
        }
        if event.is_held || self.composing(cx) {
            return;
        }
        let modal = self.devices || self.sessions || self.preview.is_some();
        if event.keystroke.key == "escape" && modal {
            if self.preview.take().is_none() {
                self.dismiss_devices(window, cx);
            } else if let Some(focus) = self.previous_focus.take() {
                focus.focus(window);
            }
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        } else if event.keystroke.key == "escape" && self.companion {
            self.retire_pickers();
            cx.emit(LanManagementEvent::CloseRequested);
            window.prevent_default();
            cx.stop_propagation();
        } else if modal && event.keystroke.key == "tab" {
            let order = &self.overlay_tab_order;
            if !order.is_empty() {
                let current = order.iter().position(|focus| focus.is_focused(window));
                let index = if event.keystroke.modifiers.shift {
                    current
                        .map(|index| (index + order.len() - 1) % order.len())
                        .unwrap_or(order.len() - 1)
                } else {
                    current.map(|index| (index + 1) % order.len()).unwrap_or(0)
                };
                order[index].focus(window);
            } else {
                self.overlay_focus.focus(window);
            }
            window.prevent_default();
            cx.stop_propagation();
        }
    }
}

impl Render for LanManagementPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(debug_assertions)]
        { self.performance.page_renders += 1; }
        self.sync_history_drop();
        let t = self.tokens.theme(self.dark).clone();
        if self.settings_only {
            self.overlay_tab_order.clear();
            let content = self.devices_view(&t, window, cx);
            return div().w_full().min_w_0().flex_none()
                .text_size(px(13.)).text_color(color(&t.text))
                .capture_key_down(cx.listener(Self::capture_key))
                .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                    if this.focus_visible { this.focus_visible = false; cx.notify(); }
                }))
                .child(content).into_any_element();
        }
        let narrow = self.companion || f32::from(window.viewport_size().width) <= NARROW;
        self.overlay_tab_order.clear();
        self.devices_epoch.observe(self.devices || self.sessions);
        let running = self.status["running"] == true;
        let enabled = self.status["enabled"] == true;
        let error = text(&self.status, "error");
        let connections = self
            .control(
                "lan-connections",
                if self.companion { "" } else { tr("设备与连接", "Devices and connection") },
                true,
                &t,
                window,
                cx,
                |this, window, cx| {
                    this.open_devices(false, window, cx);
                    if this.status["running"] == true {
                        this.run(Action::Scan, window, cx);
                    }
                },
            )
            .flex()
            .items_center()
            .gap(px(4.))
            .when(self.companion, |v| v.size(px(28.)).min_h(px(28.)).p(px(0.)).border_0().justify_center())
            .tooltip(tooltip(tr("设备与连接", "Devices and connection").into(), self.tokens.clone(), self.dark))
            .child(self.icon("sliders", if self.companion { 16. } else { 14. }, &t));
        let toggle = self
            .control(
                "lan-sharing-switch",
                "",
                !self.busy,
                &t,
                window,
                cx,
                move |this, window, cx| {
                    this.run(Action::Config(json!({"enabled":!enabled})), window, cx)
                },
            )
            .w(px(29.))
            .h(px(16.))
            .min_h(px(16.))
            .p(px(2.))
            .rounded_full()
            .border_0()
            .flex()
            .items_center()
            .when(enabled, |v| v.justify_end())
            .child(div().size(px(12.)).rounded_full().bg(color(&t.surface)));
        let mut caption = div()
            .h(px(36.))
            .flex_none()
            .pl(px(if self.companion { 12. } else if narrow { 10. } else { 12. }))
            .pr(px(if self.companion { 6. } else if narrow { 10. } else { 12. }))
            .flex()
            .items_center()
            .gap(px(if self.companion { 4. } else if narrow { 7. } else { 12. }))
            .border_b_1()
            .border_color(color(&t.border))
            .bg(color(&t.surface))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if !this.fixture && event.click_count == 1 {
                        if this.companion {
                            cx.emit(LanManagementEvent::MoveRequested);
                            return;
                        }
                        if let Err(error) =
                            crate::platform::begin_host_move(window, &this.host, "main")
                        {
                            log::warn!("LAN caption drag failed: {error}");
                        }
                    }
                }),
            )
            .when(!self.embedded, |caption| caption.child(
                rsx! {
                    <div text_size={px(12.)} font_weight={FontWeight::SEMIBOLD}>
                        {tr("局域网互传", "LAN sharing")}
                    </div>
                },
            ));
        if !self.companion {
            caption = caption.child(
                rsx! {
                    <div flex items_center gap={px(5.)} text_size={px(11.)}
                        text_color={color(&t.text_muted)}>
                        {self.dot(running, &t)}
                    </div>
                }
                    .child(if running {
                        tr("互传已开启", "Sharing on")
                    } else if enabled && !error.is_empty() {
                        tr("启动失败", "Failed to start")
                    } else {
                        tr("互传已关闭", "Sharing off")
                    }),
            );
        }
        caption = caption.child(div().flex_1()).child(connections);
        if !self.companion {
            caption = caption.child(toggle);
        }
        // The main window's own close button keeps its save/leave barrier.
        if (!self.fixture || self.companion) && !self.embedded {
            let close = self
                .control(
                    "lan-window-close",
                    "",
                    true,
                    &t,
                    window,
                    cx,
                    |this, window, cx| {
                        this.retire_pickers();
                        if this.companion {
                            cx.emit(LanManagementEvent::CloseRequested);
                            return;
                        }
                        super::super::close_main(&this.host);
                        window.remove_window();
                    },
                )
                .size(px(28.))
                .min_h(px(28.))
                .p(px(4.))
                .child(self.icon("close", 16., &t));
            caption = caption.child(close);
        }
        let selected = self
            .conversations
            .iter()
            .find(|conversation| conversation["id"] == self.selected)
            .cloned()
            .unwrap_or(Value::Null);
        let peer = conversation_peer(&self.peers, &self.selected)
            .cloned()
            .unwrap_or(Value::Null);
        let paired = self
            .peers
            .iter()
            .filter(|peer| manual_transfer_peer(peer))
            .count();
        let online = self
            .peers
            .iter()
            .filter(|peer| manual_transfer_peer(peer) && peer["online"] == true)
            .count();
        let title = if self.selected == "all" {
            tr("所有设备", "All devices").into()
        } else {
            text(&selected, "title")
        };
        let detail = if self.selected == "all" {
            if paired > 0 {
                format!(
                    "{} {paired} {}",
                    tr("发送到", "Send to"),
                    tr(
                        "台已配对设备；离线设备自动排队",
                        "paired devices; offline devices are queued"
                    )
                )
            } else {
                tr(
                    "配对后，可以在这里向所有设备发送",
                    "Pair a device to send content here",
                )
                .into()
            }
        } else if peer["paired"] != true {
            tr(
                "已解除配对，历史消息保留",
                "Unpaired; message history is retained",
            )
            .into()
        } else if !manual_transfer_peer(&peer) {
            tr("通过剪贴板同步收发文本和图片", "Text and images use clipboard sync").into()
        } else if peer["online"] == true {
            tr("在线 · 可接收内容", "Online · ready to receive").into()
        } else {
            tr(
                "离线 · 发送后排队，上线自动继续",
                "Offline · queued sends resume when online",
            )
            .into()
        };
        let detail = if self.companion {
            if !running {
                tr("互传未开启", "Sharing is off").to_string()
            } else if self.selected == "all" {
                format!("{online}/{paired} {}", tr("台在线 · 离线设备排队", "online · offline devices are queued"))
            } else if peer["paired"] != true {
                tr("已解除配对 · 历史消息保留", "Unpaired · message history retained").into()
            } else if !manual_transfer_peer(&peer) {
                tr("通过剪贴板同步收发文本和图片", "Text and images use clipboard sync").into()
            } else if peer["online"] == true {
                tr("在线 · 可直接发送", "Online · ready to send").into()
            } else {
                tr("离线 · 发送后排队", "Offline · sends are queued").into()
            }
        } else { detail };
        let mut target = div()
            .min_h(px(if narrow { 55. } else { 60. }))
            .flex_none()
            .px(px(if narrow { 14. } else { 20. }))
            .py(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(color(&t.border));
        if narrow && !self.companion {
            let switch = self
                .control(
                    "lan-session-switch",
                    "",
                    true,
                    &t,
                    window,
                    cx,
                    |this, window, cx| this.open_devices(true, window, cx),
                )
                .size(px(28.))
                .p(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .child(self.icon("layout-sidebar", 18., &t));
            target = target.child(switch);
        }
        target = target.child(
            rsx! {
                <div flex_1 min_w_0>
                    <div text_size={px(13.)} font_weight={FontWeight::SEMIBOLD} text_ellipsis>
                        {title}
                    </div>
                    <div mt={px(2.)} text_size={px(11.)} text_color={color(&t.text_muted)}>
                        {detail.clone()}
                    </div>
                </div>
            },
        );
        if !self.companion && self.selected == "all" && paired > 0 {
            target = target.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(color(&t.text_muted))
                    .child(if running {
                        format!("{online}/{paired} {}", tr("在线", "online"))
                    } else {
                        format!("{paired} {}", tr("台已配对", "paired"))
                    }),
            );
        }
        if self.companion {
            let icon = rsx! {
                <div relative w={px(25.)} h={px(24.)} flex_none flex items_center justify_center>
                    {self.icon(if self.selected == "all" { "all" } else { device_icon(&peer) }, 20., &t)}
                </div>
            }.when(self.selected != "all", |v| v.child(
                div().absolute().right_0().bottom_0().child(self.dot(running && peer["online"] == true, &t))
            ));
            let picker = self.control(
                "lan-session-switch", "", true, &t, window, cx,
                |this, window, cx| this.open_devices(true, window, cx),
            )
                .w_full().min_w_0().min_h(px(42.)).px(px(8.)).py(px(2.))
                .rounded(px(7.)).flex().items_center().gap(px(8.))
                .child(icon)
                .child(rsx! {
                    <div flex_1 min_w_0>
                        <div text_size={px(13.)} font_weight={FontWeight::SEMIBOLD} text_ellipsis>
                            {if self.selected == "all" { tr("所有设备", "All devices").to_string() } else { text(&selected, "title") }}
                        </div>
                        <div mt={px(2.)} text_size={px(11.)} text_color={color(&t.text_muted)} text_ellipsis>
                            {detail}
                        </div>
                    </div>
                })
                .child(self.icon("chevron-down", 14., &t));
            target = rsx! {
                <div min_h={px(50.)} flex_none px={px(8.)} py={px(3.)} flex items_center
                    border_b_1 border_color={color(&t.border)}>
                    {picker}
                </div>
            };
        }
        let mut conversation = rsx! {
            <div id={"lan-conversation"} relative flex_1 min_w_0 min_h_0 flex flex_col>
                {target}
            </div>
        }
            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                let over = event.bounds.contains(&event.event.position) && this.history_drop_allowed(drag);
                if this.history_drop_hovered != over {
                    this.history_drop_hovered = over;
                    cx.notify();
                }
                if over { this.offer_history_drop(drag, event.bounds, window); }
            }))
            .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                this.accept_history_drop(drag, window, cx);
            }));
        if !running {
            let start = self
                .control(
                    "lan-enable",
                    if enabled {
                        tr("重试启动", "Retry start")
                    } else {
                        tr("开启互传", "Enable sharing")
                    },
                    !self.busy,
                    &t,
                    window,
                    cx,
                    |this, window, cx| {
                        this.run(Action::Config(json!({"enabled":true})), window, cx)
                    },
                )
                .flex_none();
            let reason = if !error.is_empty() {
                error.clone()
            } else {
                tr(
                    "先开启互传，再发送或接收内容。当前草稿已保留。",
                    "Enable sharing to send and receive content. Your draft is saved.",
                )
                .into()
            };
            conversation = conversation.child(
                div()
                    .flex_none()
                    .px(px(if narrow { 14. } else { 18. }))
                    .py(px(9.))
                    .border_b_1()
                    .border_color(color(&t.border))
                    .bg(color(&t.surface))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .child(div().font_weight(FontWeight::MEDIUM).child(if enabled {
                                tr("互传未能启动", "Sharing could not start")
                            } else {
                                tr("互传已关闭", "Sharing is off")
                            }))
                            .child(div().text_color(color(&t.text_muted)).child(reason)),
                    )
                    .child(start),
            );
        }
        if let Some(error) = self
            .error
            .clone()
            .filter(|_| !self.devices && !self.sessions)
        {
            conversation = conversation.child(
                rsx! {
                    <div px={px(18.)} py={px(5.)} text_size={px(11.)} text_color={color(&t.error)}>
                        {error}
                    </div>
                },
            );
        }
        let mut message_area = rsx! {
            <div id={"lan-message-area"} relative flex_1 min_h_0 min_w_0 flex flex_col />
        }.on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            if *hovered { this.hovered_controls.insert("area:messages".into()); }
            else { this.hovered_controls.remove("area:messages"); }
            cx.notify();
        }));
        if self.messages.is_empty() {
            let first = paired == 0;
            let mut empty=rsx! {
                <div flex_1 min_h_0 p={px(20.)} flex flex_col items_center justify_center
                    gap={px(10.)} text_size={px(13.)} />
            }
                .child(div().font_weight(FontWeight::SEMIBOLD).child(if first {tr("把内容发到另一台电脑", "Send content to another device")}else{tr("已准备好互传", "Ready to share")}))
                .child(div().max_w(px(285.)).text_color(color(&t.text_secondary)).child(if first {tr("在两台设备上开启互传，添加设备并完成一次配对。", "Enable sharing on both devices, then add and pair the other device.")}else{tr("输入文字，或添加图片和文件开始发送。", "Type text or add images and files to start sending.")}));
            if first {
                let add = self
                    .control(
                        "lan-first-add",
                        tr("＋ 添加设备", "＋ Add device"),
                        true,
                        &t,
                        window,
                        cx,
                        |this, window, cx| this.open_devices(false, window, cx),
                    );
                empty = empty.child(add).child(
                    rsx! {
                        <div text_size={px(11.)} text_color={color(&t.text_muted)}>
                            {tr(
                            "同一局域网内使用，无需登录。",
                            "Use on the same LAN. No sign-in required.",
                        )}
                        </div>
                    },
                );
            }
            message_area = message_area.child(empty);
        } else {
            let weak = cx.weak_entity();
            let virtual_list = list(self.list.clone(), move |index, window, cx| {
                weak.update(cx, |this, cx| {
                    let Some(message) = this.messages.get(index).cloned() else {
                        return div().into_any_element();
                    };
                    let t = this.tokens.theme(this.dark).clone();
                    let continued = continued_message(
                        index
                            .checked_sub(1)
                            .and_then(|index| this.messages.get(index)),
                        &message,
                        this.selected == "all",
                    );
                    let row = this.message_view(message, continued, &t, window, cx);
                    div()
                        .w_full()
                        .min_w_0()
                        .px(px(if this.companion { 12. } else if narrow { 18. } else { 22. }))
                        .pt(px(if index == 0 {
                            10.
                        } else if continued {
                            4.
                        } else {
                            8.
                        }))
                        .pb(px(if index + 1 == this.messages.len() {
                            12.
                        } else {
                            0.
                        }))
                        .child(row)
                        .into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element())
            })
            .flex_1()
            .min_h_0()
            .w_full();
            message_area = message_area.child(virtual_list);
            let weak = cx.weak_entity();
            let focused = self.message_inputs.values().any(|input| input.read(cx).focus_handle().is_focused(window))
                || self.focus_handles.iter().any(|(id, focus)|
                    (id == "lan-latest" || ["lan-open:", "lan-reveal:", "lan-copy:",
                        "lan-retry:", "lan-history-retry:", "lan-cancel:", "lan-cancel-queued:", "lan-deliveries:", "lan-image:"]
                        .iter().any(|prefix| id.starts_with(prefix)))
                        && focus.is_focused(window));
            let scrollbar = self.message_scrollbar.render(
                "lan-message-scrollbar", ScrollbarTarget::List(self.list.clone()), self.lifetime,
                self.lan_scrollbar_style(&t, focused || self.hovered_controls.contains("area:messages")),
                Rc::new(move |_, cx| { let _ = weak.update(cx, |_, cx| cx.notify()); }),
            );
            message_area = message_area.child(scrollbar);
            if !self.near_bottom() {
                let latest = self
                    .control(
                        "lan-latest",
                        tr("回到最新消息", "Latest messages"),
                        true,
                        &t,
                        window,
                        cx,
                        |this, _, cx| {
                            this.list.scroll_to(ListOffset {
                                item_ix: this.messages.len(),
                                offset_in_item: px(0.),
                            });
                            cx.notify();
                        },
                    )
                    .rounded_full();
                message_area = message_area.child(
                    rsx! {
                        <div absolute right={px(18.)} bottom={px(10.)}>
                            {latest}
                        </div>
                    },
                );
            }
        }
        let composer = self.composer_view(&t, narrow, window, cx);
        conversation = conversation.child(message_area).child(composer);
        if self.can_receive_history() {
            let target = self.selected.clone();
            let host = self.host.clone();
            conversation = conversation.child(
                gpui::canvas(
                    move |bounds, window, _| {
                        crate::favorite_drag::register_lan_region(&target, bounds, window, host);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        }
        if self.history_drop_hovered && !self.devices && !self.sessions && self.preview.is_none() {
            let name = if self.selected == "all" { tr("所有设备", "all devices").to_string() } else { text(&selected, "title") };
            conversation = conversation.child(rsx! {
                <div absolute inset_0 p={px(20.)} flex items_center justify_center
                    bg={color(&t.background).opacity(0.88)}>
                    <div w_full min_w_0 px={px(16.)} py={px(30.)} rounded={px(12.)}
                        border_1 border_color={color(&t.accent)} bg={color(&t.surface)} flex flex_col
                        items_center gap={px(10.)}>
                        {self.icon_with_color("arrow-up", 24., color(&t.accent))}
                        <div text_size={px(14.)} font_weight={FontWeight::SEMIBOLD}>
                            {format!("{} {name}", tr("松开发送给", "Release to send to"))}
                        </div>
                        <div text_size={px(11.)} line_height={px(18.)} text_color={color(&t.text_secondary)}>
                            {tr("仅发送拖入的内容，输入草稿会保留", "Sends only the dropped content and keeps your draft")}
                        </div>
                    </div>
                </div>
            });
        }
        let mut workspace = rsx! {
            <div flex_1 min_h_0 min_w_0 flex />
        };
        if !narrow {
            let sidebar = self.sidebar_view(false, &t, window, cx);
            workspace = workspace.child(sidebar);
        }
        let mut root = rsx! {
            <div relative size_full min_w_0 min_h_0 flex flex_col text_size={px(13.)}
                text_color={color(&t.text)} bg={color(&t.background)} />
        }
            .capture_key_down(cx.listener(Self::capture_key))
            .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.focus_visible { this.focus_visible = false; cx.notify(); }
            }))
            .on_mouse_move(cx.listener(|this, _, _, cx| {
                if this.history_drop_hovered && !cx.has_active_drag() {
                    this.history_drop_hovered = false;
                    cx.notify();
                }
            }))
            .child(caption)
            .child(workspace.child(conversation));
        if self.devices || self.sessions {
            root = root.child(self.devices_view(&t, window, cx));
        }
        if let Some(attachment) = self.preview.clone() {
            root = root.child(self.preview_view(attachment, &t, window, cx));
        }
        root.into_any_element()
    }
}

impl LanManagementPage {
    fn transfer_view(
        &mut self,
        message: &Value,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if message["status"] != "sending" {
            return div().into_any_element();
        }
        let id = text(message, "id");
        let progress = message_progress(message);
        let label = if self.status["running"] == true {
            tr("发送中", "Sending")
        } else {
            tr("已暂停，开启后继续", "Paused; enable sharing to continue")
        };
        let cancel = self.control(
            format!("lan-cancel:{id}"),
            tr("取消", "Cancel"),
            !self.busy,
            t,
            window,
            cx,
            move |this, window, cx| this.run(Action::Cancel(id.clone()), window, cx),
        );
        let mut root = rsx! {
            <div w_full min_w_0 flex flex_col gap={px(5.)}>
                <div flex items_center justify_between gap={px(8.)} text_size={px(11.)}
                    text_color={color(&t.text_secondary)}>
                    {format!(
                        "{label}{}",
                        progress.map(|v| format!(" · {v}%")).unwrap_or_default()
                    )}
                    {cancel}
                </div>
            </div>
        };
        if let Some(progress) = progress {
            root = root.child(
                rsx! {
                    <div h={px(3.)} w_full rounded_full bg={color(&t.border)}>
                        <div h_full w={gpui::relative(progress as f32 / 100.)} rounded_full
                            bg={color(&t.text_secondary)} />
                    </div>
                },
            );
        }
        root.into_any_element()
    }
    fn message_state(
        &mut self,
        message: &Value,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = text(message, "id");
        let status = text(message, "status");
        let deliveries = values(message, "deliveries");
        let (retry, cancel) = message_actions(message);
        let cancel = cancel || status == "partial";
        let succeeded = matches!(status.as_str(), "received" | "delivered");
        let reason = deliveries.iter().filter(|delivery| !text(delivery, "lastError").is_empty())
            .map(|delivery| format!("{}: {}", text(delivery, "targetAlias"), text(delivery, "lastError")))
            .collect::<Vec<_>>().join("\n");
        let state_tip = if !reason.is_empty() { reason } else if status == "canceled" {
            tr("已取消 · 已送达内容不会撤回", "Canceled · delivered content is not recalled").into()
        } else if status == "queued" {
            if self.status["running"] == true {
                tr("等待设备上线，自动发送", "Waiting for device; sends automatically").into()
            } else {
                tr("已排队，开启互传后继续", "Queued; enable sharing to continue").into()
            }
        } else { message_status(message) };
        let mut state = rsx! {
            <div id={SharedString::from(format!("lan-message-state:{id}"))} min_w_0 flex flex_wrap items_center
                gap={px(5.)} text_size={px(10.)} line_height={px(14.)} />
        }
            .text_color(color(if status == "failed" {
                &t.error
            } else {
                &t.text_muted
            }))
            .tooltip(tooltip(state_tip, self.tokens.clone(), self.dark));
        if succeeded {
            state = state.child(div().text_size(px(11.)).child("✓"));
        }
        if status != "sending" {
            let label = if status == "queued" {
                    if self.status["running"] == true { tr("等待上线", "Waiting for device") } else { tr("已排队", "Queued") }
                .into()
            } else if status == "canceled" {
                tr("已取消", "Canceled")
                .into()
            } else if status == "partial" {
                tr("部分未送达", "Partially undelivered").into()
            } else {
                message_status(message)
            };
            if !succeeded {
                state = state.child(div().min_w_0().child(label));
            }
        }
        if retry {
            let retry_id = id.clone();
            state = state.child(self.control(
                format!("lan-retry:{id}"),
                if deliveries.len() > 1 {
                    tr("仅重试未送达", "Retry undelivered only")
                } else {
                    tr("重试", "Retry")
                },
                !self.busy && self.status["running"] == true,
                t,
                window,
                cx,
                move |this, window, cx| this.run(Action::Retry(retry_id.clone()), window, cx),
            ));
        }
        if cancel && status != "sending" {
            let cancel_id = id.clone();
            state = state.child(self.control(
                format!("lan-cancel-queued:{id}"),
                if status == "partial" { tr("取消", "Cancel") } else { tr("取消排队", "Cancel queued send") },
                !self.busy,
                t,
                window,
                cx,
                move |this, window, cx| this.run(Action::Cancel(cancel_id.clone()), window, cx),
            ));
        }
        if deliveries.len() > 1 {
            let expanded = self.expanded_deliveries.contains(&id);
            let delivered = deliveries
                .iter()
                .filter(|d| d["status"] == "delivered")
                .count();
            state = state.child(self.control(
                format!("lan-deliveries:{id}"),
                format!(
                    "{delivered}/{} {} {}",
                    deliveries.len(),
                    tr("台已送达", "delivered"),
                    if expanded { "▾" } else { "▸" }
                ),
                true,
                t,
                window,
                cx,
                move |this, _, cx| {
                    if !this.expanded_deliveries.remove(&id) {
                        this.expanded_deliveries.insert(id.clone());
                    }
                    cx.notify();
                },
            ));
        }
        state.into_any_element()
    }
    fn delivery_details(&self, message: &Value, t: &ThemeTokens) -> AnyElement {
        if !self.expanded_deliveries.contains(&text(message, "id")) {
            return div().into_any_element();
        }
        let mut details = rsx! {
            <div w_full mt={px(5.)} pt={px(5.)} border_t_1 border_color={color(&t.border)} flex
                flex_col gap={px(4.)} text_size={px(11.)} />
        };
        for delivery in values(message, "deliveries") {
            details = details.child(
                rsx! {
                    <div flex justify_between gap={px(10.)}>
                        <div min_w_0>
                            {text(&delivery, "targetAlias")}
                        </div>
                        <div text_color={color(&t.text_muted)}>
                            {message_status(&delivery)}
                        </div>
                    </div>
                },
            );
        }
        details.into_any_element()
    }
    fn attachment_view(
        &mut self,
        attachment: Value,
        message: &Value,
        single: bool,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = text(&attachment, "id");
        let name = text(&attachment, "name");
        let path = text(&attachment, "localPath");
        let is_image = image_attachment(&attachment);
        let available = matches!(message["status"].as_str(), Some("received" | "delivered"));
        let group: SharedString = format!("lan-file:{id}").into();
        let action_focused = [format!("lan-open:{id}"), format!("lan-reveal:{id}")].iter()
            .any(|id| self.focus_handles.get(id).is_some_and(|focus| focus.is_focused(window)));
        let mut card = rsx! {
            <div w={px(FILE_WIDTH)} max_w_full min_w_0 flex flex_col rounded={px(7.)} border_1
                border_color={color(&t.border)} bg={color(&t.surface)} />
        }.group(group.clone());
        if is_image {
            let bitmap = self.image(
                path.clone(),
                text(&attachment, "sha256"),
                ImagePurpose::Attachment,
                cx,
            );
            let mut preview = self
                .control(format!("lan-image:{id}"), "", true, t, window, cx, {
                    let attachment = attachment.clone();
                    move |this, window, cx| {
                        this.previous_focus = window.focused(cx);
                        this.preview = Some(attachment.clone());
                        this.overlay_focus.focus(window);
                        cx.notify();
                    }
                })
                .w_full()
                .p(px(0.))
                .rounded(px(6.))
                .overflow_hidden();
            if let Some((image, width, height)) = bitmap {
                let scale = (302. / width).min(164. / height).min(1.);
                preview = preview.child(
                    rsx! {
                        <div w_full flex justify_center>
                            {img(image)
                            .w(px(width * scale))
                            .h(px(height * scale))
                            .object_fit(gpui::ObjectFit::Contain)}
                        </div>
                    },
                );
            } else {
                preview = preview.child(
                    rsx! {
                        <div h={px(72.)} w_full flex items_center justify_center
                            text_size={px(11.)} text_color={color(&t.text_muted)}>
                            {tr("图片预览", "Image preview")}
                        </div>
                    },
                );
            }
            card = card.child(preview);
        }
        let open_id = id.clone();
        let open = self.control(
            format!("lan-open:{id}"),
            "",
            available && !self.busy,
            t,
            window,
            cx,
            move |this, window, cx| this.run(Action::Open(open_id.clone()), window, cx),
        ).size(px(24.)).min_h(px(24.)).p(px(0.)).flex().items_center().justify_center()
            .tooltip(tooltip(tr("打开", "Open").into(), self.tokens.clone(), self.dark))
            .child(self.icon("open-panel", 13., t));
        let reveal_id = id.clone();
        let reveal = self.control(
            format!("lan-reveal:{id}"),
            "",
            available && !self.busy,
            t,
            window,
            cx,
            move |this, window, cx| this.run(Action::Reveal(reveal_id.clone()), window, cx),
        ).size(px(24.)).min_h(px(24.)).p(px(0.)).flex().items_center().justify_center()
            .tooltip(tooltip(tr("定位", "Reveal").into(), self.tokens.clone(), self.dark))
            .child(self.icon("folder-open", 13., t));
        let actions = rsx! {
            <div flex_none flex items_center gap={px(2.)}>
                {open}
                {reveal}
            </div>
        }.when(!action_focused, |v| v.invisible().group_hover(group, |v| v.visible()));
        let mut meta = rsx! {
            <div mt={px(2.)} min_w_0 flex items_center gap={px(6.)} text_size={px(10.)}
                line_height={px(14.)} text_color={color(&t.text_muted)}>
                {format_bytes(attachment["sizeBytes"].as_u64().unwrap_or(0))}
            </div>
        }.when(is_image, |v| v.mt(px(0.)).flex_none());
        if single && available && values(message, "deliveries").len() <= 1 {
            meta = meta.child(self.message_state(message, t, window, cx));
        }
        let info = div().flex_1().min_w_0()
            .when(is_image, |v| v.flex().items_center().gap(px(6.)))
            .child(div().id(SharedString::from(format!("lan-file-name:{id}")))
                .min_w_0().when(is_image, |v| v.flex_1())
                .text_size(px(if is_image { 10. } else { 12. })).line_height(px(18.)).text_ellipsis()
                .tooltip(tooltip(name.clone(), self.tokens.clone(), self.dark)).child(name))
            .child(meta);
        let mut main = rsx! {
            <div px={px(9.)} py={px(if is_image { 5. } else { 9. })} min_w_0 flex items_center gap={px(8.)} />
        };
        if !is_image {
            main = main.child(rsx! {
                <div w={px(24.)} h={px(28.)} flex_none flex items_center justify_center>
                    {self.icon("file", 24., t)}
                </div>
            });
        }
        card = card.child(main.child(info).child(actions));
        if single && message["status"] == "sending" {
            let transfer = self.transfer_view(message, t, window, cx);
            card = card.child(rsx! { <div px={px(9.)} pb={px(7.)}>{transfer}</div> });
        }
        if single && (!available || values(message, "deliveries").len() > 1)
            && (message["status"] != "sending" || values(message, "deliveries").len() > 1) {
            let state = self.message_state(message, t, window, cx);
            card = card.child(rsx! { <div px={px(9.)} pb={px(5.)} min_w_0>{state}</div> });
        }
        if single && self.expanded_deliveries.contains(&text(message, "id")) {
            card = card.child(rsx! {
                <div px={px(9.)} pb={px(7.)} min_w_0>{self.delivery_details(message, t)}</div>
            });
        }
        card.into_any_element()
    }
    fn message_view(
        &mut self,
        message: Value,
        continued: bool,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let outgoing = message["direction"] == "outgoing";
        let id = text(&message, "id");
        let time = format_time(&message["createdAtUtc"]);
        let author = if outgoing {
            tr("我", "Me").into()
        } else {
            let alias = text(&message, "senderAlias");
            if alias.is_empty() {
                self.conversations
                    .iter()
                    .find(|c| c["id"] == self.selected)
                    .map(|v| text(v, "title"))
                    .unwrap_or_default()
            } else {
                alias
            }
        };
        let attachments = values(&message, "attachments");
        let single = attachments.len() == 1;
        let inline_success = attachments.is_empty()
            && matches!(message["status"].as_str(), Some("received" | "delivered"))
            && values(&message, "deliveries").len() <= 1;
        let width = (f32::from(window.viewport_size().width)
            - if !self.companion && f32::from(window.viewport_size().width) > NARROW {
                SIDEBAR
            } else {
                0.
            }
            - if self.companion { 24. } else { 44. })
            * if self.companion { 0.88 } else { 0.86 };
        let max_width = width.min(440.).max(140.);
        let mut card = rsx! {
            // Cached text views have no intrinsic width during layout. Keep a
            // definite containing width even when success has no status row.
            <div w={px(max_width)} max_w={px(max_width)} min_w_0 flex flex_col gap={px(4.)} />
        }
            .when(outgoing, |v| v.items_end());
        if !continued && !outgoing {
            card = card.child(
                rsx! {
                    <div flex gap={px(7.)} text_size={px(10.)}
                        text_color={color(&t.text_muted)}>
                        {author.clone()}
                        {time.clone()}
                    </div>
                },
            );
        }
        let body = text(&message, "text");
        if !body.is_empty() {
            let horizontal_space = if inline_success { 35. } else { 20. };
            let text_max_width = max_width - horizontal_space;
            let scale_factor = window.scale_factor();
            let input = if let Some(input) = self.message_inputs.get(&id) {
                input.clone()
            } else {
                let input = cx.new(|cx| {
                    let mut input = MultilineInput::new(window, cx);
                    input.set_read_only(true, cx);
                    input.set_text(body.clone(), window, cx);
                    input
                });
                self.message_inputs.insert(id.clone(), input.clone());
                input
            };
            let cached = self.message_layouts.get(&id).filter(|layout| {
                layout.max_width == text_max_width && layout.font_family == self.tokens.typography.font_family
                    && layout.scale_factor == scale_factor
            });
            let (body_width, height) = if let Some(layout) = cached {
                #[cfg(debug_assertions)]
                { self.performance.message_layout_hits += 1; }
                (layout.width, layout.height)
            } else {
              #[cfg(debug_assertions)]
              { self.performance.message_layout_misses += 1; }
              let run = gpui::TextRun {
                len: body.len(),
                font: gpui::font(self.tokens.typography.font_family.clone()),
                color: color(&t.text),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let unwrapped = body
                .lines()
                .map(|line| {
                    let text: SharedString = line.to_string().into();
                    window
                        .text_system()
                        .shape_line(
                            text.clone(),
                            px(13.),
                            &[gpui::TextRun {
                                len: text.len(),
                                ..run.clone()
                            }],
                            None,
                        )
                        .width
                })
                .max()
                .unwrap_or(px(86.));
            let (body_width, wrap_width) = message_text_widths(
                f32::from(unwrapped), horizontal_space, max_width, scale_factor,
            );
              let height = window
                .text_system()
                .shape_text(
                    body.clone().into(),
                    px(13.),
                    &[run],
                    Some(px(wrap_width)),
                    None,
                )
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| f32::from(line.size(px(19.)).height))
                        .sum::<f32>()
                })
                .unwrap_or(19.)
                .clamp(19., 304.);
              self.message_layouts.insert(id.clone(), MessageTextLayout {
                  max_width: text_max_width, font_family: self.tokens.typography.font_family.clone(), scale_factor, width: body_width, height,
              });
              (body_width, height)
            };
            input.update(cx, |input, cx| {
                if input.text() != body {
                    input.set_text(body.clone(), window, cx);
                }
                input.set_metrics(height + 12., 13., 0., cx);
                input.set_colors(
                    InputColors {
                        background: gpui::hsla(0., 0., 0., 0.),
                        foreground: color(&t.text),
                        placeholder: color(&t.text_muted),
                        border: gpui::hsla(0., 0., 0., 0.),
                        selection: color(&t.selected),
                        caret: color(&t.text),
                    },
                    cx,
                );
            });
            let copy_body = body.clone();
            let group: SharedString = format!("lan-bubble:{id}").into();
            let focused = input.read(cx).focus_handle().is_focused(window)
                || self
                    .focus_handles
                    .get(&format!("lan-copy:{id}"))
                    .is_some_and(|focus| focus.is_focused(window));
            let copy = self
                .control(
                    format!("lan-copy:{id}"),
                    "",
                    true,
                    t,
                    window,
                    cx,
                    move |this, _, cx| {
                        if !this.fixture {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_body.clone()));
                        }
                        this.notice = Some(tr("正文已复制", "Message copied").into());
                        cx.notify();
                    },
                )
                .size(px(24.)).min_h(px(24.)).p(px(0.)).flex().items_center().justify_center()
                .tooltip(tooltip(tr("复制", "Copy").into(), self.tokens.clone(), self.dark))
                .child(self.icon("copy", 13., t))
                .absolute()
                .top(px(3.))
                .right(px(3.))
                .when(!focused, |v| {
                    v.invisible().group_hover(group.clone(), |v| v.visible())
                });
            card = card.child(
                div()
                    .relative()
                    .group(group)
                    .w(px(body_width))
                    .max_w_full()
                    .pl(px(9.))
                    .pr(px(if inline_success { 24. } else { 9. }))
                    // MultilineInput retains its 6 DIP vertical inset. The
                    // outer 1 DIP brings the bubble to the confirmed 7 DIP.
                    .py(px(1.))
                    .rounded(px(if outgoing { 8. } else { 7. }))
                    .when(outgoing, |v| v.rounded_tr(px(0.)))
                    .when(!outgoing, |v| v.rounded_tl(px(0.)))
                    .border_1()
                    .border_color(color(&t.border))
                    .bg(if outgoing {
                        motion::mix_hsla(color(&t.background), color(&t.neutral_active), 0.55)
                    } else { color(&t.surface) })
                    .child(gpui::AnyView::from(input).cached(
                        gpui::StyleRefinement::default().w_full().h(px(height + 12.)),
                    ))
                    .child(copy)
                    .when(inline_success, |v| v.child(div().absolute().right(px(7.)).bottom(px(5.))
                        .text_size(px(11.)).line_height(px(14.)).text_color(color(&t.text_muted)).child("✓"))),
            );
        }
        for attachment in attachments.iter().cloned() {
            let item = self.attachment_view(attachment, &message, single, t, window, cx);
            card = card.child(item);
        }
        if let Some(error) = history_import_failure(&message) {
            let sender = text(&message, "senderDeviceId");
            let message_id = id.clone();
            let retry = self.control(format!("lan-history-retry:{id}"), tr("重试加入", "Retry adding"),
                !self.busy && !sender.is_empty(), t, window, cx,
                move |this, window, cx| this.run(Action::RetryHistory { sender: sender.clone(), message: message_id.clone() }, window, cx));
            card = card.child(rsx! {
                <div w={px(FILE_WIDTH.min(max_width))} max_w_full min_w_0 flex items_center gap={px(8.)}>
                    <div flex_1 min_w_0 text_size={px(11.)} line_height={px(16.)} text_color={color(&t.error)}>
                        {tr("加入剪贴板列表失败", "Could not add to clipboard history")}
                    </div>
                </div>
            }.id(SharedString::from(format!("lan-history-error:{id}")))
                .tooltip(tooltip(error.to_owned(), self.tokens.clone(), self.dark)).child(retry));
        }
        if !single && !inline_success {
            let mut shared = div()
                .w(px(if attachments.is_empty() {
                    max_width
                } else {
                    FILE_WIDTH.min(max_width)
                }))
                .max_w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .when(outgoing, |v| v.items_end());
            if attachments.len() > 1 {
                shared = shared.mt(px(4.)).child(
                    rsx! {
                        <div text_size={px(11.)} text_color={color(&t.text_muted)}>
                            {format!(
                            "{} · {} {}",
                            tr("本次发送", "This send"),
                            attachments.len(),
                            tr("个附件", "attachments")
                        )}
                        </div>
                    },
                );
            }
            if message["status"] == "sending" {
                let transfer = self.transfer_view(&message, t, window, cx);
                shared = shared.child(transfer);
            }
            let state = self.message_state(&message, t, window, cx);
            shared = shared
                .child(state)
                .child(self.delivery_details(&message, t));
            card = card.child(shared);
        }
        #[cfg(debug_assertions)]
        if self.fixture && !body.is_empty() {
            let probe = self.fixture_message_layout.clone();
            let probe_id = id.clone();
            let text_width = self.message_layouts[&id].width - if inline_success { 35. } else { 20. };
            let text_height = self.message_layouts[&id].height;
            card = card.on_children_prepainted(move |bounds, _, _| {
                if let Some(bounds) = bounds.get(usize::from(!continued && !outgoing)) {
                    probe.lock().unwrap()[&probe_id] = json!({
                        "singleLineWidth":text_width, "expectedTextHeight":text_height,
                        "bubbleBounds":{"x":f32::from(bounds.left()), "y":f32::from(bounds.top()),
                            "width":f32::from(bounds.size.width), "height":f32::from(bounds.size.height),
                            "right":f32::from(bounds.right()), "bottom":f32::from(bounds.bottom())},
                    });
                }
            });
        }
        div()
            .id(SharedString::from(format!("lan-message:{id}")))
            .w_full()
            .flex()
            .when(outgoing, |v| v.justify_end())
            .tooltip(tooltip(
                format!("{author} · {time}"),
                self.tokens.clone(),
                self.dark,
            ))
            .child(card)
            .into_any_element()
    }
}

fn history_import_failure(message: &Value) -> Option<&str> {
    (message["direction"] == "incoming" && message["status"] == "received"
        && message["historyImportRequested"] == true && message["historyImportCompleted"] != true)
        .then(|| message["historyImportError"].as_str().filter(|error| !error.is_empty())).flatten()
}

#[cfg(test)]
mod history_import_feedback_tests {
    use super::*;

    #[test]
    fn only_requested_incomplete_received_imports_show_recovery() {
        let failed = json!({"direction":"incoming", "status":"received", "historyImportRequested":true,
            "historyImportCompleted":false, "historyImportError":"disk unavailable"});
        assert_eq!(history_import_failure(&failed), Some("disk unavailable"));
        for (field, value) in [("historyImportRequested", json!(false)), ("historyImportCompleted", json!(true)),
            ("historyImportError", json!("")), ("status", json!("sending")), ("direction", json!("outgoing"))] {
            let mut other = failed.clone();
            other[field] = value;
            assert_eq!(history_import_failure(&other), None);
        }
        assert_eq!(history_import_failure(&json!({"status":"received"})), None);
    }
}

fn device_icon(value: &Value) -> &'static str {
    match value["deviceKind"].as_str().unwrap_or_default() {
        "laptop" => "laptop",
        "phone" => "phone",
        _ => "desktop",
    }
}
fn device_label(kind: &str) -> &'static str {
    match kind {
        "desktop" => tr("电脑", "Computer"),
        "laptop" => tr("笔记本", "Laptop"),
        "phone" => tr("手机", "Phone"),
        _ => tr("未识别", "Unknown"),
    }
}
fn continued_message(previous: Option<&Value>, current: &Value, group: bool) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if previous["direction"] != current["direction"] {
        return false;
    }
    if current["direction"] == "incoming" {
        let sender = text(current, "senderDeviceId");
        let old_sender = text(previous, "senderDeviceId");
        if (group && (sender.is_empty() || old_sender.is_empty())) || sender != old_sender {
            return false;
        }
    }
    let Some(before) = previous["createdAtUtc"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    let Some(now) = current["createdAtUtc"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    let distance = now.signed_duration_since(before).num_milliseconds();
    (0..=30_000).contains(&distance)
        && now.with_timezone(&chrono::Local).date_naive()
            == before.with_timezone(&chrono::Local).date_naive()
}
const PAIRING_TTL_SECONDS: f32 = 300.; // cliphost_core::lan::PAIRING_TTL

fn card(t: &ThemeTokens) -> gpui::Div {
    div().flex().flex_col().min_w_0().overflow_hidden().rounded(px(8.)).border_1()
        .border_color(color(&t.border)).bg(color(&t.surface))
}

fn device_row() -> gpui::Div {
    div().flex().items_center().gap(px(10.)).min_w_0().min_h(px(52.)).pl(px(12.)).pr(px(10.)).py(px(9.))
}

fn row_text(title: impl Into<SharedString>, detail: impl Into<SharedString>, t: &ThemeTokens) -> gpui::Div {
    div().flex_1().min_w_0().flex().flex_col().gap(px(1.))
        .child(div().truncate().text_size(px(13.)).line_height(px(18.)).font_weight(FontWeight::MEDIUM)
            .text_color(color(&t.text)).child(title.into()))
        .child(div().truncate().text_size(px(11.)).line_height(px(15.)).text_color(color(&t.text_muted)).child(detail.into()))
}

fn heading(title: &'static str, count: Option<String>, t: &ThemeTokens) -> gpui::Div {
    div().min_h(px(24.)).mb(px(6.)).px(px(2.)).flex().items_center().justify_between().gap(px(8.))
        .child(div().flex().items_baseline().gap(px(6.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD)
            .text_color(color(&t.text_secondary)).child(title)
            .children(count.map(|count| div().text_size(px(11.)).font_weight(FontWeight::NORMAL)
                .text_color(color(&t.text_muted)).child(count))))
}

fn section(heading: gpui::Div, card: gpui::Div) -> gpui::Div {
    div().flex_none().flex().flex_col().min_w_0().child(heading).child(card)
}

fn note(content: impl Into<SharedString>, top: f32, t: &ThemeTokens) -> gpui::Div {
    div().mt(px(top)).px(px(2.)).text_size(px(11.)).line_height(px(18.)).text_color(color(&t.text_muted)).child(content.into())
}

fn strict_config(alias: &str, port: &str, enabled: bool) -> Result<Value, String> {
    if alias.trim().is_empty() || alias.encode_utf16().count() > 64 {
        return Err(tr(
            "设备名称不能为空，且不能超过 64 个字符。",
            "Enter a device name with at most 64 characters.",
        )
        .into());
    }
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return Err(tr(
            "端口须为 1024–65535 的整数。",
            "Port must be an integer from 1024 to 65535.",
        )
        .into());
    }
    let port: u16 = port.parse().map_err(|_| {
        tr(
            "端口须为 1024–65535 的整数。",
            "Port must be an integer from 1024 to 65535.",
        )
    })?;
    if port < 1024 {
        return Err(tr(
            "端口须为 1024–65535 的整数。",
            "Port must be an integer from 1024 to 65535.",
        )
        .into());
    }
    Ok(json!({"enabled":enabled,"deviceAlias":alias.trim(),"port":port}))
}

#[cfg(test)]
mod alignment_tests {
    use super::*;

    #[test]
    fn rapid_messages_require_same_sender_and_a_forward_thirty_second_window() {
        let first = json!({"direction":"incoming","senderDeviceId":"peer-a","createdAtUtc":"2026-10-17T12:00:00Z"});
        let mut next = json!({"direction":"incoming","senderDeviceId":"peer-a","createdAtUtc":"2026-10-17T12:00:30Z"});
        assert!(continued_message(Some(&first), &next, true));
        next["createdAtUtc"] = json!("2026-10-17T12:00:30.001Z");
        assert!(!continued_message(Some(&first), &next, true));
        next["createdAtUtc"] = json!("2026-10-17T11:59:59Z");
        assert!(!continued_message(Some(&first), &next, true));
        next["createdAtUtc"] = json!("2026-10-17T12:00:04Z");
        next["senderDeviceId"] = json!("peer-b");
        assert!(!continued_message(Some(&first), &next, true));
        next["senderDeviceId"] = Value::Null;
        assert!(!continued_message(Some(&first), &next, true));
        next["direction"] = json!("outgoing");
        assert!(!continued_message(Some(&first), &next, false));
    }

    #[test]
    fn connection_drafts_reject_invalid_values_before_saving() {
        for port in ["", "1023", "65536", "29766x", "-1", " 29766"] {
            assert!(strict_config("Device", port, true).is_err(), "{port}");
        }
        assert!(strict_config("   ", "29766", true).is_err());
        assert!(strict_config(&"😀".repeat(33), "29766", true).is_err());
        assert_eq!(
            strict_config(" Device ", "65535", false).unwrap(),
            json!({"enabled":false,"deviceAlias":"Device","port":65535})
        );
    }
}

impl LanManagementPage {
    pub(super) fn has_target(&self) -> bool {
        conversation_has_target(&self.peers, &self.selected)
    }
    pub(super) fn composing(&self, cx: &gpui::App) -> bool {
        self.composer.read(cx).is_composing()
            || self.alias.read(cx).is_composing()
            || self.port.read(cx).is_composing()
            || self
                .codes
                .values()
                .any(|input| input.read(cx).is_composing())
    }
    fn icon(&self, name: &str, size: f32, t: &ThemeTokens) -> AnyElement {
        self.icon_with_color(name, size, color(&t.text_secondary))
    }
    fn device_tile(&self, icon: &str, online: Option<bool>, t: &ThemeTokens) -> AnyElement {
        let tile = div().relative().size(px(32.)).flex_none().flex().items_center().justify_center().rounded(px(7.))
            .bg(motion::mix_hsla(color(&t.background), color(&t.neutral_active), 0.6))
            .child(self.icon(icon, 18., t));
        match online {
            // 8 px status dot with a 2 px ring cut out of the card surface.
            Some(online) => tile.child(div().absolute().right(px(-4.)).bottom(px(-4.)).size(px(12.)).rounded_full()
                .border_2().border_color(color(&t.surface))
                .bg(if online { motion::mix_hsla(color(&t.background), color(&t.text), 0.6) } else { color(&t.text_muted) })),
            None => tile,
        }
        .into_any_element()
    }
    fn icon_with_color(&self, name: &str, size: f32, foreground: gpui::Hsla) -> AnyElement {
        let Some(path) = self
            .tokens
            .icons
            .get(name)
            .or_else(|| self.tokens.icons.get("desktop"))
        else {
            return div().size(px(size)).into_any_element();
        };
        svg()
            .path(path.clone())
            .size(px(size))
            .flex_none()
            .text_color(foreground)
            .into_any_element()
    }
    pub(super) fn discard_compose_files(&self, paths: Vec<String>) {
        if paths.is_empty() || self.fixture {
            return;
        }
        let (_, receiver) =
            self.writes
                .submit(self.host.clone(), Action::DiscardCompose(paths), None);
        drop(receiver); // Accepted by the same FIFO and included in the exit barrier.
    }
    fn remove_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.files.retain(|old| old != path);
        self.invalidate_image(path);
        self.owned_files.remove(path);
        self.composer_revision = self.composer_revision.wrapping_add(1);
        self.discard_empty_failed_send(cx);
        self.discard_compose_files(vec![path.into()]);
        cx.notify();
    }
    fn remove_recent(&mut self, id: &str, cx: &mut Context<Self>) {
        let files = self
            .recent_items
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.files.clone())
            .unwrap_or_default();
        self.recent_items.retain(|item| item.id != id);
        for path in files {
            if !self
                .recent_items
                .iter()
                .any(|item| item.files.contains(&path))
            {
                self.remove_file(&path, cx);
            }
        }
        self.composer_revision = self.composer_revision.wrapping_add(1);
        self.discard_empty_failed_send(cx);
        cx.notify();
    }
    fn begin_compose(&mut self, recent: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.exit_pending || self.fixture || self.composing(cx) {
            return;
        }
        self.composer.read(cx).focus_handle().focus(window);
        let target = self.composer.read(cx).paste_target();
        self.paste_target = Some((target, self.selected.clone(), self.lifetime));
        self.run(
            if recent {
                Action::ComposeRecent
            } else {
                Action::ComposeClipboard
            },
            window,
            cx,
        );
    }
    pub(super) fn open_devices(&mut self, sessions: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        if !sessions && !self.fixture {
            self.host.show_web_route("/main/settings/devices");
            return;
        }
        self.previous_focus = window.focused(cx);
        self.devices = !sessions;
        self.sessions = sessions;
        if !sessions {
            self.pairing_refresh.opened();
            self.renew_pairing_code(window, cx);
        }
        self.attach_menu = false;
        self.overlay_focus.focus(window);
        cx.notify();
    }
    fn dismiss_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        self.devices = false;
        self.sessions = false;
        self.forget_confirm = None;
        if let Some(focus) = self.previous_focus.take() {
            focus.focus(window);
        }
        cx.notify();
    }
    pub(super) fn dismiss_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        self.preview = None;
        if let Some(focus) = self.previous_focus.take() {
            focus.focus(window);
        }
        cx.notify();
    }
    pub(super) fn control(
        &mut self,
        id: impl Into<String>,
        label: impl Into<SharedString>,
        enabled: bool,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
        callback: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        let id = id.into();
        let kind = button_kind(&id);
        let modal = self.devices || self.sessions || self.preview.is_some();
        let interactive = enabled && !self.exit_pending && !self.settings_leave_locked
            && (!modal || id.starts_with("overlay:"));
        let focus = self
            .focus_handles
            .entry(id.clone())
            .or_insert_with(|| cx.focus_handle())
            .clone()
            .tab_stop(interactive);
        if interactive && id.starts_with("overlay:") {
            self.overlay_tab_order.push(focus.clone());
        }
        let focused = self.focus_visible && focus.is_focused(window);
        let mut hover_progress = self.transition(
            format!("hover:{id}"),
            interactive && self.hovered_controls.contains(&id),
            motion::HOVER,
            window,
        );
        if !interactive {
            self.hovered_controls.remove(&id);
            hover_progress = 0.;
        }
        let focus_progress = self.transition(format!("focus:{id}"), focused, motion::FOCUS, window);
        let mut background = gpui::transparent_black();
        let mut border = gpui::transparent_black();
        let mut foreground = color(&t.text_secondary);
        let (height, horizontal, vertical, radius) = match kind {
            ButtonKind::Secondary | ButtonKind::Primary | ButtonKind::Danger => (28., 8., 4., 6.),
            ButtonKind::Flat | ButtonKind::Unpair => (24., 6., 2., 5.),
            ButtonKind::Compact => (20., 4., 0., 5.),
            ButtonKind::FileAction => (24., 5., 2., 5.),
            ButtonKind::Copy => (24., 6., 2., 5.),
            ButtonKind::Latest => (28., 10., 4., 14.),
            ButtonKind::Remove => (20., 0., 0., 10.),
            ButtonKind::Image => (0., 0., 0., 0.),
            _ => (28., 0., 0., 6.),
        };
        if matches!(kind, ButtonKind::Secondary | ButtonKind::Copy | ButtonKind::Latest) {
            background = color(&t.surface);
            border = color(&t.border);
            if kind == ButtonKind::Secondary { foreground = color(&t.text); }
        }
        if kind == ButtonKind::Danger {
            background = motion::mix_hsla(color(&t.surface), color(&t.error), 0.07);
            border = motion::mix_hsla(color(&t.border), color(&t.error), 0.38);
            foreground = color(&t.error);
        }
        if matches!(kind, ButtonKind::Send | ButtonKind::Primary) {
            background = color(&t.text);
            foreground = color(&t.background);
        }
        // Unpair stays muted and turns to the error colour while hovered or confirming.
        if kind == ButtonKind::Unpair {
            let confirming = id.strip_prefix("overlay:forget:").is_some_and(|peer| self.forget_confirm.as_deref() == Some(peer));
            let progress = if confirming { 1. } else { hover_progress };
            foreground = motion::mix_hsla(color(&t.text_muted), color(&t.error), progress);
            background = color(&t.error).opacity(0.09 * progress);
        }
        if kind == ButtonKind::Switch {
            let on = self.sync_toggle_value(&id).unwrap_or(self.status["enabled"] == true);
            let surface = if id.starts_with("overlay:sync:") { format!("surface:{id}") } else { "surface:sharing".into() };
            let progress = self.transition(surface, on, motion::SURFACE, window);
            background = motion::mix_hsla(color(&t.border_secondary), color(&t.accent), progress);
        }
        if kind == ButtonKind::Conversation {
            let conversation = id.strip_prefix("overlay:conversation:").or_else(|| id.strip_prefix("conversation:")).unwrap_or_default();
            let selected = conversation == self.selected;
            let progress = self.transition(format!("surface:{id}"), selected, motion::SURFACE, window);
            background = motion::mix_hsla(gpui::transparent_black(), color(&t.neutral_active), progress);
            border = motion::mix_hsla(gpui::transparent_black(), color(&t.border), progress);
            if selected { foreground = color(&t.text); }
        }
        if kind == ButtonKind::Segment {
            if let Some((peer, value)) = id.strip_prefix("overlay:kind:").and_then(|id| id.rsplit_once(':')) {
                let profile = if peer == "local" { Some(&self.status) } else { self.peers.iter().find(|item| item["deviceId"] == peer) };
                let selected = profile.and_then(|profile| profile["deviceKindOverride"].as_str()).unwrap_or("auto") == value;
                let progress = self.transition(format!("surface:kind:{peer}:{value}"), selected, motion::SURFACE, window);
                background = motion::mix_hsla(gpui::transparent_black(), color(&t.neutral_active), progress);
                foreground = motion::mix_hsla(color(&t.text_muted), color(&t.text), progress);
            }
        }
        let hover_background = match kind {
            ButtonKind::Send => motion::mix_hsla(color(&t.text), color(&t.background), 0.12),
            ButtonKind::Primary => motion::mix_hsla(color(&t.text), color(&t.background), 0.14),
            ButtonKind::Unpair => background,
            ButtonKind::Danger => motion::mix_hsla(color(&t.surface), color(&t.error), 0.13),
            ButtonKind::Switch => motion::mix_hsla(background, color(&t.text), 0.12),
            ButtonKind::Image => background,
            _ => color(&t.hover),
        };
        let pressed = match kind {
            ButtonKind::Send | ButtonKind::Primary => color(&t.text_secondary),
            ButtonKind::Danger => motion::mix_hsla(color(&t.surface), color(&t.error), 0.20),
            ButtonKind::Switch => motion::mix_hsla(background, color(&t.text), 0.22),
            ButtonKind::Image => background,
            _ => color(&t.neutral_active),
        };
        let hover_border = if kind == ButtonKind::Danger {
            motion::mix_hsla(color(&t.border), color(&t.error), 0.62)
        } else if matches!(kind, ButtonKind::Secondary | ButtonKind::Copy | ButtonKind::Latest) {
            color(&t.border_secondary)
        } else { border };
        let background = motion::mix_hsla(background, hover_background, hover_progress);
        let border = motion::mix_hsla(motion::mix_hsla(border, hover_border, hover_progress), color(&t.accent), focus_progress);
        if !matches!(kind, ButtonKind::Send | ButtonKind::Primary | ButtonKind::Unpair | ButtonKind::Danger | ButtonKind::Switch | ButtonKind::Image) {
            foreground = motion::mix_hsla(foreground, color(&t.text), hover_progress);
        }
        let hover_id = id.clone();
        let click_id = id.clone();
        let callback = Arc::new(callback);
        let click_focus = focus.clone();
        let label: SharedString = label.into();
        div()
            .id(SharedString::from(id))
            .track_focus(&focus)
            .px(px(horizontal))
            .py(px(vertical))
            .min_h(px(height))
            .rounded(px(radius))
            .border_1()
            .when(matches!(kind, ButtonKind::Icon | ButtonKind::Remove | ButtonKind::Image | ButtonKind::Switch | ButtonKind::Send | ButtonKind::Segment), |button| button.border_0())
            .border_color(border)
            .bg(background)
            .text_size(px(11.))
            .line_height(px(18.))
            .when(matches!(kind, ButtonKind::Secondary | ButtonKind::Primary | ButtonKind::Danger), |button| button.font_weight(FontWeight::MEDIUM))
            .text_color(foreground)
            .opacity(if enabled { 1. } else { 0.45 })
            .when(interactive, |button| button.cursor_pointer().active(move |style| style.bg(pressed)))
            .when(!interactive, |button| button.cursor_default())
            .when(focus_progress > 0., |button| button.shadow(vec![gpui::BoxShadow {
                color: color(&t.accent).opacity(focus_progress), offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(0.), spread_radius: px(1.),
            }]))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let changed = if *hovered && interactive {
                    this.hovered_controls.insert(hover_id.clone())
                } else {
                    this.hovered_controls.remove(&hover_id)
                };
                if changed {
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                if interactive {
                    click_focus.focus(window);
                }
            })
            .on_click(cx.listener(move |this, event, window, cx| {
                let modal = this.devices || this.sessions || this.preview.is_some();
                if interactive && !this.exit_pending && !this.settings_leave_locked && (!modal || click_id.starts_with("overlay:"))
                    && !(matches!(event, gpui::ClickEvent::Keyboard(_)) && this.composing(cx)) {
                    cx.stop_propagation();
                    callback(this, window, cx);
                }
            }))
            // Empty text still owns a line in GPUI block layout. Icon-only
            // controls and image previews must not acquire that extra row.
            .when(!label.is_empty(), |button| button.child(label))
    }
    /// Compact segmented picker: "自动" stays a word, explicit kinds are icon-only with tooltips.
    fn icon_picker(
        &mut self,
        id: &str,
        profile: &Value,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detected = profile["detectedDeviceKind"].as_str().unwrap_or("unknown");
        let mut options = div().flex_none().flex().gap(px(1.)).p(px(2.)).rounded(px(7.))
            .border_1().border_color(color(&t.border)).bg(color(&t.background));
        for (index, value) in ["auto", "desktop", "laptop", "phone"].into_iter().enumerate() {
            let peer = id.to_string();
            let keyboard_peer = peer.clone();
            let hint = if value == "auto" {
                format!("{}（{}）", tr("自动识别", "Detect automatically"), device_label(detected))
            } else {
                format!("{}{}", tr("显示为", "Show as "), device_label(value))
            };
            let control = self.control(
                format!("overlay:kind:{id}:{value}"),
                if value == "auto" { tr("自动", "Auto") } else { "" },
                !self.busy,
                t,
                window,
                cx,
                move |this, window, cx| {
                    let params = if peer == "local" {
                        json!({"deviceKindOverride":value})
                    } else {
                        json!({"peerDeviceKindOverrides":{peer.clone():value}})
                    };
                    this.run(Action::Config(params), window, cx);
                },
            )
            .h(px(24.))
            .min_h(px(24.))
            .min_w(px(26.))
            .px(px(if value == "auto" { 7. } else { 5. }))
            .py(px(0.))
            .flex()
            .items_center()
            .justify_center()
            .tooltip(tooltip(hint, self.tokens.clone(), self.dark))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if this.busy
                    || this.composing(cx)
                    || !matches!(event.keystroke.key.as_str(), "left" | "right" | "up" | "down")
                {
                    return;
                }
                let next = if matches!(event.keystroke.key.as_str(), "left" | "up") {
                    (index + 3) % 4
                } else {
                    (index + 1) % 4
                };
                let value = ["auto", "desktop", "laptop", "phone"][next];
                let params = if keyboard_peer == "local" {
                    json!({"deviceKindOverride":value})
                } else {
                    json!({"peerDeviceKindOverrides":{keyboard_peer.clone():value}})
                };
                this.run(Action::Config(params), window, cx);
                if let Some(focus) = this.focus_handles.get(&format!("overlay:kind:{keyboard_peer}:{value}")) {
                    focus.focus(window);
                }
                cx.stop_propagation();
            }));
            // Icon colour follows the segment's own foreground transition.
            options = options.child(if value == "auto" {
                control
            } else {
                let selected = profile["deviceKindOverride"].as_str().unwrap_or("auto") == value;
                let tint = if selected { color(&t.text) } else { color(&t.text_muted) };
                control.child(self.icon_with_color(value, 15., tint))
            });
        }
        options.into_any_element()
    }

    fn conversation_rows(
        &mut self,
        overlay: bool,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefix = if overlay { "overlay:" } else { "" };
        let mut rows = rsx! {
            <div flex flex_col gap={px(4.)} p={px(7.)} />
        };
        let mut conversations = self.conversations.clone();
        if !conversations.iter().any(|v| v["id"] == "all") {
            conversations.insert(
                0,
                json!({"id":"all","kind":"group","title":tr("所有设备","All devices")}),
            );
        }
        for conversation in conversations {
            let id = text(&conversation, "id");
            let group = id == "all" || conversation["kind"] == "group";
            let paired = self.peers.iter().filter(|p| manual_transfer_peer(p)).count();
            let peer = conversation_peer(&self.peers, &id)
                .cloned()
                .unwrap_or(Value::Null);
            let kind = if group { "all" } else { device_icon(&peer) };
            let title = text(&conversation, "title");
            let detail = if group {
                format!("{paired} {}", tr("台已配对设备", "paired devices"))
            } else if conversation["online"] == true {
                tr("在线", "Online").into()
            } else {
                tr("离线", "Offline").into()
            };
            let icon = self.icon(kind, 20., t);
            let icon = rsx! {
                <div relative w={px(25.)} h={px(22.)} flex_none flex items_center justify_center>
                    {icon}
                </div>
            }
                .when(!group, |v| {
                    v.child(
                        rsx! {
                            <div absolute right={px(0.)} bottom={px(0.)}>
                                {self.dot(conversation["online"] == true, t)}
                            </div>
                        },
                    )
                });
            let unread = conversation["unreadCount"].as_u64().unwrap_or(0);
            rows = rows.child(
                self.control(
                    format!("{prefix}conversation:{id}"),
                    "",
                    true,
                    t,
                    window,
                    cx,
                    move |this, window, cx| {
                        this.select(id.clone(), window, cx);
                        if overlay {
                            this.dismiss_devices(window, cx);
                        }
                    },
                )
                .w_full()
                .min_h(px(58.))
                .px(px(9.))
                .py(px(8.))
                .rounded(px(7.))
                .flex()
                .items_center()
                .gap(px(9.))
                .child(icon)
                .child(
                    rsx! {
                        <div flex_1 min_w_0 flex flex_col gap={px(2.)}>
                            <div text_size={px(13.)} text_ellipsis>
                                {title}
                            </div>
                            <div text_size={px(11.)} text_color={color(&t.text_muted)}>
                                {detail}
                            </div>
                        </div>
                    },
                )
                .when(unread > 0, |v| {
                    v.child(div().text_size(px(11.)).child(unread.to_string()))
                }),
            );
        }
        rows.into_any_element()
    }
    fn sidebar_view(
        &mut self,
        overlay: bool,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefix = if overlay { "overlay:" } else { "" };
        let rows = self.conversation_rows(overlay, t, window, cx);
        let icon = self.icon(device_icon(&self.status), 16., t);
        let add = self.control(
            format!("{prefix}add-device"),
            tr("＋ 添加设备", "＋ Add device"),
            true,
            t,
            window,
            cx,
            |this, window, cx| this.open_devices(false, window, cx),
        );
        let scroll = div().id(if overlay { "lan-session-modal-scroll" } else { "lan-session-scroll" })
            .flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.session_scroll)
            .smooth_scroll("lan-sessions", cx)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if *hovered { this.hovered_controls.insert("area:sessions".into()); }
                else { this.hovered_controls.remove("area:sessions"); }
                cx.notify();
            }))
            .child(rows);
        let weak = cx.weak_entity();
        let scrollbar = self.session_scrollbar.render(
            "lan-session-scrollbar", ScrollbarTarget::Scroll(self.session_scroll.clone()), self.lifetime,
            self.lan_scrollbar_style(t, self.hovered_controls.contains("area:sessions")
                || (overlay && self.overlay_focus.contains_focused(window, cx))),
            Rc::new(move |_, cx| { let _ = weak.update(cx, |_, cx| cx.notify()); }),
        );
        rsx! {
            <div w={px(SIDEBAR)} max_w_full h_full min_h_0 flex_none flex flex_col border_r_1
                border_color={color(&t.border)}>
                <div h={px(43.)} flex_none px={px(10.)} flex items_center justify_between
                    text_size={px(11.)} text_color={color(&t.text_muted)}>
                    {tr("设备", "Devices")}
                    {add}
                </div>
            </div>
        }
            .when(overlay, |v| v.w_full().border_r_0())
            .child(
                div().relative().flex_1().min_h_0().w_full().flex().flex_col().child(scroll).child(scrollbar),
            )
            .child(
                rsx! {
                    <div px={px(15.)} py={px(12.)} flex_none border_t_1
                        border_color={color(&t.border)} flex items_center gap={px(8.)}>
                        {icon}
                        <div flex_1 min_w_0>
                            <div text_size={px(13.)} text_ellipsis>
                                {text(&self.status, "deviceAlias")}
                            </div>
                            <div text_size={px(11.)} text_color={color(&t.text_muted)}>
                                {tr("此设备", "This device")}
                            </div>
                        </div>
                    </div>
                },
            )
            .into_any_element()
    }
}

impl LanManagementPage {
    fn devices_view(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = self.status["enabled"] == true;
        let mut body = div()
            .id("lan-devices-scroll")
            .when(!self.settings_only, |body| body.flex_1().min_h_0().overflow_y_scroll()
                .track_scroll(&self.device_scroll).smooth_wheel("lan-devices-scroll", &self.device_scroll, cx))
            .when(self.settings_only, |body| body.w_full().min_w_0().flex_none())
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if *hovered { this.hovered_controls.insert("area:devices".into()); }
                else { this.hovered_controls.remove("area:devices"); }
                cx.notify();
            }))
            .p(px(if self.settings_only { 0. } else { 16. }))
            .flex()
            .flex_col()
            .gap(px(20.));
        if self.sessions {
            let rows = self.sidebar_view(true, t, window, cx);
            body = body.p(px(0.)).child(rsx! {
                <div h_full w_full>
                    {rows}
                </div>
            });
        } else {
            // Grouped cards with one row shape (tile / two text lines / trailing action),
            // matching the confirmed `GpuiAlignmentLanStage` drawer.
            let running = self.status["running"] == true;
            let failed = enabled && !running && !text(&self.status, "error").is_empty();
            let pad = if self.companion { 6. } else { 8. };
            let drawer_width = if self.settings_only { f32::from(window.viewport_size().width) - 256. }
                else { (f32::from(window.viewport_size().width) - pad * 2.).min(398.) };
            let narrow = drawer_width - 32. <= 340.;
            if self.companion || self.settings_only {
                let toggle = self.control(
                    "overlay:sharing-switch", "", !self.busy, t, window, cx,
                    move |this, window, cx| this.run(Action::Config(json!({"enabled":!enabled})), window, cx),
                )
                    .w(px(if self.settings_only { 32. } else { 29. }))
                    .h(px(if self.settings_only { 18. } else { 16. }))
                    .min_h(px(if self.settings_only { 18. } else { 16. })).p(px(2.)).rounded_full().border_0()
                    .flex().items_center().when(enabled, |v| v.justify_end())
                    .bg(if enabled { color(&t.accent) } else { color(&t.border_secondary) })
                    .tooltip(tooltip(if enabled { tr("关闭局域网互传", "Disable LAN sharing") } else { tr("开启局域网互传", "Enable LAN sharing") }.into(), self.tokens.clone(), self.dark))
                    .child(div().size(px(if self.settings_only { 14. } else { 12. })).rounded_full().bg(color(&t.surface)));
                let (tint, detail) = if running {
                    (Some(color(&t.accent)), tr("已开启 · 附近设备可以发现此设备", "On · Nearby devices can find this one").to_string())
                } else if failed {
                    (Some(color(&t.error)), format!("{} · {}", tr("启动失败", "Failed to start"), text(&self.status, "error")))
                } else {
                    (None, tr("已关闭 · 其他设备无法发现此设备", "Off · Other devices can’t find this one").to_string())
                };
                // Tint by alpha over the surface: an HSLA mix from neutral grey
                // (hue 0) to the accent would sweep through magenta.
                let (border, wash) = match tint {
                    _ if self.settings_only && !failed => (color(&t.border), gpui::transparent_black()),
                    Some(tint) => (tint.opacity(if failed { 0.40 } else { 0.35 }), tint.opacity(if failed { 0.06 } else { 0.07 })),
                    None => (color(&t.border), gpui::transparent_black()),
                };
                let mut text_column = row_text(tr("局域网互传", "LAN sharing"), detail, t);
                if failed { text_column = text_column.text_color(color(&t.error)); }
                body = body.child(
                    div().flex_none().rounded(px(8.)).border_1().border_color(border).bg(color(&t.surface)).overflow_hidden()
                        .child(div().flex().items_center().gap(px(12.)).pl(px(14.)).pr(px(12.)).py(px(11.)).bg(wash)
                            .child(text_column).child(toggle)),
                );
            }
            if let Some(error) = self.error.clone() {
                body = body.child(div().px(px(2.)).text_size(px(11.)).text_color(color(&t.error)).child(error));
            }
            if self.settings_only {
                if let Some(content) = self.settings_content.clone() {
                    body = body.child(content);
                }
            } else {
                body = body.child(self.sync_global_card(t, window, cx));
            }

            // This device: identity row + pairing code.
            let profile = self.status.clone();
            let picker = self.icon_picker("local", &profile, t, window, cx);
            let code = &self.status["pairingCode"];
            let seconds = pairing_seconds(&self.status);
            let live = running && seconds.is_some_and(|seconds| seconds > 0)
                && (self.fixture || pairing_code_valid(&self.status, chrono::Utc::now()));
            let digits: Vec<char> = text(code, "code").chars().filter(char::is_ascii_digit).collect();
            let timer = if !running {
                tr("开启互传后显示", "Shown once sharing is on").to_string()
            } else if let Some(seconds) = seconds {
                if seconds == 0 { tr("正在更新…", "Updating…").to_string() }
                else { format!("{}:{:02} {}", seconds / 60, seconds % 60, tr("后自动更新", "until the next code")) }
            } else {
                tr("正在更新…", "Updating…").to_string()
            };
            let mut boxes = div().flex().gap(px(4.));
            for index in 0..6 {
                let digit = if live { digits.get(index).map(|d| d.to_string()).unwrap_or_default() } else { String::new() };
                boxes = boxes.child(
                    div().w(px(30.)).h(px(38.)).when(index == 3, |v| v.ml(px(8.)))
                        .flex().items_center().justify_center().rounded(px(6.)).border_1()
                        .border_color(if live { color(&t.border) } else { color(&t.border_secondary) })
                        .when(live, |v| v.bg(color(&t.surface)))
                        .font_family("Consolas").text_size(px(20.)).font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(&t.text)).child(digit),
                );
            }
            let mut code_block = div().p(px(12.)).flex().flex_col()
                .border_t_1().border_color(color(&t.border))
                .bg(motion::mix_hsla(color(&t.surface), color(&t.background), 0.45))
                .child(div().mb(px(8.)).flex().justify_between().gap(px(8.)).text_size(px(11.))
                    .text_color(color(&t.text_muted))
                    .child(tr("配对码", "Pairing code"))
                    .child(div().text_color(if live { color(&t.text_muted) } else { color(&t.text_secondary) }).child(timer)))
                .child(div().flex().items_center().gap(px(10.)).child(boxes));
            if let (true, Some(seconds)) = (live, seconds) {
                let fraction = (seconds as f32 / PAIRING_TTL_SECONDS).clamp(0., 1.);
                code_block = code_block.child(
                    div().mt(px(10.)).h(px(2.)).w_full().rounded(px(2.)).bg(color(&t.border))
                        .child(div().h_full().w(gpui::relative(fraction)).rounded(px(2.)).bg(color(&t.text_muted))),
                );
            }
            let alias = text(&self.status, "deviceAlias");
            code_block = code_block.child(note(
                if running {
                    format!("{}「{alias}」{}", tr("在另一台设备上选择", "On the other device, choose "), tr("，然后输入此码。", ", then enter this code."))
                } else {
                    tr("开启互传后，其他设备才能找到并添加此设备。", "Turn sharing on so other devices can find and add this one.").into()
                },
                9., t,
            ));
            let local_row = device_row()
                .child(self.device_tile(device_icon(&self.status), None, t))
                .child(row_text(alias.clone(), tr("其他设备将看到此名称", "Other devices see this name"), t))
                .child(picker);
            body = body.child(section(heading(tr("此设备", "This device"), None, t), card(t).child(local_row).child(code_block)));

            // Nearby devices: pairing expands under the chosen row.
            let scan = enabled.then(|| self.control("overlay:scan", "", enabled && !self.busy, t, window, cx,
                |this, window, cx| this.run(Action::Scan, window, cx))
                .flex().items_center().gap(px(4.))
                .child(self.icon("refresh", 13., t)).child(tr("重新查找", "Find again")));
            let nearby_peers = self.peers.iter().filter(|p| p["paired"] != true).cloned().collect::<Vec<_>>();
            let mut nearby_card = card(t);
            if !enabled || nearby_peers.is_empty() {
                let action = if !enabled {
                    self.control("overlay:enable-sharing", tr("开启互传", "Turn on sharing"), !self.busy, t, window, cx,
                        |this, window, cx| this.run(Action::Config(json!({"enabled":true})), window, cx))
                } else {
                    self.control("overlay:find-devices", tr("查找设备", "Find devices"), !self.busy, t, window, cx,
                        |this, window, cx| this.run(Action::Scan, window, cx))
                };
                nearby_card = nearby_card.child(
                    div().py(px(18.)).px(px(14.)).flex().flex_col().items_center().gap(px(8.))
                        .text_size(px(11.)).text_color(color(&t.text_secondary))
                        .child(if enabled { tr("还没有找到附近设备。", "No nearby devices found yet.") }
                            else { tr("先开启互传，再查找附近设备。", "Turn sharing on to find nearby devices.") })
                        .child(action),
                );
            }
            // While sharing is off the prompt replaces the list; peers come back once it runs.
            for (index, peer) in nearby_peers.into_iter().enumerate().filter(|_| enabled) {
                let id = text(&peer, "deviceId");
                let name = text(&peer, "alias");
                let online = peer["online"] == true;
                let available = enabled && online && peer["pairingAvailable"].as_bool().unwrap_or(true);
                let pairing = self.pair_target.as_deref() == Some(id.as_str());
                let detail = if !online { tr("离线，等待设备上线", "Offline, waiting for the device") }
                    else if available { tr("可配对", "Available to pair") }
                    else { tr("对方尚未开启配对", "Pairing isn’t open on that device") };
                let mut row = device_row()
                    .child(self.device_tile(device_icon(&peer), None, t))
                    .child(row_text(name.clone(), detail, t));
                if !available { row = row.opacity(0.55); }
                if available && !pairing {
                    let pair_id = id.clone();
                    row = row.child(self.control(format!("overlay:begin-pair:{id}"), tr("配对", "Pair"), !self.busy, t, window, cx,
                        move |this, _, cx| { this.pair_target = Some(pair_id.clone()); this.error = None; cx.notify(); }));
                }
                let mut group = div().flex().flex_col().when(index > 0, |v| v.border_t_1().border_color(color(&t.border)))
                    .when(pairing, |v| v.bg(motion::mix_hsla(color(&t.surface), color(&t.background), 0.45)))
                    .child(row);
                if pairing {
                    if let Some(input) = self.codes.get(&id).cloned() {
                        self.overlay_tab_order.push(input.read(cx).focus_handle());
                        let valid = pairing_code(input.read(cx).text()).len() == 6;
                        let pair_input = input.clone();
                        let pair_id = id.clone();
                        let confirm = self.control(format!("overlay:confirm-pair:{id}"), tr("配对", "Pair"),
                            valid && available && !self.busy, t, window, cx,
                            move |this, window, cx| {
                                if pair_input.read(cx).is_composing() { return; }
                                let code = pairing_code(pair_input.read(cx).text());
                                if code.len() == 6 { this.run(Action::Pair(pair_id.clone(), code), window, cx); }
                            });
                        let cancel_id = id.clone();
                        let cancel = self.control("overlay:cancel-pair", tr("取消", "Cancel"), !self.busy, t, window, cx,
                            move |this, _, cx| {
                                this.settings_write_errors.remove(&format!("pair:{cancel_id}"));
                                this.pair_target = None;
                                cx.notify();
                            });
                        group = group.child(
                            div().pl(px(if narrow { 12. } else { 54. })).pr(px(12.)).pb(px(12.)).flex().flex_col().gap(px(7.))
                                .child(div().text_size(px(11.)).text_color(color(&t.text_secondary))
                                    .child(format!("{}{name}{}", tr("输入", "Enter "), tr("屏幕上的 6 位配对码", "’s 6-digit pairing code"))))
                                .child(div().flex().items_center().gap(px(6.))
                                    .child(div().w(px(132.)).min_w_0().child(input)).child(confirm).child(cancel)),
                        );
                    }
                }
                nearby_card = nearby_card.child(group);
            }
            body = body.child(
                section(heading(tr("添加附近设备", "Add a nearby device"), None, t).children(scan), nearby_card)
                    .child(note(tr("找不到设备？确认两台设备在同一局域网、对方已开启互传，并允许 ClipHost 通过防火墙。",
                        "Can’t find a device? Check both are on the same LAN with sharing on, and that the firewall allows ClipHost."), 8., t)),
            );

            // Paired devices.
            let peers = self.peers.iter().filter(|p| p["paired"] == true).cloned().collect::<Vec<_>>();
            let count = (!peers.is_empty()).then(|| peers.len().to_string());
            let mut paired_card = card(t).w_full().flex_none();
            if peers.is_empty() {
                paired_card = paired_card.child(div().py(px(18.)).px(px(14.)).flex().justify_center()
                    .text_size(px(11.)).text_color(color(&t.text_secondary))
                    .child(tr("尚未配对设备，先从附近设备中添加。", "No paired devices yet. Add one from nearby devices.")));
            }
            for (index, peer) in peers.into_iter().enumerate() {
                let id = text(&peer, "deviceId");
                let name = text(&peer, "alias");
                let online = peer["online"] == true;
                let forget_id = id.clone();
                let forget = self.control(format!("overlay:forget:{id}"), tr("解除配对", "Unpair"), !self.busy, t, window, cx,
                    move |this, _, cx| {
                        this.forget_confirm = if this.forget_confirm.as_deref() == Some(forget_id.as_str()) { None } else { Some(forget_id.clone()) };
                        cx.notify();
                    });
                let picker = self.icon_picker(&id, &peer, t, window, cx);
                let row = device_row()
                    .child(self.device_tile(device_icon(&peer), Some(online && running), t))
                    .child(row_text(name.clone(), if online { tr("在线", "Online") } else { tr("离线", "Offline") }, t));
                // Narrow drawers move the picker under the name instead of squeezing it.
                let mut group = div().w_full().min_w_0().flex_none().flex().flex_col().when(index > 0, |v| v.border_t_1().border_color(color(&t.border)));
                group = if narrow {
                    group.child(row.child(forget)).child(div().pl(px(54.)).pb(px(10.)).mt(px(-4.)).flex().child(picker))
                } else {
                    group.child(row.child(picker).child(forget))
                };
                if self.forget_confirm.as_deref() == Some(id.as_str()) {
                    let confirm_id = id.clone();
                    let confirm = self.control(format!("overlay:forget-confirm:{id}"), tr("确认解除", "Unpair"), !self.busy, t, window, cx,
                        move |this, window, cx| this.run(Action::Forget(confirm_id.clone()), window, cx));
                    let cancel_id = id.clone();
                    let cancel = self.control(format!("overlay:forget-cancel:{id}"), tr("取消", "Cancel"), true, t, window, cx,
                        move |this, _, cx| {
                            this.settings_write_errors.remove(&format!("forget:{cancel_id}"));
                            this.forget_confirm = None;
                            cx.notify();
                        });
                    group = group.child(
                        div().pl(px(if narrow { 12. } else { 54. })).pr(px(12.)).pb(px(12.)).flex().flex_col().gap(px(8.))
                            .child(div().text_size(px(11.)).line_height(px(18.)).text_color(color(&t.text_secondary)).child(format!(
                                "{}{name}{}", tr("解除与", "Unpair "), tr("的配对？历史消息会保留，之后发送需重新配对。", "? History stays; you’ll need to pair again to send."))))
                            .child(div().flex().justify_end().gap(px(6.)).child(cancel).child(confirm)),
                    );
                }
                if !self.settings_only {
                    group = group.child(self.sync_peer_status(&id, t));
                }
                paired_card = paired_card.child(group);
            }
            body = body.child(section(heading(tr("已配对设备", "Paired devices"), count, t), paired_card));
            if !self.settings_only {
                body = body.child(self.received_files_card(t, window, cx));
            }

            // Advanced settings, collapsed into its own card.
            let advanced_open = self.advanced;
            let summary = self.control("overlay:advanced", "", true, t, window, cx,
                |this, _, cx| { this.advanced = !this.advanced; cx.notify(); })
                .w_full().min_h(px(40.)).px(px(12.)).rounded(px(0.)).border_0()
                .flex().items_center().gap(px(8.)).text_size(px(12.))
                .child(self.icon("gear", 15., t))
                .child(div().flex_1().child(tr("高级连接设置", "Advanced connection settings")))
                .child(self.icon_with_color(if advanced_open { "chevron-down" } else { "chevron-right" }, 13., color(&t.text_muted)));
            // Clipped cards must not shrink inside the fixed-height scroll column.
            let mut settings = card(t).flex_none().child(summary);
            if advanced_open {
                self.overlay_tab_order.push(self.alias.read(cx).focus_handle());
                self.overlay_tab_order.push(self.port.read(cx).focus_handle());
                let save = self.control("overlay:save-settings", tr("保存", "Save"), !self.busy, t, window, cx,
                    |this, window, cx| {
                        if this.composing(cx) { return; }
                        match strict_config(this.alias.read(cx).text(), this.port.read(cx).text(), this.status["enabled"] == true) {
                            Ok(params) => this.run(Action::Config(params), window, cx),
                            Err(error) => this.report_error(error, cx),
                        }
                    });
                let field = |label: &'static str, input: AnyElement| div().flex().flex_col().gap(px(5.))
                    .text_size(px(11.)).text_color(color(&t.text_secondary)).child(label).child(input);
                settings = settings.child(
                    div().p(px(12.)).border_t_1().border_color(color(&t.border)).flex().flex_col().gap(px(10.))
                        .child(field(tr("本机设备名称", "This device’s name"), self.alias.clone().into_any_element()))
                        .child(field(tr("监听端口", "Listening port"), self.port.clone().into_any_element()))
                        .child(div().text_size(px(11.)).text_color(color(&t.text_muted))
                            .child(tr("设备名称供附近设备识别。", "Nearby devices identify this one by its name.")))
                        .child(div().flex().justify_end().child(save)),
                );
            }
            body = body.child(settings);
        }
        if self.settings_only {
            return body.into_any_element();
        }
        let close = self
            .control(
                "overlay:close",
                "",
                true,
                t,
                window,
                cx,
                |this, window, cx| this.dismiss_devices(window, cx),
            )
            .size(px(28.))
            .p(px(6.))
            .child(self.icon("close", 16., t));
        let weak = cx.weak_entity();
        let scrollbar = self.device_scrollbar.render(
            "lan-device-scrollbar", ScrollbarTarget::Scroll(self.device_scroll.clone()), self.lifetime,
            self.lan_scrollbar_style(t, self.hovered_controls.contains("area:devices")
                || self.overlay_focus.contains_focused(window, cx)),
            Rc::new(move |_, cx| { let _ = weak.update(cx, |_, cx| cx.notify()); }),
        );
        let body = div().relative().flex_1().min_h_0().w_full().flex().flex_col()
            .child(body).when(!self.sessions, |v| v.child(scrollbar));
        let drawer = rsx! {
            <div id={"lan-device-drawer"} w={px(398.)} max_w_full h_full min_h_0 flex flex_col
                rounded={px(8.)} border_1 border_color={color(&t.border_secondary)}
                bg={color(&t.background)} shadow_lg occlude />
        }
            .track_focus(&self.overlay_focus)
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(
                rsx! {
                    <div h={px(43.)} flex_none px={px(16.)} flex items_center justify_between
                        border_b_1 border_color={color(&t.border)} bg={color(&t.surface)} />
                }
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(if self.sessions {
                                tr("选择设备", "Choose a device")
                            } else {
                                tr("设备与连接", "Devices and connection")
                            }),
                    )
                    .child(close),
            )
            .child(body);
        let drawer = drawer.with_animation(
            ("lan-drawer-in", self.devices_epoch.observe(true)),
            motion::enter(motion::PANEL_ENTER, motion::EASE_OUT),
            |drawer, t| drawer.opacity(t),
        );
        div()
            .absolute()
            .inset_0()
            .p(px(if self.companion { 6. } else { 8. }))
            .flex()
            .justify_end()
            .bg(gpui::rgba(0x00000028))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| { cx.stop_propagation(); this.dismiss_devices(window, cx); }),
            )
            .child(drawer)
            .into_any_element()
    }
}
