//! Presentation of the clipboard's existing digit-key routing. This module never
//! requests keyboard focus or changes the core's input ownership.
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use cliphost_core::{i18n::tr, window_place::overlay_work_area_from_point};
use gpui::{
    canvas, div, point, prelude::*, px, relative, AnimationExt, AnyElement, Context, Hsla,
    MouseButton, PathBuilder, Task, Timer, WeakEntity, Window, WindowHandle, WindowOptions,
};
use gpui_rsx::rsx;
use serde_json::{json, Value};

use crate::{crisp_svg::svg, motion, platform, preview::DesignTokens, view::color};

type Rect = (i32, i32, i32, i32);
const CARD_WIDTH: f32 = 240.0;
const POPUP_INSET: f32 = 8.0;

/// A single first-pin notice per retained clipboard view; hover help is always
/// available. The first notice consumes visible, active, non-hovered time;
/// manual explanations have a bounded lifetime even while hovered.
#[derive(Clone, Debug)]
struct HintState {
    active: bool,
    pinned: bool,
    visible: bool,
    seen: bool,
    automatic: bool,
    suppressed: bool,
    marker_hover: bool,
    bubble_hover: bool,
    remaining: Duration,
    last: Instant,
    bridge_until: Option<Instant>,
    manual_until: Option<Instant>,
}

impl HintState {
    fn new(now: Instant) -> Self {
        Self {
            active: false,
            pinned: false,
            visible: false,
            seen: false,
            automatic: false,
            suppressed: false,
            marker_hover: false,
            bubble_hover: false,
            remaining: motion::KEYBOARD_PASTE_HINT_DURATION,
            last: now,
            bridge_until: None,
            manual_until: None,
        }
    }

    fn tick(&mut self, now: Instant) {
        if self.automatic && self.active && self.visible && !self.marker_hover && !self.bubble_hover
        {
            self.remaining = self
                .remaining
                .saturating_sub(now.saturating_duration_since(self.last));
            if self.remaining.is_zero() {
                self.automatic = false;
            }
        }
        self.last = now;
        if self.bridge_until.is_some_and(|until| now >= until) {
            self.bridge_until = None;
        }
        // Ordinary help must not remain over the list indefinitely, even if
        // the pointer stays still or the native HWND misses a leave event.
        if self.manual_until.is_some_and(|until| now >= until) {
            self.automatic = false;
            self.suppressed = true;
            self.bubble_hover = false;
            self.bridge_until = None;
            self.manual_until = None;
        }
        if !self.hint_visible() {
            self.manual_until = None;
        }
    }

    fn update(&mut self, active: bool, pinned: bool, visible: bool, now: Instant) {
        self.tick(now);
        if self.active != active {
            // Do not turn an already displayed active explanation into an
            // unsolicited paused card when editing/search takes the digits.
            self.dismiss(now);
        }
        self.active = active;
        self.pinned = pinned;
        self.visible = visible;
        if !active {
            self.automatic = false;
        }
        if !visible {
            self.automatic = false;
            self.marker_hover = false;
            self.bubble_hover = false;
            self.bridge_until = None;
            self.manual_until = None;
        }
        if active && pinned && visible && !self.seen {
            self.seen = true;
            self.automatic = true;
            self.suppressed = false;
            self.remaining = motion::KEYBOARD_PASTE_HINT_DURATION;
        }
    }

    fn hover(&mut self, bubble: bool, hovered: bool, now: Instant) {
        self.tick(now);
        let was_visible = self.hint_visible();
        // A closing popup can deliver a final hover notification after hide.
        // Only a fresh marker entry may reopen an explicitly dismissed card.
        if bubble && hovered && self.suppressed {
            return;
        }
        let was_hovered = if bubble {
            self.bubble_hover
        } else {
            self.marker_hover
        };
        if bubble {
            self.bubble_hover = hovered;
        } else {
            self.marker_hover = hovered;
        }
        if hovered {
            // Closing while hovered stays closed until a fresh entry.
            if !was_hovered {
                self.suppressed = false;
            }
            self.bridge_until = None;
        } else if was_hovered {
            // The popup includes the CSS 8px gap; allow one native event turn
            // to cross between the two separate HWNDs without flicker.
            self.bridge_until = Some(now + motion::HOVER);
        }
        if !self.automatic && !was_visible && self.hint_visible() {
            self.manual_until = Some(now + motion::KEYBOARD_PASTE_HINT_DURATION);
        }
    }

    fn reconcile_pointer(&mut self, cursor: (i32, i32), anchor: Option<Rect>, popup: Option<Rect>, now: Instant) {
        let inside = |rect: Option<Rect>| rect.is_some_and(|(x, y, w, h)| {
            cursor.0 >= x && cursor.1 >= y && cursor.0 < x + w && cursor.1 < y + h
        });
        // Reconcile only existing hover. Moving a window under a stationary
        // pointer must never itself open this explanation.
        if self.marker_hover && !inside(anchor) {
            self.hover(false, false, now);
        }
        if self.bubble_hover && !inside(popup) {
            self.hover(true, false, now);
        }
    }

    fn dismiss(&mut self, now: Instant) {
        self.tick(now);
        self.automatic = false;
        self.suppressed = true;
        // A hidden no-activate HWND may never deliver mouse-leave. Its old
        // hover cannot keep the next marker-hover presentation alive.
        self.bubble_hover = false;
        self.bridge_until = None;
        self.manual_until = None;
    }

    fn hint_visible(&self) -> bool {
        self.visible
            && !self.suppressed
            && (self.automatic
                || self.marker_hover
                || self.bubble_hover
                || self.bridge_until.is_some())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    rect: Rect,
    above: bool,
    arrow_x: f32,
    scale: f32,
}

impl Placement {
    fn place(
        anchor: Rect,
        work: Rect,
        owner: Option<Rect>,
        scale: f32,
        logical_height: f32,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let inset = (POPUP_INSET * scale).round() as i32;
        let width = ((CARD_WIDTH + POPUP_INSET * 2.0) * scale).round() as i32;
        // CSS clamps the card to the titlebar's 8 DIP horizontal inset. The
        // native window adds 8 DIP of shadow, so its outer limits are the owner.
        let width = width
            .min(work.2.max(1))
            .min(owner.map_or(i32::MAX, |owner| owner.2.max(1)));
        let height = (logical_height * scale).ceil() as i32;
        let height = height.min(work.3.max(1));
        let work_max_left = work.0 + (work.2 - width).max(0);
        let min_left = owner.map_or(work.0, |owner| owner.0.clamp(work.0, work_max_left));
        let max_left = owner.map_or(work_max_left, |owner| {
            (owner.0 + owner.2 - width)
                .min(work.0 + work.2 - width)
                .clamp(min_left, work_max_left)
        });
        let left = (anchor.0 + anchor.2 + inset - width).clamp(min_left, max_left);
        let below = anchor.1 + anchor.3;
        let above = below + height > work.1 + work.3 && anchor.1 - height >= work.1;
        let top = if above { anchor.1 - height } else { below }
            .clamp(work.1, work.1 + (work.3 - height).max(0));
        let arrow_x = ((anchor.0 + anchor.2 / 2 - left) as f32 / scale).clamp(
            POPUP_INSET + 12.0,
            (width as f32 / scale - POPUP_INSET - 12.0).max(20.0),
        );
        Self {
            rect: (left, top, width, height),
            above,
            arrow_x,
            scale,
        }
    }
}

/// Render this entity in the titlebar, including its 36 DIP Fold strip. Call
/// `update` from the owner's existing maintenance pass using actual routing state.
pub struct KeyboardPasteStatus {
    state: HintState,
    reason: Option<String>,
    tokens: DesignTokens,
    dark: bool,
    anchor: Option<Rect>,
    owner_bounds: Option<Rect>,
    owner_hwnd: isize,
    popup: Option<WindowHandle<KeyboardPasteBubble>>,
    popup_hwnd: isize,
    popup_shown: bool,
    popup_sync_pending: bool,
    popup_epoch: u64,
    presentation_alive: Arc<AtomicBool>,
    popup_signature: Option<(bool, bool, Option<String>, Placement)>,
    bubble_height: f32,
    ticker: Option<Task<()>>,
    hover_motion: motion::Tweens<&'static str>,
    error: Option<String>,
}

impl KeyboardPasteStatus {
    pub fn new(tokens: DesignTokens, dark: bool) -> Self {
        Self {
            state: HintState::new(Instant::now()),
            reason: None,
            tokens,
            dark,
            anchor: None,
            owner_bounds: None,
            owner_hwnd: 0,
            popup: None,
            popup_hwnd: 0,
            popup_shown: false,
            popup_sync_pending: false,
            popup_epoch: 0,
            presentation_alive: Arc::new(AtomicBool::new(false)),
            popup_signature: None,
            bubble_height: 152.0,
            ticker: None,
            hover_motion: motion::Tweens::default(),
            error: None,
        }
    }

    pub fn set_theme(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        if self.dark != dark || self.tokens.theme(self.dark) != tokens.theme(dark) {
            self.tokens = tokens;
            self.dark = dark;
            self.popup_signature = None;
            cx.notify();
        }
    }

    pub fn update(
        &mut self,
        active: bool,
        reason: Option<String>,
        pinned: bool,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = (
            self.state.active,
            self.state.visible,
            self.state.hint_visible(),
            self.reason.clone(),
        );
        self.state.update(active, pinned, visible, Instant::now());
        self.reason = reason;
        self.owner_bounds = platform::client_rect_on_screen(
            window,
            gpui::Bounds {
                origin: point(px(0.0), px(0.0)),
                size: window.viewport_size(),
            },
        );
        #[cfg(windows)]
        {
            self.owner_hwnd = platform::hwnd(window).map_or(0, |hwnd| hwnd.0 as isize);
        }
        self.request_popup_sync(cx);
        if visible && self.ticker.is_none() {
            self.ticker = Some(cx.spawn_in(window, async move |this, cx| loop {
                Timer::after(Duration::from_millis(50)).await;
                let keep = this
                    .update_in(cx, |this, _, cx| {
                        let was = this.state.hint_visible();
                        let now = Instant::now();
                        if let Some(cursor) = platform::cursor_position() {
                            let popup = this.popup_shown.then(|| this.popup_signature.as_ref().map(|signature| signature.3.rect)).flatten();
                            this.state.reconcile_pointer(cursor, this.anchor, popup, now);
                        }
                        this.state.tick(now);
                        this.request_popup_sync(cx);
                        if was != this.state.hint_visible() {
                            cx.notify();
                        }
                        this.state.visible
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }));
        } else if !visible {
            self.ticker = None;
        }
        if before
            != (
                self.state.active,
                self.state.visible,
                self.state.hint_visible(),
                self.reason.clone(),
            )
        {
            cx.notify();
        }
    }

    /// The production path does not need fixture-specific behavior. A fixture
    /// may reset only this view's one-time notice before calling `update` again.
    #[cfg(debug_assertions)]
    pub fn replay_fixture_hint(&mut self, cx: &mut Context<Self>) {
        self.state = HintState::new(Instant::now());
        self.popup_signature = None;
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub fn set_fixture_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.state.hover(false, hovered, Instant::now());
        self.request_popup_sync(cx);
        cx.notify();
    }

    pub fn diagnostics(&self) -> Value {
        #[cfg(windows)]
        let no_activate = if self.popup_hwnd == 0 {
            None
        } else {
            unsafe {
                use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
                Some(
                    GetWindowLongPtrW(HWND(self.popup_hwnd as *mut _), GWL_EXSTYLE)
                        & WS_EX_NOACTIVATE.0 as isize
                        != 0,
                )
            }
        };
        #[cfg(not(windows))]
        let no_activate: Option<bool> = None;
        json!({ "active": self.state.active, "inactiveReason": self.reason,
            "pinned": self.state.pinned, "visible": self.state.visible,
            "hintVisible": self.state.hint_visible(), "firstHintSeen": self.state.seen,
            "firstHint": self.state.automatic, "remainingMs": self.state.remaining.as_millis() as u64,
            "markerHovered": self.state.marker_hover, "bubbleHovered": self.state.bubble_hover,
            "suppressed": self.state.suppressed, "anchor": self.anchor,
            "manualTimeoutArmed":self.state.manual_until.is_some(), "popupEpoch":self.popup_epoch,
            "ownerBounds": self.owner_bounds,
            "popupHwnd": self.popup_hwnd, "popupShown": self.popup_shown,
            "popupNoActivate": no_activate, "error": self.error })
    }

    /// Opening/updating another native window can synchronously draw either
    /// surface. Run it after all parent/entity update borrows have unwound.
    fn request_popup_sync(&mut self, cx: &mut Context<Self>) {
        if self.popup_sync_pending {
            return;
        }
        self.popup_sync_pending = true;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |owner, cx| {
                owner.popup_sync_pending = false;
                owner.sync_popup(cx);
            });
        });
    }

    fn sync_popup(&mut self, cx: &mut Context<Self>) {
        if !self.state.hint_visible() || self.anchor.is_none() || self.owner_hwnd == 0 {
            self.presentation_alive.store(false, Ordering::Release);
            if self.popup_shown {
                self.state.bubble_hover = false;
                if let Some(handle) = self.popup {
                    let _ = handle.update(cx, |_, window, _| platform::hide(window));
                }
                self.popup_shown = false;
                self.popup_signature = None;
            }
            return;
        }
        let anchor = self.anchor.unwrap();
        let Some(area) = overlay_work_area_from_point(anchor.0 + anchor.2 / 2, anchor.1) else {
            return;
        };
        let work = (
            area.work.left.round() as i32,
            area.work.top.round() as i32,
            area.work.width.round() as i32,
            area.work.height.round() as i32,
        );
        let placement = Placement::place(
            anchor,
            work,
            self.owner_bounds,
            area.dpi_scale_y as f32,
            self.bubble_height,
        );
        let signature = (
            self.state.active,
            self.state.automatic,
            self.reason.clone(),
            placement,
        );
        if self.popup_shown && self.popup_signature.as_ref() == Some(&signature) {
            return;
        }
        if !self.popup_shown {
            self.presentation_alive = Arc::new(AtomicBool::new(true));
            self.popup_epoch = self.popup_epoch.wrapping_add(1);
        }
        let presentation_alive = self.presentation_alive.clone();
        let _presentation_guard =
            platform::guard_condition_inherited(move || presentation_alive.load(Ordering::Acquire));
        if self
            .popup
            .as_ref()
            .is_some_and(|handle| handle.read(cx).is_err())
        {
            self.popup = None;
        }
        let result = (|| -> Result<(), String> {
            let handle = if let Some(handle) = self.popup {
                handle
            } else {
                let owner = cx.weak_entity();
                let tokens = self.tokens.clone();
                let dark = self.dark;
                let handle = cx
                    .open_window(
                        WindowOptions {
                            titlebar: None,
                            focus: false,
                            show: false,
                            is_resizable: false,
                            is_minimizable: false,
                            window_background: gpui::WindowBackgroundAppearance::Transparent,
                            ..Default::default()
                        },
                        move |window, cx| {
                            window.set_window_title(tr(
                                "ClipHost GPUI · 数字键提示",
                                "ClipHost GPUI · Number paste help",
                            ));
                            cx.new(|_| KeyboardPasteBubble {
                                owner,
                                tokens,
                                dark,
                                active: false,
                                reason: None,
                                placement,
                                epoch: 0,
                            })
                        },
                    )
                    .map_err(|error| error.to_string())?;
                self.popup = Some(handle);
                handle
            };
            let owner_hwnd = self.owner_hwnd;
            let active = self.state.active;
            let reason = self.reason.clone();
            let tokens = self.tokens.clone();
            let dark = self.dark;
            let just_opened = !self.popup_shown;
            let epoch = self.popup_epoch;
            let hwnd = handle
                .update(cx, |bubble, window, cx| {
                    bubble.active = active;
                    bubble.reason = reason;
                    bubble.tokens = tokens;
                    bubble.dark = dark;
                    bubble.placement = placement;
                    if just_opened {
                        bubble.epoch = epoch;
                    }
                    cx.notify();
                    platform::overlay_show(window, owner_hwnd, placement.rect)?;
                    #[cfg(windows)]
                    let hwnd = platform::hwnd(window)?.0 as isize;
                    #[cfg(not(windows))]
                    let hwnd = 0;
                    Ok::<isize, String>(hwnd)
                })
                .map_err(|error| error.to_string())??;
            self.popup_hwnd = hwnd;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.popup_shown = true;
                self.popup_signature = Some(signature);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(error);
            }
        }
    }

    pub fn render_marker(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let accent = color(&theme.accent);
        let now = Instant::now();
        let progress = self.hover_motion.value(
            "hover",
            if self.state.marker_hover { 1.0 } else { 0.0 },
            motion::HOVER,
            motion::EASE,
            now,
        );
        let active_progress = self.hover_motion.value(
            "active",
            if self.state.active { 1.0 } else { 0.0 },
            motion::HOVER,
            motion::EASE,
            now,
        );
        if self.hover_motion.any_active(now) {
            window.request_animation_frame();
        }
        let foreground = motion::mix_hsla(
            motion::mix_hsla(
                color(&theme.text_muted),
                color(&theme.text_secondary),
                progress,
            ),
            accent,
            active_progress,
        );
        let background = motion::mix_hsla(
            color(&theme.hover).opacity(progress),
            accent.opacity(0.08 + 0.05 * progress),
            active_progress,
        );
        let icon = self
            .tokens
            .icons
            .get("keyboard")
            .cloned()
            .unwrap_or_else(|| "keyboard.svg".into());
        let border = color(&theme.surface);
        let view = cx.weak_entity();
        let probe = canvas(
            move |bounds, window, cx| {
                platform::record_caption_exclusion(window, bounds);
                let Some(rect) = platform::client_rect_on_screen(window, bounds) else {
                    return;
                };
                let view = view.clone();
                cx.defer(move |cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this.anchor != Some(rect) {
                            this.anchor = Some(rect);
                            this.request_popup_sync(cx);
                        }
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div()
            .id("keyboard-paste-status")
            .relative()
            .flex_none()
            .size(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(background)
            .cursor(gpui::CursorStyle::ContextualMenu)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.state.hover(false, *hovered, Instant::now());
                this.request_popup_sync(cx);
                cx.notify();
            }))
            .child(svg().path(icon).size(px(16.0)).text_color(foreground))
            .child(
                div()
                    .absolute()
                    .top(px(3.0))
                    .right(px(2.0))
                    .size(px(4.0))
                    .rounded_full()
                    .bg(accent)
                    .border(px(1.5))
                    .border_color(border)
                    .opacity(active_progress),
            )
            .child(probe)
            .into_any_element()
    }
}

impl Render for KeyboardPasteStatus {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_marker(window, cx)
    }
}

impl Drop for KeyboardPasteStatus {
    fn drop(&mut self) {
        self.presentation_alive.store(false, Ordering::Release);
        // GPUI retains the hidden tool window. No callback may leave it visible
        // after the marker's owning view is gone.
        #[cfg(windows)]
        if self.popup_hwnd != 0 {
            if let Err(error) = platform::hide_retained_overlay(self.popup_hwnd) {
                log::warn!("Cannot hide retained keyboard hint: {error}");
            }
        }
    }
}

struct KeyboardPasteBubble {
    owner: WeakEntity<KeyboardPasteStatus>,
    tokens: DesignTokens,
    dark: bool,
    active: bool,
    reason: Option<String>,
    placement: Placement,
    epoch: u64,
}

fn keycap(label: &'static str, theme: &crate::preview::ThemeTokens) -> AnyElement {
    div()
        .px(px(3.0))
        .border_1()
        .border_color(color(&theme.border))
        .rounded(px(3.0))
        .bg(color(&theme.secondary))
        .text_color(color(&theme.text))
        .text_size(px(10.0))
        .line_height(relative(1.4))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(label)
        .into_any_element()
}

fn arrow(surface: Hsla, border: Hsla, above: bool) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (inset, fill) in [(0.0, border), (1.0, surface)] {
                let mut path = PathBuilder::fill();
                let y = if above { -1.0 } else { 5.0 };
                let tip = if above { 5.0 - inset } else { inset };
                path.move_to(bounds.origin + point(px(inset), px(y)));
                path.line_to(bounds.origin + point(px(5.0), px(tip)));
                path.line_to(bounds.origin + point(px(10.0 - inset), px(y)));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, fill);
                }
            }
        },
    )
    .w(px(10.0))
    .h(px(6.0))
    .into_any_element()
}

impl Render for KeyboardPasteBubble {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.tokens.theme(self.dark).clone();
        let icon = self
            .tokens
            .icons
            .get("keyboard")
            .cloned()
            .unwrap_or_else(|| "keyboard.svg".into());
        let heading = tr(
            if self.active {
                "数字键粘贴中"
            } else {
                "数字粘贴已暂停"
            },
            if self.active {
                "Number paste active"
            } else {
                "Number paste paused"
            },
        );
        let title = div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .mb(px(7.0))
            .text_color(color(&theme.text))
            .line_height(relative(1.3))
            .child(
                svg()
                    .path(icon)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(color(if self.active {
                        &theme.accent
                    } else {
                        &theme.text_muted
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(heading),
            )
            .child(
                div()
                    .id("keyboard-paste-hint-close")
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_color(color(&theme.text_muted))
                    .hover(|element| {
                        element
                            .bg(color(&theme.hover))
                            .text_color(color(&theme.text))
                    })
                    .child("×")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, window, cx| {
                        let owner = this.owner.clone();
                        let epoch = this.epoch;
                        cx.defer(move |cx| {
                            let _ = owner.update(cx, |owner, cx| {
                                if owner.popup_epoch != epoch {
                                    return;
                                }
                                owner.state.dismiss(Instant::now());
                                owner.presentation_alive.store(false, Ordering::Release);
                                owner.popup_shown = false;
                                owner.popup_signature = None;
                                cx.notify();
                            });
                        });
                        let _ = platform::hide(window);
                    })),
            );
        let body = if self.active {
            rsx! {
                <div flex flex_col>
                    <div flex items_center line_height={relative(1.65)}>
                        {keycap("1–9", &theme)}
                        <div mx={px(1.0)}>"、"</div>
                        {keycap("0", &theme)}
                        <div>{tr(" 粘贴对应条目。", " Paste the matching item.")}</div>
                    </div>
                    <div mt={px(2.0)}>{tr("可在其它窗口选择输入位置。", "Choose an input position in another window.")}</div>
                    <div mt={px(2.0)}>{tr("在外部开始打字后，自动恢复正常输入。", "Typing in another window releases number keys.")}</div>
                    <div mt={px(9.0)} pt={px(8.0)} border_t_1 border_color={color(&theme.border)}
                        text_color={color(&theme.text_muted)}>
                        <div flex items_center flex_wrap gap={px(3.0)}>
                            <div>{tr("按", "Press")}</div>
                            {keycap("Esc", &theme)}
                            <div>{tr("隐藏剪贴板，恢复正常输入。", "to hide clipboard and resume normal typing.")}</div>
                        </div>
                    </div>
                </div>
            }.into_any_element()
        } else {
            div()
                .child(self.reason.clone().unwrap_or_else(|| {
                    tr(
                        "当前未接管数字键，数字输入仍交给正在使用的窗口。",
                        "Number keys currently go to the window you are using.",
                    )
                    .to_owned()
                }))
                .into_any_element()
        };
        let shadow = |alpha: f32, y: f32, blur: f32| gpui::BoxShadow {
            color: gpui::black().opacity(alpha),
            offset: point(px(0.0), px(y)),
            blur_radius: px(blur),
            spread_radius: px(0.0),
        };
        let owner = self.owner.clone();
        let mut card = div()
            .relative()
            .w_full()
            .flex_none()
            .p(px(12.0))
            .border_1()
            .border_color(color(&theme.border_secondary).opacity(0.78))
            .rounded(px(10.0))
            .bg(color(&theme.surface))
            .text_color(color(&theme.text_secondary))
            .text_size(px(11.0))
            .line_height(relative(1.65))
            .shadow(vec![shadow(0.14, 8.0, 24.0), shadow(0.06, 2.0, 6.0)])
            .child(title)
            .child(body);
        let tail = div()
            .absolute()
            .left(px(self.placement.arrow_x - POPUP_INSET - 5.0))
            .when(self.placement.above, |element| element.bottom(px(-5.0)))
            .when(!self.placement.above, |element| element.top(px(-5.0)))
            .child(arrow(
                color(&theme.surface),
                color(&theme.border_secondary),
                self.placement.above,
            ));
        card = card.child(tail);
        let above = self.placement.above;
        div()
            .w_full()
            .p(px(POPUP_INSET))
            .font_family(self.tokens.typography.font_family.clone())
            .on_children_prepainted(move |bounds, _, cx| {
                let height = bounds
                    .first()
                    .map_or(0.0, |bounds| f32::from(bounds.size.height))
                    + POPUP_INSET * 2.0;
                let owner = owner.clone();
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |owner, cx| {
                        if (owner.bubble_height - height).abs() > 1.0 {
                            owner.bubble_height = height;
                            owner.popup_signature = None;
                            owner.request_popup_sync(cx);
                        }
                    });
                });
            })
            .id("keyboard-paste-hint-popup")
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                let owner = this.owner.clone();
                let hovered = *hovered;
                let epoch = this.epoch;
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |owner, cx| {
                        if !owner.popup_shown || owner.popup_epoch != epoch
                            || !owner.presentation_alive.load(Ordering::Acquire)
                        {
                            return;
                        }
                        owner.state.hover(true, hovered, Instant::now());
                        owner.request_popup_sync(cx);
                        cx.notify();
                    });
                });
            }))
            .child(card.with_animation(
                ("keyboard-hint-enter", self.epoch),
                motion::enter(motion::KEYBOARD_PASTE_HINT_ENTER, motion::EASE_OUT),
                move |element, t| {
                    element
                        .opacity(t)
                        .top(px((1.0 - t) * if above { 3.0 } else { -3.0 }))
                },
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn manual_help_expires_without_leave_and_does_not_reopen_during_the_same_hover() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(false, true, true, start);
        hint.hover(false, true, at(start, 100));
        assert!(hint.hint_visible() && !hint.automatic);
        hint.hover(true, true, at(start, 1000));
        hint.tick(at(start, 4099));
        assert!(hint.hint_visible());
        hint.tick(at(start, 4100));
        assert!(!hint.hint_visible() && hint.suppressed);
        hint.hover(false, true, at(start, 5000));
        hint.hover(true, true, at(start, 5100));
        assert!(!hint.hint_visible());
        hint.hover(false, false, at(start, 5200));
        hint.hover(false, true, at(start, 5300));
        assert!(hint.hint_visible() && !hint.automatic && !hint.seen);
    }

    #[test]
    fn real_pointer_outside_both_windows_recovers_missing_leave_notifications() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        let anchor = Some((100, 100, 24, 24));
        let popup = Some((80, 132, 256, 100));
        hint.update(false, true, true, start);
        hint.hover(false, true, at(start, 100));
        hint.hover(true, true, at(start, 150));
        hint.reconcile_pointer((600, 600), anchor, popup, at(start, 200));
        assert!(!hint.marker_hover && !hint.bubble_hover);
        hint.tick(at(start, 200) + motion::HOVER + Duration::from_millis(1));
        assert!(!hint.hint_visible());
        hint.reconcile_pointer((110, 110), anchor, popup, at(start, 400));
        assert!(!hint.hint_visible(), "moving a window under the cursor does not open help");
    }

    #[test]
    fn pointer_reconciliation_keeps_the_marker_to_popup_bridge() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(false, true, true, start);
        hint.hover(false, true, at(start, 100));
        hint.reconcile_pointer((110, 128), Some((100, 100, 24, 24)), Some((80, 132, 256, 100)), at(start, 200));
        assert!(hint.hint_visible());
        hint.hover(true, true, at(start, 250));
        hint.tick(at(start, 500));
        assert!(hint.hint_visible() && hint.bubble_hover);
        hint.reconcile_pointer((600, 600), None, None, at(start, 600));
        hint.tick(at(start, 600) + motion::HOVER + Duration::from_millis(1));
        assert!(!hint.hint_visible());
    }

    #[test]
    fn editing_transition_retires_old_help_without_replaying_the_first_notice() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(true, true, true, start);
        hint.hover(false, true, at(start, 50));
        hint.update(false, true, true, at(start, 100));
        assert!(!hint.hint_visible() && hint.seen);
        hint.hover(true, true, at(start, 150));
        hint.hover(false, true, at(start, 200));
        assert!(!hint.hint_visible());
        hint.hover(false, false, at(start, 250));
        hint.hover(false, true, at(start, 300));
        assert!(hint.hint_visible() && !hint.automatic);
        hint.dismiss(at(start, 350));
        assert!(!hint.hint_visible());
        hint.update(true, true, true, at(start, 400));
        assert!(!hint.hint_visible() && hint.seen);
    }

    #[test]
    fn first_effective_pin_shows_once_without_replay_on_later_pins() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(false, true, true, start);
        assert!(!hint.seen && !hint.hint_visible());
        hint.update(true, true, true, at(start, 10));
        assert!(hint.seen && hint.automatic && hint.hint_visible());
        hint.tick(at(start, 4010));
        assert!(!hint.hint_visible());
        hint.update(true, false, true, at(start, 4020));
        hint.update(true, true, true, at(start, 4030));
        assert!(!hint.automatic);
    }

    #[test]
    fn hover_on_either_surface_pauses_only_the_unspent_notice_time() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(true, true, true, start);
        hint.hover(false, true, at(start, 1000));
        hint.hover(true, true, at(start, 2000));
        hint.hover(false, false, at(start, 2500));
        hint.tick(at(start, 6000));
        assert_eq!(hint.remaining, Duration::from_millis(3000));
        hint.hover(true, false, at(start, 7000));
        hint.tick(at(start, 9999));
        assert!(hint.automatic);
        hint.tick(at(start, 10000));
        assert!(!hint.hint_visible());
    }

    #[test]
    fn close_is_suppressed_until_leave_and_fresh_hover_entry() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(true, true, true, start);
        hint.hover(true, true, at(start, 100));
        hint.dismiss(at(start, 200));
        hint.update(true, true, true, at(start, 250));
        hint.hover(true, true, at(start, 300));
        assert!(!hint.hint_visible());
        hint.hover(true, false, at(start, 400));
        hint.hover(false, true, at(start, 500));
        assert!(hint.hint_visible() && !hint.automatic);
    }

    #[test]
    fn close_without_native_leave_cannot_lend_stale_bubble_hover_to_next_help() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(true, true, true, start);
        hint.hover(true, true, at(start, 100));
        hint.dismiss(at(start, 200));
        assert!(!hint.bubble_hover && !hint.hint_visible());
        // Hiding the popup is assumed not to send any bubble leave event.
        hint.hover(false, true, at(start, 300));
        assert!(hint.hint_visible());
        hint.hover(false, false, at(start, 400));
        hint.tick(at(start, 400) + motion::HOVER + Duration::from_millis(1));
        assert!(!hint.hint_visible() && !hint.bubble_hover);
    }

    #[test]
    fn inactive_and_hidden_cancel_one_time_notice_but_preserve_hover_help() {
        let start = Instant::now();
        let mut hint = HintState::new(start);
        hint.update(true, true, true, start);
        hint.update(false, true, true, at(start, 100));
        assert!(!hint.hint_visible());
        hint.hover(false, true, at(start, 200));
        assert!(hint.hint_visible());
        hint.update(false, true, false, at(start, 300));
        assert!(!hint.hint_visible() && !hint.marker_hover && !hint.bubble_hover);
        hint.update(true, true, true, at(start, 400));
        assert!(!hint.hint_visible() && hint.seen);
    }

    #[test]
    fn popup_escapes_fold_strip_and_flips_inside_monitor_work_area() {
        let below = Placement::place((500, 100, 24, 24), (0, 0, 1920, 1080), None, 1.0, 150.0);
        assert!(!below.above && below.rect.1 == 124);
        let above = Placement::place((1900, 1050, 24, 24), (0, 0, 1920, 1080), None, 1.25, 150.0);
        assert!(above.above);
        assert!(above.rect.0 >= 0 && above.rect.0 + above.rect.2 <= 1920);
        assert!(above.rect.1 >= 0 && above.rect.1 + above.rect.3 <= 1050);
    }

    #[test]
    fn narrow_owner_clamps_card_to_titlebar_horizontal_inset() {
        let placed = Placement::place(
            (296, 106, 24, 24),
            (0, 0, 1920, 1080),
            Some((100, 100, 300, 36)),
            1.0,
            150.0,
        );
        assert_eq!(placed.rect.0, 100);
        assert_eq!(placed.rect.2, 256);
        let card_left = placed.rect.0 + POPUP_INSET as i32;
        let card_right = placed.rect.0 + placed.rect.2 - POPUP_INSET as i32;
        assert!(card_left >= 108 && card_right <= 392);
        assert!(
            placed.rect.1 >= 130,
            "Fold height does not clip the separate popup"
        );
    }
}
