//! The infrequently used LAN conversation lives beside the clipboard, in the
//! same native slot as the editor. Hiding it keeps the page and its drafts alive.

use std::sync::Arc;

use cliphost_core::{i18n::tr, HostState};
use gpui::{
    div, prelude::*, px, size, App, Bounds, Context, Entity, Subscription, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use serde_json::{json, Value};

use crate::{
    editor::EditorProtection,
    management::{LanManagementEvent, LanManagementPage},
    preview::DesignTokens,
    view::color,
    window_background::{PreparedBackground, WindowBackgroundOptions},
};

pub(crate) struct LanCompanionWindow {
    host: Arc<HostState>,
    page: Entity<LanManagementPage>,
    open: bool,
    owner_presented: bool,
    protection: Option<EditorProtection>,
    base_tokens: DesignTokens,
    tokens: DesignTokens,
    dark: bool,
    background: Option<Arc<PreparedBackground>>,
    background_options: WindowBackgroundOptions,
    _subscription: Subscription,
}

impl LanCompanionWindow {
    pub(crate) fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut App,
    ) -> Result<(), String> {
        if let Some(handle) = existing {
            if let Ok(result) = handle.update(cx, |this, window, cx| {
                this.set_design_tokens(tokens.clone(), dark, cx);
                this.present(window, cx)
            }) {
                return result;
            }
        }
        let bounds = Bounds::centered(None, size(px(400.), px(600.)), cx);
        let handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(320.), px(250.))),
                    show: false,
                    focus: !cliphost_core::is_headless(),
                    ..Default::default()
                },
                move |window, cx| {
                    window.set_window_title(tr("局域网互传", "LAN transfer"));
                    if let Err(error) = crate::platform::register_probe_window(window) {
                        log::warn!("LAN companion probe registration failed: {error}");
                    }
                    crate::favorite_drag::register_surface(window, host.clone(), "clipboard", cx);
                    let entity = cx.new(|cx| Self::new(host, tokens, dark, window, cx));
                    let weak = entity.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        if let Some(entity) = weak.upgrade() {
                            entity.update(cx, |this, cx| this.dismiss(window, cx));
                            false
                        } else {
                            true
                        }
                    });
                    entity
                },
            )
            .map_err(|error| error.to_string())?;
        let result = handle
            .update(cx, |this, window, cx| {
                crate::platform::set_geometry(window, None, Some((400., 600.)))?;
                this.present(window, cx)
            })
            .map_err(|error| error.to_string())?;
        if result.is_err() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        } else {
            *existing = Some(handle);
        }
        result
    }

    fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let page = cx.new(|cx| {
            let mut page =
                LanManagementPage::new(host.clone(), tokens.clone(), dark, false, window, cx);
            page.set_companion(true, cx);
            page
        });
        let subscription = cx.subscribe_in(&page, window, |this, _, event, window, cx| {
            match event {
                LanManagementEvent::CloseRequested => this.dismiss(window, cx),
                LanManagementEvent::MoveRequested => {
                    if let Err(error) = crate::platform::begin_companion_move(window, &this.host) {
                        log::warn!("Cannot move the clipboard with its LAN companion: {error}");
                    }
                }
                // The page already keeps the actionable error next to its input.
                LanManagementEvent::Error(_) => {}
            }
        });
        Self {
            host,
            page,
            open: false,
            owner_presented: false,
            protection: None,
            base_tokens: tokens.clone(),
            tokens,
            dark,
            background: None,
            background_options: WindowBackgroundOptions::default(),
            _subscription: subscription,
        }
    }

    fn present(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        self.open = true;
        self.owner_presented = true;
        self.protection
            .get_or_insert_with(|| EditorProtection::acquire_companion(self.host.clone()));
        self.page
            .update(cx, |page, cx| page.set_active(true, window, cx));
        self.page.read(cx).focus_composer(window, cx);
        crate::platform::dock_companion(window, self.host.host_hwnd(), 320.)?;
        if cliphost_core::is_headless() {
            crate::platform::set_no_activate(window, true)?;
            crate::platform::show_no_activate(window)?;
        } else {
            crate::platform::show(window, true, true)?;
        }
        cx.notify();
        Ok(())
    }

    /// Close only this side pane. Keep its entity, conversations and input.
    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.owner_presented = false;
        self.page
            .update(cx, |page, cx| page.set_active(false, window, cx));
        self.protection = None;
        if let Err(error) =
            crate::platform::undock_companion(window).and_then(|_| crate::platform::hide(window))
        {
            log::warn!("Cannot hide the LAN companion: {error}");
        }
        cx.notify();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// An open but unavailable pane still owns ordinary history clicks. The
    /// caller reports this error and must not fall back to clipboard pasting.
    pub(crate) fn history_click_target(&self, cx: &App) -> Result<String, String> {
        if !self.open || !self.owner_presented || !self.host.native_clipboard_list_presented() {
            return Err(tr(
                "互传暂未就绪，请先完成右侧操作。",
                "LAN transfer is not ready. Finish the action in the side pane first.",
            ).into());
        }
        self.page.read(cx).history_click_target()
    }

    pub(crate) fn diagnostics(&self) -> Value {
        json!({"open":self.open,"ownerPresented":self.owner_presented,
            "protected":self.protection.is_some(),"width":400,"minWidth":320,
            "owner":"clipboard","pageRetained":true})
    }

    /// Native ownership handles Hide/minimize immediately. Also suspend the
    /// content for Fold and invalidate its pickers while its owner is absent.
    pub(crate) fn sync_owner_presented(
        &mut self,
        presented: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.open || self.owner_presented == presented {
            return;
        }
        self.owner_presented = presented;
        self.page
            .update(cx, |page, cx| page.set_active(presented, window, cx));
        self.protection = if presented {
            Some(EditorProtection::acquire_companion(self.host.clone()))
        } else {
            None
        };
        if let Err(error) = crate::platform::set_companion_visible(window, presented) {
            log::warn!("Cannot update the LAN companion visibility: {error}");
        }
        cx.notify();
    }

    pub(crate) fn set_exit_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.page
            .update(cx, |page, cx| page.set_exit_pending(pending, cx));
    }

    pub(crate) fn handle_lan_event(
        &mut self,
        name: &str,
        data: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.page
            .update(cx, |page, cx| page.on_event(name, data, window, cx));
    }

    fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.base_tokens = tokens;
        self.tokens = if self.background.is_some() {
            crate::window_glass::tokens(&self.base_tokens, &self.background_options)
        } else {
            self.base_tokens.clone()
        };
        self.dark = dark;
        self.page.update(cx, |page, cx| {
            page.set_design_tokens(self.tokens.clone(), dark, cx)
        });
        cx.notify();
    }

    pub(crate) fn set_window_background(
        &mut self,
        snapshot: Option<Arc<PreparedBackground>>,
        options: WindowBackgroundOptions,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        self.background = snapshot;
        self.background_options = options;
        self.set_design_tokens(tokens, dark, cx);
    }
}

impl Drop for LanCompanionWindow {
    fn drop(&mut self) {
        self.protection = None;
    }
}

impl Render for LanCompanionWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let base = self.base_tokens.theme(self.dark);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(color(&base.background))
            .font_family(self.tokens.typography.font_family.clone())
            .children(self.background.as_ref().map(|image| {
                crate::window_background::render_background_image(image, &self.background_options)
            }))
            .children(self.background.as_ref().map(|_| {
                div()
                    .absolute()
                    .inset_0()
                    .bg(crate::window_glass::page_tint(
                        base,
                        &self.background_options,
                    ))
            }))
            .child(div().relative().size_full().child(self.page.clone()))
    }
}
