//! Category-row filter popovers (custom categories, process filters).
//! Both hang from the row's 「⋯」, right aligned, with a definite height so
//! their own header and footer stay put while the body scrolls
//! (ui-design/README.md, 2026-10-24).
use super::*;
use gpui::{AnimationExt, AnyElement};

/// Preferred popover width; narrow windows shrink it.
const WIDTH: f32 = 352.0;
/// Gap to the window edges.
const INSET: f32 = 8.0;
/// Gap below the category row.
const ANCHOR_GAP: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FilterPopoverGeometry {
    pub top: f32,
    pub right: f32,
    pub width: f32,
    pub height: f32,
}

impl FilterPopoverGeometry {
    /// `preferred` is the content height; the popover never extends past the
    /// window and keeps that height definite for the scrolling body.
    pub fn new(window_width: f32, window_height: f32, preferred: f32, tokens: &DesignTokens) -> Self {
        let l = &tokens.layout;
        let top = l.title_bar_height + l.content_padding_y + l.toolbar_height + ANCHOR_GAP;
        let available = (window_height - top - INSET).max(0.0);
        Self {
            top,
            right: INSET,
            width: WIDTH.min((window_width - INSET * 2.0).max(0.0)),
            height: preferred.max(0.0).min(available),
        }
    }
}

impl ClipboardPreview {
    pub(super) fn filter_popover_open(&self) -> bool {
        self.tag_editor.is_some() || self.process_editor.is_some() || self.process_open_pending
    }

    /// Shared shell: transparent click-away backdrop plus the card.
    pub(super) fn filter_popover_shell(
        &self,
        id: &'static str,
        preferred_height: f32,
        content: AnyElement,
        dismiss: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let size = window.viewport_size();
        let geometry = FilterPopoverGeometry::new(
            size.width.into(),
            size.height.into(),
            preferred_height,
            &tokens,
        );
        let card = rsx! {
            <div id={SharedString::from(format!("{id}-card"))} relative size_full min_h_0 flex
                flex_col overflow_hidden bg={self.popover_surface()} border_1
                border_color={color(&t.border_secondary)} rounded={px(10.0)} shadow_lg
                text_color={color(&t.text)} />
        }
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(content);
        div()
            .id(SharedString::from(format!("{id}-backdrop")))
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| dismiss(this, window, cx)),
            )
            .child(
                rsx! {
                    <div id={SharedString::from(id)} absolute top={px(geometry.top)}
                        right={px(geometry.right)} w={px(geometry.width)} h={px(geometry.height)}
                        min_h_0 />
                }
                .child(card.with_animation(
                    (id, self.filter_popover_generation),
                    crate::motion::enter(crate::motion::LIST_SETTINGS_ENTER, crate::motion::EASE_OUT),
                    |card, progress| card.opacity(progress).top(px(-4.0 * (1.0 - progress))),
                )),
            )
            .into_any_element()
    }

    /// 「自定义分类」: category tag editor.
    pub(super) fn tag_editor_view(
        &self,
        editor: Entity<FilterTabsEditor>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let preferred = editor.read(cx).preferred_height();
        let content = div()
            .size_full()
            .min_h_0()
            // Name fields need native keyboard activation in the no-focus window.
            .capture_any_mouse_down(cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                if event.button == MouseButton::Left {
                    this.engage_keyboard(window, cx);
                }
            }))
            .child(editor)
            .into_any_element();
        self.filter_popover_shell(
            "tag-editor",
            preferred,
            content,
            |this, window, cx| {
                if this.exit_pending {
                    return;
                }
                this.tag_editor = None;
                this.focus.focus(window);
                this.restore_no_focus(window);
                cx.notify();
            },
            window,
            cx,
        )
    }

    #[cfg(debug_assertions)]
    pub(super) fn filter_popover_diagnostics(&self, window: &Window, cx: &gpui::App) -> Value {
        let scroll = |handle: gpui::ScrollHandle| {
            let bounds = handle.bounds();
            json!({"offset":-f32::from(handle.offset().y),"max":f32::from(handle.max_offset().height),
                "x":f32::from(bounds.origin.x),"y":f32::from(bounds.origin.y),
                "width":f32::from(bounds.size.width),"height":f32::from(bounds.size.height)})
        };
        let size = window.viewport_size();
        let geometry = |preferred: f32| {
            let g = FilterPopoverGeometry::new(size.width.into(), size.height.into(), preferred, &self.clipboard_tokens());
            json!({"top":g.top,"right":g.right,"width":g.width,"height":g.height,"preferred":preferred})
        };
        let categories = self.tag_editor.as_ref().map(|(editor, _)| {
            let editor = editor.read(cx);
            json!({"geometry":geometry(editor.preferred_height()),"scroll":scroll(editor.body_scroll()),
                "diagnostics":editor.diagnostics(cx)})
        });
        let process = self.process_editor.as_ref().map(|(editor, _)| {
            let editor = editor.read(cx);
            json!({"geometry":geometry(crate::process_filters::PREFERRED_HEIGHT),"scroll":scroll(editor.body_scroll()),
                "diagnostics":editor.diagnostics(cx)})
        });
        json!({"categories":categories,"process":process})
    }

    /// Isolated fixture: fixed sources and one saved preset, no-op writer.
    #[cfg(debug_assertions)]
    pub fn open_process_editor_visual(
        &mut self,
        host: Arc<HostState>,
        edit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::process_filters::{PresetWriter, ProcessPreset, SourceProcess};
        assert!(
            matches!(self.model, UiModel::Fixture(_)) && cliphost_core::is_headless(),
            "isolated debug fixture only"
        );
        let presets = vec![ProcessPreset {
            id: "pf_visual".into(),
            display_name: String::new(),
            include_exe_names: vec!["chrome.exe".into(), "WeChat.exe".into()],
        }];
        self.process_presets = presets.clone();
        let now = chrono::Utc::now();
        let ago = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339();
        let samples = [
            ("chrome.exe", "Google Chrome", 1284, 0),
            ("WeChat.exe", "微信", 932, 3),
            ("Code.exe", "Visual Studio Code", 611, 12),
            ("WXWork.exe", "企业微信", 402, 95),
            ("explorer.exe", "Windows 资源管理器", 388, 130),
            ("QQ.exe", "QQ", 251, 60 * 26),
            ("WindowsTerminal.exe", "Windows Terminal", 197, 60 * 30),
            ("msedge.exe", "Microsoft Edge", 143, 60 * 24 * 3),
            ("EXCEL.EXE", "Microsoft Excel", 96, 60 * 24 * 4),
            ("Snipaste.exe", "Snipaste", 71, 60 * 24 * 6),
            ("Obsidian.exe", "Obsidian", 38, 60 * 24 * 13),
            ("Feishu.exe", "", 12, 60 * 24 * 24),
        ];
        let rows = samples
            .iter()
            .map(|(exe, _, count, minutes)| SourceProcess {
                exe_name: (*exe).into(),
                last_seen: ago(*minutes),
                item_count: *count,
            })
            .collect();
        let names = samples
            .iter()
            .filter(|(_, name, _, _)| !name.is_empty())
            .map(|(exe, name, _, _)| ((*exe).to_string(), (*name).to_string()))
            .collect();
        let writer: PresetWriter = Arc::new(|_| Ok(()));
        let (tokens, dark, selection) = (self.clipboard_tokens(), self.dark, self.process_selection.clone());
        let editing = edit.then(|| "pf_visual".to_string());
        let entity = cx.new(|cx| {
            ProcessFilterEditor::new(host, tokens, dark, presets, selection, editing, writer, window, cx)
        });
        entity.update(cx, |editor, cx| editor.seed_catalog_visual(rows, names, cx));
        let subscription = cx.subscribe_in(&entity, window, |this, _, event, window, cx| {
            if matches!(event, ProcessFiltersEvent::Saved | ProcessFiltersEvent::Cancelled) {
                this.close_process_editor(window, cx);
            }
        });
        self.process_editor = Some((entity, subscription));
        self.filter_popover_generation = self.filter_popover_generation.wrapping_add(1);
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub fn press_tag_restore_visual(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.tag_editor.as_ref().map(|(editor, _)| editor.clone()).is_some_and(|editor| {
            editor.update(cx, |editor, cx| editor.restore_defaults(window, cx))
        })
    }

    #[cfg(debug_assertions)]
    pub fn reset_filter_popover_scroll_visual(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = if let Some((editor, _)) = &self.tag_editor {
            Some(editor.read(cx).body_scroll())
        } else {
            self.process_editor.as_ref().map(|(editor, _)| editor.read(cx).body_scroll())
        };
        if let Some(handle) = handle {
            handle.set_offset(gpui::point(px(0.0), px(0.0)));
        }
        window.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popover_hangs_below_category_row_and_fits_small_windows() {
        let tokens = DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        let roomy = FilterPopoverGeometry::new(380.0, 600.0, 420.0, &tokens);
        assert_eq!(roomy.width, WIDTH);
        assert_eq!(roomy.height, 420.0);
        let l = &tokens.layout;
        assert!(roomy.top > l.title_bar_height + l.toolbar_height);
        let short = FilterPopoverGeometry::new(320.0, 420.0, 900.0, &tokens);
        assert_eq!(short.width, 320.0 - INSET * 2.0);
        assert_eq!(short.top + short.height, 420.0 - INSET);
    }
}
