//! FIFO queue presentation. The core is the only source of queue ordering,
//! inferred range, armed state, gestures and paste advancement.
//!
//! Embed this entity over the history view. Its parent must retire it when the
//! source view hides, loses its input session, changes page or resizes.
use gpui_rsx::rsx;
use crate::smooth_wheel::SmoothWheelExt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use cliphost_core::{
    host::{
        HostState, NativeFifoControl, NativeFifoKey, NativeFifoKeyboardBinding,
        NativeFifoKeyboardScope, NativeFifoPreviewRequest,
    },
    i18n::tr,
};
use gpui::{
    canvas, div, fill, point, prelude::*, px, size, AnyWindowHandle, Bounds, Context,
    DispatchPhase, EventEmitter, FocusHandle, FontWeight, KeyDownEvent, KeyUpEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollHandle, Subscription, Task, Window,
};
use serde_json::{json, Value};

use crate::{preview::DesignTokens, view::color, window_glass::mix};

#[derive(Clone, Debug)]
pub enum FifoPanelEvent {
    /// Returned by the guarded native arm after its completion is still current.
    Started(Value),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplyPhase {
    Preview,
    Arm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplyDisposition {
    Ignore,
    Retire,
    Apply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FifoControl {
    Row(usize),
    Retry,
    Start,
    Cancel,
}

impl FifoControl {
    fn native(self) -> NativeFifoControl {
        match self {
            Self::Row(index) => NativeFifoControl::Row(index),
            Self::Retry => NativeFifoControl::Retry,
            Self::Start => NativeFifoControl::Start,
            Self::Cancel => NativeFifoControl::Cancel,
        }
    }

    fn from_native(control: NativeFifoControl) -> Option<Self> {
        match control {
            NativeFifoControl::Container => None,
            NativeFifoControl::Row(index) => Some(Self::Row(index)),
            NativeFifoControl::Retry => Some(Self::Retry),
            NativeFifoControl::Start => Some(Self::Start),
            NativeFifoControl::Cancel => Some(Self::Cancel),
        }
    }
}

// This is capture identity, not PreviewSession's reply/arm attempt counter.
static NATIVE_FIFO_CAPTURE: AtomicU64 = AtomicU64::new(0);

fn mint_native_capture(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .ok()
        .map(|previous| previous + 1)
}

fn native_scope_matches(
    scope: &NativeFifoKeyboardScope,
    generation: u64,
    focus_epoch: u64,
) -> bool {
    scope.generation == generation
        && scope.focus_epoch == focus_epoch
        && !scope.composing
        && !scope.starting
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeSpaceIdentity {
    generation: u64,
    focus_epoch: u64,
    press_id: u64,
    control: NativeFifoControl,
}

#[derive(Clone, Copy, Debug)]
struct NativeSpacePress {
    identity: NativeSpaceIdentity,
    activation: SpacePress,
}

fn take_native_space(
    pending: &mut Option<NativeSpacePress>,
    identity: NativeSpaceIdentity,
) -> Option<SpacePress> {
    if pending
        .as_ref()
        .is_some_and(|press| press.identity == identity)
    {
        pending.take().map(|press| press.activation)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug)]
struct SpacePress {
    generation: u64,
    control: Option<FifoControl>,
}

impl SpacePress {
    fn activation(
        self,
        generation: u64,
        focused: Option<FifoControl>,
        native_current: bool,
    ) -> Option<FifoControl> {
        if self.generation == generation && native_current && self.control == focused {
            self.control
        } else {
            None
        }
    }
}

fn discard_ordinary_space_on_blur(pending: &mut Option<SpacePress>, blurred: Option<FifoControl>) {
    if pending
        .as_ref()
        .is_some_and(|press| press.control == blurred)
    {
        *pending = None;
    }
}

fn button_activation(
    key: &str,
    held: bool,
    focused: Option<FifoControl>,
    container_focused: bool,
) -> Option<FifoControl> {
    if held || !matches!(key, "enter" | "space" | " ") {
        return None;
    }
    focused.or_else(|| (key == "enter" && container_focused).then_some(FifoControl::Start))
}

#[derive(Default)]
struct PreviewSession {
    generation: u64,
    active: bool,
    loading: bool,
    starting: bool,
    items: Vec<Value>,
    start_index: usize,
    error: Option<String>,
}

impl PreviewSession {
    fn control_enabled(&self, control: FifoControl) -> bool {
        if !self.active || self.starting {
            return false;
        }
        match control {
            FifoControl::Row(index) => !self.loading && index < self.items.len(),
            FifoControl::Retry => !self.loading && self.error.is_some(),
            FifoControl::Start => self.selected_ids().is_some(),
            FifoControl::Cancel => true,
        }
    }

    fn native_enabled_order(&self) -> Vec<NativeFifoControl> {
        let mut order = (0..self.items.len())
            .filter(|index| self.control_enabled(FifoControl::Row(*index)))
            .map(NativeFifoControl::Row)
            .collect::<Vec<_>>();
        if self.control_enabled(FifoControl::Retry) {
            order.push(NativeFifoControl::Retry);
        }
        if self.control_enabled(FifoControl::Start) {
            order.push(NativeFifoControl::Start);
        }
        if self.control_enabled(FifoControl::Cancel) {
            order.push(NativeFifoControl::Cancel);
        }
        order
    }

    fn reply_disposition(
        &self,
        generation: u64,
        phase: ReplyPhase,
        native_current: bool,
    ) -> ReplyDisposition {
        let waiting = match phase {
            ReplyPhase::Preview => self.loading,
            ReplyPhase::Arm => self.starting,
        };
        if !self.active || !waiting || self.generation != generation {
            // An old result must not even retire a later request in this entity.
            ReplyDisposition::Ignore
        } else if !native_current {
            ReplyDisposition::Retire
        } else {
            ReplyDisposition::Apply
        }
    }

    fn cancel_disposition(&self, generation: u64, native_current: bool) -> ReplyDisposition {
        if !self.active || self.starting || self.generation != generation {
            ReplyDisposition::Ignore
        } else if !native_current {
            ReplyDisposition::Retire
        } else {
            ReplyDisposition::Apply
        }
    }

    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.active = true;
        self.loading = true;
        self.starting = false;
        self.items.clear();
        self.start_index = 0;
        self.error = None;
        self.generation
    }

    fn receive(&mut self, generation: u64, result: Result<Value, String>) -> bool {
        if !self.active || !self.loading || self.generation != generation {
            return false;
        }
        self.loading = false;
        self.starting = false;
        match result {
            Ok(Value::Array(items)) => {
                self.items = items;
                if self.items.is_empty() {
                    self.error = Some(
                        tr(
                            "没有可连续粘贴的条目",
                            "No items available for sequential paste",
                        )
                        .into(),
                    );
                } else if self
                    .items
                    .iter()
                    .any(|item| item["id"].as_str().is_none_or(|id| id.trim().is_empty()))
                {
                    // A malformed reply must not leave an unexplained disabled
                    // Start button or allow trimming into a partial queue.
                    self.items.clear();
                    self.error = Some(
                        tr("无法预览连续粘贴队列", "Unable to preview the paste queue").into(),
                    );
                }
            }
            Ok(_) => {
                self.error =
                    Some(tr("无法预览连续粘贴队列", "Unable to preview the paste queue").into())
            }
            Err(error) => self.error = Some(error),
        }
        true
    }

    fn retire(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.active = false;
        self.loading = false;
        self.starting = false;
        self.items.clear();
        self.start_index = 0;
        self.error = None;
    }

    fn selected_ids(&self) -> Option<Vec<String>> {
        if !self.active || self.loading || self.starting || self.start_index >= self.items.len() {
            return None;
        }
        // Never sort again here. `fifo_preview` applies time/SubIndex ordering
        // and endpoint inference; this is exactly the chosen suffix.
        self.items[self.start_index..]
            .iter()
            .map(|item| {
                item["id"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_string)
            })
            .collect()
    }

    fn select(&mut self, index: usize) -> bool {
        if !self.active || self.loading || self.starting || index >= self.items.len() {
            return false;
        }
        self.start_index = index;
        true
    }

    fn select_stamped_row(&mut self, control: NativeFifoControl) -> bool {
        match control {
            NativeFifoControl::Row(index) => self.select(index),
            _ => false,
        }
    }

    fn begin_start(&mut self) -> Option<(u64, Vec<String>)> {
        let ids = self.selected_ids()?;
        // Each attempt owns its reply, including a retry in the same preview.
        self.generation = self.generation.wrapping_add(1);
        self.starting = true;
        self.error = None;
        Some((self.generation, ids))
    }

    fn receive_start(&mut self, generation: u64, result: Result<Value, String>) -> Option<Value> {
        if !self.active || !self.starting || self.generation != generation {
            return None;
        }
        self.starting = false;
        match result {
            Ok(status) => {
                self.retire();
                Some(status)
            }
            Err(error) => {
                // Keep the chosen suffix and its preview on failure.
                self.error = Some(error);
                None
            }
        }
    }
}

fn preview_text(item: &Value) -> String {
    let text = ["summary", "displayText", "title"]
        .iter()
        .filter_map(|key| item[key].as_str())
        // Match FifoPasteQueuePanel's `summary || displayText || title`.
        .find(|text| !text.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return tr("(空)", "(empty)").into();
    }
    // WebView limits by UTF-16 length. Keep the same limit while avoiding a
    // broken surrogate at the cut point in a native text run.
    let mut units = 0;
    let mut prefix = String::new();
    for ch in text.chars() {
        units += ch.len_utf16();
        if units > 48 {
            prefix.push('…');
            break;
        }
        prefix.push(ch);
    }
    prefix
}

const PANEL_WIDTH: f32 = 280.0;
const PANEL_MAX_HEIGHT: f32 = 420.0;
const LIST_MAX_HEIGHT: f32 = 260.0;
const ROW_HEIGHT: f32 = 28.0;
const ROW_GAP: f32 = 2.0;

/// Private FIFO scroll geometry. ScrollHandle stores a negative content offset.
#[derive(Clone, Copy, Debug)]
struct FifoScrollbar {
    track_height: f32,
    max_scroll: f32,
    thumb_height: f32,
    thumb_top: f32,
}

impl FifoScrollbar {
    fn new(track_height: f32, max_scroll: f32, offset_y: f32) -> Option<Self> {
        if !track_height.is_finite()
            || !max_scroll.is_finite()
            || !offset_y.is_finite()
            || track_height <= 0.0
            || max_scroll <= 0.0
        {
            return None;
        }
        let thumb_height = (track_height * track_height / (track_height + max_scroll))
            .max(20.0)
            .min(track_height);
        let thumb_top =
            (-offset_y).clamp(0.0, max_scroll) / max_scroll * (track_height - thumb_height);
        Some(Self {
            track_height,
            max_scroll,
            thumb_height,
            thumb_top,
        })
    }

    fn scroll_for_thumb_top(self, top: f32) -> f32 {
        let travel = self.track_height - self.thumb_height;
        if travel <= 0.0 {
            0.0
        } else {
            top.clamp(0.0, travel) / travel * self.max_scroll
        }
    }

    fn contains_thumb(self, y: f32) -> bool {
        y >= self.thumb_top && y <= self.thumb_top + self.thumb_height
    }
}

#[derive(Clone, Copy, Debug)]
struct FifoScrollbarDrag {
    generation: u64,
    grab_y: f32,
}

impl FifoScrollbarDrag {
    fn scroll_y(self, generation: u64, scrollbar: FifoScrollbar, pointer_y: f32) -> Option<f32> {
        (self.generation == generation && pointer_y.is_finite())
            .then(|| scrollbar.scroll_for_thumb_top(pointer_y - self.grab_y))
    }
}

#[derive(Debug, Clone, Copy)]
struct PanelGeometry {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    list_height: f32,
}

fn panel_geometry(
    viewport: (f32, f32),
    anchor: (f32, f32),
    rows: usize,
    hint_height: f32,
    loading: bool,
    has_error: bool,
) -> PanelGeometry {
    let (vw, vh) = (viewport.0.max(0.0), viewport.1.max(0.0));
    let margin_x = 8.0f32.min(vw / 2.0);
    let margin_y = 8.0f32.min(vh / 2.0);
    let width = PANEL_WIDTH.min((vw - 2.0 * margin_x).max(0.0));
    let max_height = PANEL_MAX_HEIGHT.min((vh - 2.0 * margin_y).max(0.0));
    // Explicit title/hint/button line heights make positioning use the same
    // bounds that are painted. Reserve an error line without hiding actions.
    let chrome_height =
        92.0 + hint_height + if loading { 32.0 } else { 0.0 } + if has_error { 40.0 } else { 0.0 };
    let rows_height = rows as f32 * ROW_HEIGHT + rows.saturating_sub(1) as f32 * ROW_GAP;
    let list_height = rows_height
        .min(LIST_MAX_HEIGHT)
        .min((max_height - chrome_height).max(0.0));
    let height = (chrome_height + list_height).min(max_height);
    let left = anchor
        .0
        .clamp(margin_x, (vw - width - margin_x).max(margin_x));
    let top = anchor
        .1
        .clamp(margin_y, (vh - height - margin_y).max(margin_y));
    PanelGeometry {
        left,
        top,
        width,
        height,
        list_height,
    }
}

pub struct FifoPanel {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    params: Value,
    session: PreviewSession,
    request: Option<NativeFifoPreviewRequest>,
    scroll: ScrollHandle,
    pending: Option<Task<()>>,
    window: AnyWindowHandle,
    previous_focus: Option<FocusHandle>,
    focus: FocusHandle,
    row_focus: Vec<FocusHandle>,
    retry_focus: FocusHandle,
    start_focus: FocusHandle,
    cancel_focus: FocusHandle,
    space_press: Option<SpacePress>,
    _space_blurs: Vec<Subscription>,
    row_space_blurs: Vec<Subscription>,
    scrollbar_drag: Option<FifoScrollbarDrag>,
    native_capture: Option<u64>,
    native_focus_epoch: u64,
    native_observed: Option<(Option<NativeFifoControl>, usize)>,
    native_scope: Option<NativeFifoKeyboardScope>,
    native_binding: Option<NativeFifoKeyboardBinding>,
    native_space_press: Option<NativeSpacePress>,
}

impl EventEmitter<FifoPanelEvent> for FifoPanel {}

impl Drop for FifoPanel {
    fn drop(&mut self) {
        if let Some(binding) = self.native_binding.take() {
            self.host.retire_native_fifo_keyboard(&binding);
        }
    }
}

impl FifoPanel {
    /// `params` is the same request as WebView: ids + endId + visibleIds for a
    /// single endpoint, or just selected ids for a multi-selection. Optional
    /// `x`/`y` are the pointer position in parent-window logical pixels. The
    /// parent must also pass its acquired clipboard `nativeMenuLease`.
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        params: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            host,
            tokens,
            dark,
            params,
            session: PreviewSession::default(),
            request: None,
            scroll: ScrollHandle::new(),
            pending: None,
            window: window.window_handle(),
            previous_focus: window.focused(cx),
            focus: cx.focus_handle(),
            row_focus: Vec::new(),
            retry_focus: cx.focus_handle().tab_stop(true),
            start_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            space_press: None,
            _space_blurs: Vec::new(),
            row_space_blurs: Vec::new(),
            scrollbar_drag: None,
            native_capture: None,
            native_focus_epoch: 0,
            native_observed: None,
            native_scope: None,
            native_binding: None,
            native_space_press: None,
        };
        for (focus, control) in [
            (panel.focus.clone(), None),
            (panel.retry_focus.clone(), Some(FifoControl::Retry)),
            (panel.start_focus.clone(), Some(FifoControl::Start)),
            (panel.cancel_focus.clone(), Some(FifoControl::Cancel)),
        ] {
            panel
                ._space_blurs
                .push(cx.on_blur(&focus, window, move |this, _, _| {
                    // Real focus events include control-to-control transitions and
                    // window deactivation. Do not disturb a different/new press.
                    discard_ordinary_space_on_blur(&mut this.space_press, control);
                }));
        }
        panel.load(cx);
        // GPUI logical focus does not activate the OS window or select a new
        // input target. It gives Tab a position before the panel's buttons.
        panel.focus.focus(window);
        panel
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        cx.notify();
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let generation = self.session.begin();
        self.scroll = ScrollHandle::new();
        self.row_focus.clear();
        self.row_space_blurs.clear();
        self.space_press = None;
        self.scrollbar_drag = None;
        // A fresh capture ID lets synchronization replace Retry's same-lease
        // binding scope without a presentation tombstone.
        self.native_capture = None;
        self.native_scope = None;
        self.native_space_press = None;
        self.native_observed = None;
        self.native_focus_epoch = 0;
        self.request = None;
        let request = self.params["nativeMenuLease"]
            .as_u64()
            .ok_or_else(|| tr("缺少连续粘贴预览租约。", "Missing FIFO preview lease.").to_string())
            .and_then(|lease| {
                self.host
                    .capture_native_fifo_preview(lease, self.host.host_hwnd())
            });
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.session.receive(generation, Err(error));
                cx.notify();
                return;
            }
        };
        self.request = Some(request.clone());
        self.native_capture = mint_native_capture(&NATIVE_FIFO_CAPTURE);
        let host = self.host.clone();
        let params = self.params.clone();
        self.pending = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.fifo_preview_native(&params, &request) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .session
                    .reply_disposition(generation, ReplyPhase::Preview, true)
                    == ReplyDisposition::Ignore
                {
                    return;
                }
                // Core's post-read check happened on another thread. Check
                // again when this retained entity actually receives the reply.
                let native_current = this
                    .request
                    .as_ref()
                    .is_some_and(|request| this.host.native_fifo_preview_is_current(request));
                match this.session.reply_disposition(
                    generation,
                    ReplyPhase::Preview,
                    native_current,
                ) {
                    ReplyDisposition::Apply => {
                        this.session.receive(generation, result);
                        this.row_focus = (0..this.session.items.len())
                            .map(|_| cx.focus_handle().tab_stop(true))
                            .collect();
                        this.pending = None;
                        cx.notify();
                    }
                    ReplyDisposition::Retire => this.retire(cx),
                    ReplyDisposition::Ignore => {}
                }
            });
        }));
        cx.notify();
    }

    /// Invalidate outstanding preview work without emitting an event. Parent
    /// removal/hide must use this even if it still retains the entity briefly.
    pub fn retire(&mut self, cx: &mut Context<Self>) {
        if let Some(binding) = self.native_binding.take() {
            self.host.retire_native_fifo_keyboard(&binding);
        }
        // Closing and the core's final arm check share the same state lock.
        // Parent's later release is intentionally idempotent for this lease.
        if let Some(lease) = self.params["nativeMenuLease"].as_u64() {
            self.host.release_native_context_menu(lease);
        }
        self.session.retire();
        self.request = None;
        self.pending = None;
        self.space_press = None;
        self.scrollbar_drag = None;
        self.native_capture = None;
        self.native_scope = None;
        self.native_space_press = None;
        let mut previous_controls = self.row_focus.clone();
        previous_controls.extend([
            self.focus.clone(),
            self.retry_focus.clone(),
            self.start_focus.clone(),
            self.cancel_focus.clone(),
        ]);
        self.row_focus.clear();
        self.row_space_blurs.clear();
        let previous_focus = self.previous_focus.take();
        let window_handle = self.window;
        cx.defer(move |cx| {
            let _ = window_handle.update(cx, |_, window, _| {
                // A newer popup/search owns its own focus. Never restore over
                // that handoff from a delayed retirement callback.
                if previous_controls
                    .iter()
                    .any(|focus| focus.is_focused(window))
                {
                    if let Some(previous) = previous_focus {
                        previous.focus(window);
                    } else {
                        window.blur();
                    }
                }
            });
        });
        cx.notify();
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel_for_generation(self.session.generation, cx);
    }

    fn cancel_for_generation(&mut self, generation: u64, cx: &mut Context<Self>) {
        let native_current = self
            .request
            .as_ref()
            .is_some_and(|request| self.host.native_fifo_preview_is_current(request));
        match self.session.cancel_disposition(generation, native_current) {
            ReplyDisposition::Ignore => return,
            ReplyDisposition::Retire => {
                self.retire(cx);
                return;
            }
            ReplyDisposition::Apply => {}
        }
        self.retire(cx);
        // Dismissing a preview never cancels an already armed core queue.
        cx.emit(FifoPanelEvent::Cancelled);
    }

    fn start(&mut self, generation: u64, cx: &mut Context<Self>) {
        if !self.session.active || self.session.generation != generation {
            return;
        }
        let Some(request) = self.request.clone() else {
            return;
        };
        if !self.host.native_fifo_preview_is_current(&request) {
            self.retire(cx);
            return;
        }
        let Some((generation, ids)) = self.session.begin_start() else {
            return;
        };
        self.scrollbar_drag = None;
        self.native_scope = None;
        self.native_space_press = None;
        // Keep the preview lease until the guarded arm succeeds. Hiding,
        // switching source or new copying before mutation retires queued work;
        // failure leaves the chosen suffix visible for the user to retry.
        let host = self.host.clone();
        self.pending = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { host.fifo_arm_native_preview(&json!({ "ids": ids }), &request) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .session
                    .reply_disposition(generation, ReplyPhase::Arm, true)
                    == ReplyDisposition::Ignore
                {
                    return;
                }
                let native_current = this.request.as_ref().is_some_and(|request| match &result {
                    // A successful arm hid its own source. The completion
                    // ticket permits that hide only, never a later reopen.
                    Ok(status) => this.host.native_fifo_arm_reply_is_current(request, status),
                    Err(_) => this.host.native_fifo_preview_is_current(request),
                });
                if this
                    .session
                    .reply_disposition(generation, ReplyPhase::Arm, native_current)
                    == ReplyDisposition::Retire
                {
                    // No Started/Cancelled event: its parent may already own a
                    // newer surface even if this entity survived briefly.
                    this.retire(cx);
                    return;
                }
                this.pending = None;
                if let Some(status) = this.session.receive_start(generation, result) {
                    cx.emit(FifoPanelEvent::Started(status));
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn focused_control(&self, window: &Window) -> Option<FifoControl> {
        self.row_focus
            .iter()
            .position(|focus| focus.is_focused(window))
            .map(FifoControl::Row)
            .or_else(|| {
                self.retry_focus
                    .is_focused(window)
                    .then_some(FifoControl::Retry)
            })
            .or_else(|| {
                self.start_focus
                    .is_focused(window)
                    .then_some(FifoControl::Start)
            })
            .or_else(|| {
                self.cancel_focus
                    .is_focused(window)
                    .then_some(FifoControl::Cancel)
            })
    }

    fn native_focus(&self, window: &Window) -> Option<NativeFifoControl> {
        self.focused_control(window)
            .map(FifoControl::native)
            .or_else(|| {
                self.focus
                    .is_focused(window)
                    .then_some(NativeFifoControl::Container)
            })
    }

    fn acknowledge_native_focus(&mut self, window: &Window) {
        self.native_observed = Some((self.native_focus(window), self.session.start_index));
    }

    fn ordinary_focus_changed(&mut self, window: &Window) {
        // Leaving and returning to the same control cannot revive its down.
        // This immediate path also covers several key events before a paint.
        self.space_press = None;
        if let Some(epoch) = self.native_focus_epoch.checked_add(1) {
            self.native_focus_epoch = epoch;
        } else {
            // Exhaustion cannot turn an old packet into a new input scope.
            self.native_capture = None;
        }
        self.native_scope = None;
        self.native_space_press = None;
        self.acknowledge_native_focus(window);
    }

    fn activate_control(
        &mut self,
        control: FifoControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session.control_enabled(control) {
            return;
        }
        match control {
            FifoControl::Row(index) => {
                self.session.select(index);
                self.scroll.scroll_to_item(index);
            }
            FifoControl::Retry => {
                self.focus.focus(window);
                self.load(cx);
            }
            FifoControl::Start => self.start(self.session.generation, cx),
            FifoControl::Cancel => self.cancel(cx),
        }
    }

    fn focus_native_control(&self, control: NativeFifoControl, window: &mut Window) {
        match control {
            NativeFifoControl::Container => self.focus.focus(window),
            NativeFifoControl::Row(index) => {
                if let Some(focus) = self.row_focus.get(index) {
                    focus.focus(window);
                    self.scroll.scroll_to_item(index);
                }
            }
            NativeFifoControl::Retry => self.retry_focus.focus(window),
            NativeFifoControl::Start => self.start_focus.focus(window),
            NativeFifoControl::Cancel => self.cancel_focus.focus(window),
        }
    }

    /// This is the original request paired with the native scope. Do not
    /// recapture a request when draining an already accepted key packet.
    pub fn native_preview_request(&self) -> Option<&NativeFifoPreviewRequest> {
        self.request.as_ref()
    }

    /// Preserve this capture generation/focus epoch when preparing admission.
    /// Neither is PreviewSession's reply or arm-attempt counter.
    pub fn native_keyboard_scope(
        &mut self,
        window: &Window,
        _cx: &mut Context<Self>,
    ) -> Option<NativeFifoKeyboardScope> {
        let observed = (self.native_focus(window), self.session.start_index);
        if self
            .native_observed
            .is_some_and(|previous| previous != observed)
        {
            self.ordinary_focus_changed(window);
        } else {
            self.native_observed = Some(observed);
        }
        self.native_scope = None;
        if !self.session.active || self.request.is_none() {
            return None;
        }
        let scope = NativeFifoKeyboardScope {
            generation: self.native_capture?,
            focus_epoch: self.native_focus_epoch,
            enabled_order: self.session.native_enabled_order(),
            focused: observed.0?,
            selected_row: self.session.start_index,
            composing: false, // FIFO has no text input; root supplies its IME barrier.
            starting: self.session.starting,
        };
        self.native_scope = Some(scope.clone());
        Some(scope)
    }

    /// Root calls after synchronizing its overlay guard and before draining
    /// each batch. Scope replacement must not retire the same-lease binding
    /// first: that would create Core's presentation retirement tombstone.
    pub fn sync_native_keyboard(
        &mut self,
        window: &Window,
        composing: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(mut scope) = self.native_keyboard_scope(window, cx) else {
            if let Some(binding) = self.native_binding.take() {
                self.host.retire_native_fifo_keyboard(&binding);
            }
            self.native_scope = None;
            self.native_space_press = None;
            return;
        };
        scope.composing = composing;
        let Some(request) = self.request.as_ref() else {
            return;
        };
        match self.host.bind_native_fifo_keyboard(request, scope.clone()) {
            Ok(binding) => {
                self.native_binding = Some(binding);
                self.native_scope = Some(scope);
            }
            Err(_) => {
                // Admission may have retired while arm's own hide receipt is
                // awaiting UI delivery. Do not synthesize a preview Cancel.
                self.native_scope = None;
                self.native_space_press = None;
                if let Some(binding) = self.native_binding.take() {
                    self.host.retire_native_fifo_keyboard(&binding);
                }
            }
        }
    }

    /// A packet is already swallowed by Core. Stale packets are discarded,
    /// never offered to another surface or allowed to clear a newer Space.
    pub fn handle_native_key(
        &mut self,
        event: &NativeFifoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session.active
            || self.session.starting
            || self.request.is_none()
            || self.native_capture != Some(event.generation())
            || !self.native_scope.as_ref().is_some_and(|scope| {
                native_scope_matches(scope, event.generation(), event.focus_epoch())
                    && scope.enabled_order.contains(&event.control())
            })
            || !self.host.native_fifo_key_is_current(event)
        {
            return;
        }
        let control = event.control();
        let identity = NativeSpaceIdentity {
            generation: event.generation(),
            focus_epoch: event.focus_epoch(),
            press_id: event.press_id(),
            control,
        };
        match (event.key(), event.down()) {
            ("Tab", true) => self.focus_native_control(control, window),
            ("ArrowUp" | "ArrowDown", true) => {
                // Core already advanced selected_row. Apply its exact target,
                // including repeats, rather than stepping the UI a second time.
                if self.session.select_stamped_row(control) {
                    self.focus_native_control(control, window);
                }
            }
            ("Enter", true) if !event.repeat() => {
                if let Some(control) = FifoControl::from_native(control) {
                    self.activate_control(control, window, cx);
                }
            }
            ("Escape", true) if !event.repeat() => self.cancel(cx),
            ("Space", true) if !event.repeat() => {
                self.native_space_press = Some(NativeSpacePress {
                    identity,
                    activation: SpacePress {
                        generation: self.session.generation,
                        control: FifoControl::from_native(control)
                            .filter(|control| self.session.control_enabled(*control)),
                    },
                });
            }
            ("Space", false) => {
                if let Some(press) = take_native_space(&mut self.native_space_press, identity) {
                    if !event.alt() && !event.ctrl() && !event.shift() && !event.win() {
                        if let Some(control) = press.activation(
                            self.session.generation,
                            self.focused_control(window),
                            true,
                        ) {
                            self.activate_control(control, window, cx);
                        }
                    }
                }
            }
            _ => return,
        }
        if self.native_capture == Some(event.generation()) {
            // Replay may lag behind Core's cursor. Acknowledging its logical
            // focus prevents the next sync from classifying it as external.
            self.acknowledge_native_focus(window);
        }
        cx.notify();
    }

    /// Root forwards real GPUI key input through this entry to preserve the
    /// WebView buttons' Tab and Enter/Space semantics. No native focus calls.
    pub fn key_down_in(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.session.active {
            return false;
        }
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == "tab"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.function
        {
            if self.session.starting {
                return true;
            }
            if self
                .request
                .as_ref()
                .is_none_or(|request| !self.host.native_fifo_preview_is_current(request))
            {
                self.retire(cx);
                return true;
            }
            if modifiers.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
            if let Some(FifoControl::Row(index)) = self.focused_control(window) {
                self.scroll.scroll_to_item(index);
            }
            self.ordinary_focus_changed(window);
            cx.notify();
            return true;
        }
        if modifiers.number_of_modifiers() == 0
            && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
        {
            if self.session.starting {
                return true;
            }
            if self
                .request
                .as_ref()
                .is_none_or(|request| !self.host.native_fifo_preview_is_current(request))
            {
                self.retire(cx);
                return true;
            }
            let focused = self.focused_control(window);
            let activation = button_activation(
                &event.keystroke.key,
                event.is_held,
                focused,
                self.focus.is_focused(window),
            );
            if matches!(event.keystroke.key.as_str(), "space" | " ") {
                // HTML buttons activate Space on release. Keep the original
                // control/generation; repeats or a later focus cannot re-arm it.
                if !event.is_held && (focused.is_some() || self.focus.is_focused(window)) {
                    self.ordinary_focus_changed(window);
                    self.space_press = Some(SpacePress {
                        generation: self.session.generation,
                        control: activation
                            .filter(|control| self.session.control_enabled(*control)),
                    });
                }
                return focused.is_some() || self.focus.is_focused(window);
            }
            if let Some(control) =
                activation.filter(|control| self.session.control_enabled(*control))
            {
                self.ordinary_focus_changed(window);
                self.activate_control(control, window, cx);
                cx.notify();
            }
            return focused.is_some() || self.focus.is_focused(window);
        }
        let handled = self.key_down(event, cx);
        if handled && matches!(event.keystroke.key.as_str(), "up" | "down") {
            if let Some(focus) = self.row_focus.get(self.session.start_index) {
                focus.focus(window);
            }
            self.ordinary_focus_changed(window);
        }
        handled
    }

    /// Root forwards GPUI Space key-up as well as key-down. An unmatched key-up
    /// cannot activate a control, and the accepted pair is drained only once.
    pub fn key_up_in(
        &mut self,
        event: &KeyUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(event.keystroke.key.as_str(), "space" | " ") {
            return false;
        }
        let Some(press) = self.space_press.take() else {
            return false;
        };
        let native_current = self
            .request
            .as_ref()
            .is_some_and(|request| self.host.native_fifo_preview_is_current(request));
        if !native_current {
            self.retire(cx);
            return true;
        }
        let control = press.activation(
            self.session.generation,
            self.focused_control(window),
            native_current,
        );
        if event.keystroke.modifiers.number_of_modifiers() == 0 {
            if let Some(control) = control.filter(|control| self.session.control_enabled(*control))
            {
                self.ordinary_focus_changed(window);
                self.activate_control(control, window, cx);
                cx.notify();
            }
        }
        true
    }

    /// Parent forwards accepted keyboard input here before history shortcuts.
    /// It also permits fake-focus/no-focus input without focusing a new HWND.
    pub fn key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.session.active {
            return false;
        }
        if event.keystroke.modifiers.number_of_modifiers() != 0 {
            return false;
        }
        if self.session.starting {
            return matches!(
                event.keystroke.key.as_str(),
                "escape" | "enter" | "up" | "down"
            );
        }
        if self
            .request
            .as_ref()
            .is_some_and(|request| !self.host.native_fifo_preview_is_current(request))
        {
            self.retire(cx);
            // Do not let a key accepted for an old FIFO fall through to a
            // reopened history's Enter/Escape handler.
            return matches!(
                event.keystroke.key.as_str(),
                "escape" | "enter" | "up" | "down"
            );
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                if !event.is_held {
                    self.cancel(cx);
                }
            }
            "enter" => {
                if !event.is_held {
                    self.start(self.session.generation, cx);
                }
            }
            "up" => {
                if self
                    .session
                    .select(self.session.start_index.saturating_sub(1))
                {
                    self.scroll.scroll_to_item(self.session.start_index);
                    cx.notify();
                }
            }
            "down" => {
                let index =
                    (self.session.start_index + 1).min(self.session.items.len().saturating_sub(1));
                if self.session.select(index) {
                    self.scroll.scroll_to_item(index);
                    cx.notify();
                }
            }
            _ => return false,
        }
        true
    }

    fn scrollbar_current(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if !self.session.active || self.session.starting || self.session.generation != generation {
            return false;
        }
        if self
            .request
            .as_ref()
            .is_none_or(|request| !self.host.native_fifo_preview_is_current(request))
        {
            self.retire(cx);
            return false;
        }
        true
    }

    fn scrollbar_metrics(&self) -> Option<FifoScrollbar> {
        FifoScrollbar::new(
            f32::from(self.scroll.bounds().size.height),
            f32::from(self.scroll.max_offset().height),
            f32::from(self.scroll.offset().y),
        )
    }

    fn scrollbar_down(&mut self, generation: u64, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if !self.scrollbar_current(generation, cx) {
            return;
        }
        let Some(scrollbar) = self.scrollbar_metrics() else {
            return;
        };
        let y = f32::from(event.position.y - self.scroll.bounds().top());
        if scrollbar.contains_thumb(y) {
            self.scrollbar_drag = Some(FifoScrollbarDrag {
                generation,
                grab_y: y - scrollbar.thumb_top,
            });
        } else {
            // Like the WebView track, page towards the click without changing
            // the chosen queue suffix or moving the logical button focus.
            let direction = if y < scrollbar.thumb_top { -1.0 } else { 1.0 };
            let scroll_y = (-f32::from(self.scroll.offset().y)
                + direction * scrollbar.track_height)
                .clamp(0.0, scrollbar.max_scroll);
            self.scroll
                .set_offset(point(self.scroll.offset().x, px(-scroll_y)));
        }
        cx.notify();
    }

    fn scrollbar_move(
        &mut self,
        generation: u64,
        pointer_y: Option<f32>,
        finish: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self
            .scrollbar_drag
            .filter(|drag| drag.generation == generation)
        else {
            return false;
        };
        if finish {
            self.scrollbar_drag = None;
        }
        if self.scrollbar_current(generation, cx) {
            if let Some(scroll_y) = self.scrollbar_metrics().and_then(|scrollbar| {
                pointer_y.and_then(|y| {
                    drag.scroll_y(
                        generation,
                        scrollbar,
                        y - f32::from(self.scroll.bounds().top()),
                    )
                })
            }) {
                self.scroll
                    .set_offset(point(self.scroll.offset().x, px(-scroll_y)));
            }
            cx.notify();
        }
        true
    }
}

impl Render for FifoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.session.active {
            return div().into_any_element();
        }
        if self.row_space_blurs.len() != self.row_focus.len() {
            self.row_space_blurs = self
                .row_focus
                .iter()
                .enumerate()
                .map(|(index, focus)| {
                    cx.on_blur(focus, window, move |this, _, _| {
                        discard_ordinary_space_on_blur(
                            &mut this.space_press,
                            Some(FifoControl::Row(index)),
                        );
                    })
                })
                .collect();
        }
        let generation = self.session.generation;
        let t = self.tokens.theme(self.dark);
        let viewport = window.viewport_size();
        let viewport = (f32::from(viewport.width), f32::from(viewport.height));
        let anchor = (
            self.params["x"].as_f64().unwrap_or(8.0) as f32,
            self.params["y"].as_f64().unwrap_or(40.0) as f32,
        );
        let hint = tr(
            "最早在上，点一行可裁掉更早的条目",
            "Oldest first; select a row to skip earlier items",
        );
        let width = panel_geometry(viewport, anchor, 0, 16.0, false, false).width;
        let hint_text: gpui::SharedString = hint.into();
        let hint_run = gpui::TextRun {
            len: hint_text.len(),
            font: gpui::font(self.tokens.typography.font_family.clone()),
            color: gpui::white(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let hint_height = window
            .text_system()
            .shape_text(
                hint_text,
                px(11.0),
                &[hint_run],
                Some(px((width - 22.0).max(1.0))),
                None,
            )
            .map(|lines| {
                lines
                    .iter()
                    .map(|line| f32::from(line.size(px(16.0)).height))
                    .sum::<f32>()
            })
            .unwrap_or(32.0)
            .max(16.0);
        let geometry = panel_geometry(
            viewport,
            anchor,
            self.session.items.len(),
            hint_height,
            self.session.loading,
            self.session.error.is_some(),
        );
        let content_height = self.session.items.len() as f32 * ROW_HEIGHT
            + self.session.items.len().saturating_sub(1) as f32 * ROW_GAP;
        let has_scrollbar = geometry.list_height > 0.0 && content_height > geometry.list_height;
        let scrollbar_width = self.tokens.layout.scrollbar_width.clamp(6.0, 20.0);
        let scrollbar_scroll = self.scroll.clone();
        let scrollbar_color = color(&t.scrollbar_thumb);
        let scrollbar_track = color(&t.background);
        let scrollbar = rsx! {
            <div id={"fifo-scrollbar"} absolute right_0 top_0 h={px(geometry.list_height)}
                w={px(scrollbar_width)} />
        }
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    // Track/drag input cannot focus the panel or activate a row.
                    window.prevent_default();
                    cx.stop_propagation();
                    this.scrollbar_down(generation, event, cx);
                }),
            )
            .child(
                canvas(
                    move |bounds, _, _| {
                        FifoScrollbar::new(
                            f32::from(bounds.size.height),
                            f32::from(scrollbar_scroll.max_offset().height),
                            f32::from(scrollbar_scroll.offset().y),
                        )
                    },
                    move |bounds, scrollbar, window, _| {
                        if let Some(scrollbar) = scrollbar {
                            window.paint_quad(fill(bounds, scrollbar_track).corner_radii(px(5.0)));
                            let thumb = Bounds::new(
                                point(
                                    bounds.left() + px(2.0),
                                    bounds.top() + px(scrollbar.thumb_top),
                                ),
                                size(
                                    (bounds.size.width - px(4.0)).max(px(1.0)),
                                    px(scrollbar.thumb_height),
                                ),
                            );
                            window.paint_quad(fill(thumb, scrollbar_color).corner_radii(px(5.0)));
                        }
                    },
                )
                .size_full(),
            );
        let move_scrollbar = cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
            let held = event.pressed_button == Some(MouseButton::Left);
            if this.scrollbar_move(
                generation,
                held.then(|| f32::from(event.position.y)),
                !held,
                cx,
            ) {
                cx.stop_propagation();
            }
        });
        let release_scrollbar = cx.listener(move |this, event: &MouseUpEvent, _, cx| {
            if event.button == MouseButton::Left
                && this.scrollbar_move(generation, Some(f32::from(event.position.y)), true, cx)
            {
                // A release over a row/action must only finish the thumb drag.
                cx.stop_propagation();
            }
        });
        let scrollbar_pointer_events = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                // The existing native capture keeps delivering move/up outside
                // the track; a div's hitbox-filtered listeners would lose them.
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture {
                        move_scrollbar(event, window, cx);
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture {
                        release_scrollbar(event, window, cx);
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let last = self.session.items.len().saturating_sub(1);
        let enabled = self.session.selected_ids().is_some();
        let rows = self
            .session
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let trimmed = index < self.session.start_index;
                let selected = index == self.session.start_index;
                let focus = self.row_focus.get(index).cloned();
                let focused = focus.as_ref().is_some_and(|focus| focus.is_focused(window));
                rsx! {
                    <div id={("fifo-row", index)} flex items_center gap={px(8.0)} px={px(8.0)}
                        h={px(ROW_HEIGHT)} flex_shrink_0 rounded={px(5.0)} text_size={px(12.0)}
                        line_height={px(16.0)} />
                }
                    .when(!self.session.starting, |row| row.cursor_pointer())
                    .when(selected, |row| {
                        row.bg(color(&mix(&t.tertiary, &t.accent, 0.30)))
                    })
                    .when(trimmed, |row| row.opacity(0.4))
                    .when(focused, |row| row.border_1().border_color(color(&t.accent)))
                    .when_some(focus, |row, focus| {
                        row.track_focus(
                            &focus
                                .tab_index(index as isize)
                                .tab_stop(!self.session.starting),
                        )
                    })
                    .hover(|style| style.bg(color(&mix(&t.tertiary, &t.accent, 0.25))))
                    .child(
                        rsx! {
                            <div w={px(18.0)} flex_shrink_0 text_color={color(&t.text_muted)}>
                                {(index + 1).to_string()}
                            </div>
                        },
                    )
                    .child(
                        rsx! {
                            <div flex_1 min_w_0 text_ellipsis>
                                {preview_text(item)}
                            </div>
                        },
                    )
                    .when(index == last, |row| {
                        row.child(
                            rsx! {
                                <div flex_shrink_0 text_size={px(10.0)}
                                    text_color={color(&t.text_muted)}>
                                    {tr("终点", "End")}
                                </div>
                            },
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.session.generation != generation {
                            return;
                        }
                        if this.request.as_ref().is_none_or(|request| {
                            !this.host.native_fifo_preview_is_current(request)
                        }) {
                            this.retire(cx);
                            return;
                        }
                        if this.session.select(index) {
                            this.scroll.scroll_to_item(index);
                            this.ordinary_focus_changed(window);
                            cx.notify();
                        }
                    }))
            })
            .collect::<Vec<_>>();
        div()
            .absolute()
            .inset_0()
            .id("fifo-preview-overlay")
            .on_any_mouse_down(
                cx.listener(move |this, _, _, cx| this.cancel_for_generation(generation, cx)),
            )
            .child(scrollbar_pointer_events)
            .child(
                rsx! {
                    <div id={"fifo-preview-panel"} absolute left={px(geometry.left)}
                        top={px(geometry.top)} w={px(geometry.width)} h={px(geometry.height)}
                        overflow_hidden p={px(10.0)} flex flex_col gap={px(8.0)}
                        rounded={px(8.0)} border_1 border_color={color(&t.border)}
                        bg={color(&t.surface)}
                        font_family={self.tokens.typography.font_family.clone()}
                        text_size={px(13.0)} text_color={color(&t.text)} />
                }
                    .track_focus(&self.focus)
                    .tab_group()
                    .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .child(
                        rsx! {
                            <div h={px(18.0)} flex_shrink_0 line_height={px(18.0)}
                                font_weight={FontWeight::MEDIUM}>
                                {tr("连续粘贴（先进先出）", "Sequential paste (FIFO)")}
                            </div>
                        },
                    )
                    .child(
                        rsx! {
                            <div h={px(hint_height)} flex_shrink_0 text_size={px(11.0)}
                                line_height={px(16.0)} text_color={color(&t.text_muted)}>
                                {hint}
                            </div>
                        },
                    )
                    .when(self.session.loading, |panel| {
                        panel.child(
                            rsx! {
                                <div h={px(24.0)} flex_shrink_0>
                                    {tr("加载中…", "Loading…")}
                                </div>
                            },
                        )
                    })
                    .child(
                        rsx! {
                            <div relative min_h_0 h={px(geometry.list_height)} flex_shrink_0 />
                        }
                            .child(
                                rsx! {
                                    <div id={"fifo-preview-rows"} h_full flex flex_col
                                        gap={px(ROW_GAP)} />
                                }
                                    .when(has_scrollbar, |list| list.pr(px(scrollbar_width)))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .smooth_wheel("fifo-preview-rows", &self.scroll, cx)
                                    .children(rows),
                            )
                            .when(has_scrollbar, |list| list.child(scrollbar)),
                    )
                    .when_some(self.session.error.clone(), |panel, error| {
                        panel.child(
                            div()
                                .id("fifo-preview-error")
                                .h(px(32.0))
                                .flex_shrink_0()
                                .overflow_y_scroll()
                                .text_color(color(&t.error))
                                .text_size(px(12.0))
                                .line_height(px(16.0))
                                .child(error),
                        )
                    })
                    .child(
                        rsx! {
                            <div h={px(28.0)} flex_shrink_0 flex justify_end items_center
                                gap={px(6.0)} text_size={px(12.0)} line_height={px(16.0)} />
                        }
                            .when(
                                self.session.error.is_some()
                                    && !self.session.loading
                                    && !self.session.starting,
                                |buttons| {
                                    buttons.child(
                                        rsx! {
                                            <div id={"fifo-retry"} cursor_pointer px={px(8.0)}
                                                py={px(5.0)} rounded={px(6.0)} border_1
                                                border_color={color(&t.border_secondary)}
                                                text_color={color(&t.text)} />
                                        }
                                            .track_focus(
                                                &self
                                                    .retry_focus
                                                    .clone()
                                                    .tab_index(self.session.items.len() as isize),
                                            )
                                            .when(self.retry_focus.is_focused(window), |button| {
                                                button.border_1().border_color(color(&t.accent))
                                            })
                                            .child(tr("重试", "Retry"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                if this.session.active
                                                    && this.session.generation == generation
                                                    && !this.session.loading
                                                    && !this.session.starting
                                                {
                                                    this.focus.focus(window);
                                                    this.load(cx);
                                                }
                                            })),
                                    )
                                },
                            )
                            .child(
                                rsx! {
                                    <div id={"fifo-start"} rounded={px(6.0)} px={px(10.0)}
                                        py={px(5.0)} border_1 border_color={color(&t.accent)}
                                        bg={color(&mix(&t.surface, &t.accent, 0.22))}
                                        text_color={color(&t.text)} />
                                }
                                    .track_focus(
                                        &self
                                            .start_focus
                                            .clone()
                                            .tab_index(self.session.items.len() as isize + 1)
                                            .tab_stop(enabled),
                                    )
                                    .when(self.start_focus.is_focused(window), |button| {
                                        button.border_1().border_color(color(&t.text))
                                    })
                                    .when(enabled, |button| button.cursor_pointer())
                                    .when(!enabled, |button| button.opacity(0.4))
                                    .child(if self.session.starting {
                                        tr("开始中…", "Starting…")
                                    } else {
                                        tr("开始", "Start")
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start(generation, cx)
                                    })),
                            )
                            .child(
                                rsx! {
                                    <div id={"fifo-cancel"} rounded={px(6.0)} px={px(10.0)}
                                        py={px(5.0)} border_1
                                        border_color={color(&t.border_secondary)}
                                        text_color={color(&t.text)} />
                                }
                                    .track_focus(
                                        &self
                                            .cancel_focus
                                            .clone()
                                            .tab_index(self.session.items.len() as isize + 2)
                                            .tab_stop(!self.session.starting),
                                    )
                                    .when(self.cancel_focus.is_focused(window), |button| {
                                        button.border_color(color(&t.accent))
                                    })
                                    .when(!self.session.starting, |button| button.cursor_pointer())
                                    .when(self.session.starting, |button| button.opacity(0.4))
                                    .child(tr("取消", "Cancel"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel_for_generation(generation, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_space_focus_roundtrip_discards_old_up_and_allows_fresh_down() {
        let press = SpacePress {
            generation: 5,
            control: Some(FifoControl::Start),
        };
        let mut pending = Some(press);
        discard_ordinary_space_on_blur(&mut pending, Some(FifoControl::Start));
        assert!(
            pending.take().is_none(),
            "returning to Start must not revive its old Space"
        );
        pending = Some(press);
        assert_eq!(
            pending
                .take()
                .unwrap()
                .activation(5, Some(FifoControl::Start), true),
            Some(FifoControl::Start),
            "a fresh Space remains usable after the roundtrip"
        );
    }

    #[test]
    fn unrelated_control_blur_cannot_cancel_a_new_ordinary_space() {
        let mut pending = Some(SpacePress {
            generation: 8,
            control: Some(FifoControl::Start),
        });
        discard_ordinary_space_on_blur(&mut pending, Some(FifoControl::Row(1)));
        assert_eq!(pending.as_ref().unwrap().control, Some(FifoControl::Start));
        discard_ordinary_space_on_blur(&mut pending, Some(FifoControl::Start));
        assert!(
            pending.is_none(),
            "window blur from the pressed control also retires it"
        );
    }

    #[test]
    fn native_capture_ids_distinguish_entities_and_never_wrap() {
        let counter = AtomicU64::new(0);
        let mut first = PreviewSession::default();
        let mut second = PreviewSession::default();
        assert_eq!(first.begin(), second.begin());
        assert_ne!(mint_native_capture(&counter), mint_native_capture(&counter));
        let exhausted = AtomicU64::new(u64::MAX - 1);
        assert_eq!(mint_native_capture(&exhausted), Some(u64::MAX));
        assert_eq!(mint_native_capture(&exhausted), None);
        assert_eq!(mint_native_capture(&exhausted), None);
        assert_eq!(exhausted.load(Ordering::Acquire), u64::MAX);
    }

    #[test]
    fn native_old_scope_and_space_release_cannot_take_new_pending_press() {
        let scope = NativeFifoKeyboardScope {
            generation: 12,
            focus_epoch: 3,
            enabled_order: vec![NativeFifoControl::Cancel],
            focused: NativeFifoControl::Cancel,
            selected_row: 0,
            composing: false,
            starting: false,
        };
        assert!(!native_scope_matches(&scope, 11, 3));
        assert!(!native_scope_matches(&scope, 12, 2));
        assert!(native_scope_matches(&scope, 12, 3));
        let identity = NativeSpaceIdentity {
            generation: 12,
            focus_epoch: 3,
            press_id: 9,
            control: NativeFifoControl::Cancel,
        };
        let mut pending = Some(NativeSpacePress {
            identity,
            activation: SpacePress {
                generation: 5,
                control: Some(FifoControl::Cancel),
            },
        });
        for stale in [
            NativeSpaceIdentity {
                generation: 11,
                ..identity
            },
            NativeSpaceIdentity {
                focus_epoch: 2,
                ..identity
            },
            NativeSpaceIdentity {
                press_id: 8,
                ..identity
            },
            NativeSpaceIdentity {
                control: NativeFifoControl::Start,
                ..identity
            },
        ] {
            assert!(take_native_space(&mut pending, stale).is_none());
            assert_eq!(pending.as_ref().unwrap().identity, identity);
        }
        assert!(take_native_space(&mut pending, identity).is_some());
        assert!(take_native_space(&mut pending, identity).is_none());
    }

    #[test]
    fn native_arrows_apply_stamped_row_without_another_increment() {
        let mut session = PreviewSession::default();
        let generation = session.begin();
        session.receive(generation, Ok(json!([{"id":"a"},{"id":"b"},{"id":"c"}])));
        session.select(2);
        assert!(session.select_stamped_row(NativeFifoControl::Row(0)));
        assert_eq!(session.start_index, 0);
        assert!(session.select_stamped_row(NativeFifoControl::Row(0)));
        assert_eq!(session.start_index, 0);
        assert!(!session.select_stamped_row(NativeFifoControl::Row(99)));
        assert!(!session.select_stamped_row(NativeFifoControl::Start));
        assert_eq!(session.start_index, 0);
    }

    #[test]
    fn native_order_matches_enabled_rows_buttons_and_pauses_starting() {
        let mut session = PreviewSession::default();
        let generation = session.begin();
        assert_eq!(session.native_enabled_order(), [NativeFifoControl::Cancel]);
        session.receive(generation, Ok(json!([{"id":"a"},{"id":"b"}])));
        assert_eq!(
            session.native_enabled_order(),
            [
                NativeFifoControl::Row(0),
                NativeFifoControl::Row(1),
                NativeFifoControl::Start,
                NativeFifoControl::Cancel
            ]
        );
        let (arm, _) = session.begin_start().unwrap();
        assert!(session.native_enabled_order().is_empty());
        session.receive_start(arm, Err("temporary failure".into()));
        assert_eq!(
            session.native_enabled_order(),
            [
                NativeFifoControl::Row(0),
                NativeFifoControl::Row(1),
                NativeFifoControl::Retry,
                NativeFifoControl::Start,
                NativeFifoControl::Cancel
            ]
        );
    }

    #[test]
    fn fifo_scrollbar_uses_full_scroll_range_and_has_no_empty_track() {
        for (height, max_scroll) in [(0.0, 100.0), (260.0, 0.0), (260.0, -1.0)] {
            assert!(FifoScrollbar::new(height, max_scroll, 0.0).is_none());
        }
        let top = FifoScrollbar::new(260.0, 638.0, 0.0).unwrap();
        let bottom = FifoScrollbar::new(260.0, 638.0, -638.0).unwrap();
        assert_eq!(top.thumb_top, 0.0);
        assert!((bottom.thumb_top + bottom.thumb_height - 260.0).abs() < 0.001);
        assert!((bottom.scroll_for_thumb_top(bottom.thumb_top) - 638.0).abs() < 0.001);
        assert_eq!(bottom.scroll_for_thumb_top(-500.0), 0.0);
        assert_eq!(bottom.scroll_for_thumb_top(500.0), 638.0);
    }

    #[test]
    fn fifo_thumb_drag_keeps_grab_position_and_cannot_move_a_new_generation() {
        let scrollbar = FifoScrollbar::new(260.0, 2340.0, -1170.0).unwrap();
        let drag = FifoScrollbarDrag {
            generation: 4,
            grab_y: 7.0,
        };
        let unchanged = drag
            .scroll_y(4, scrollbar, scrollbar.thumb_top + 7.0)
            .unwrap();
        assert!((unchanged - 1170.0).abs() < 0.001);
        assert_eq!(drag.scroll_y(4, scrollbar, 5000.0), Some(2340.0));
        assert_eq!(drag.scroll_y(4, scrollbar, -5000.0), Some(0.0));
        assert_eq!(drag.scroll_y(5, scrollbar, 5000.0), None);
        assert_eq!(drag.scroll_y(4, scrollbar, f32::NAN), None);
        let dense = FifoScrollbar::new(260.0, 100_000.0, 0.0).unwrap();
        assert_eq!(dense.thumb_height, 20.0);
    }

    #[test]
    fn chosen_suffix_preserves_exact_core_order_and_endpoint() {
        let mut session = PreviewSession::default();
        let generation = session.begin();
        assert!(session.receive(
            generation,
            Ok(json!([
                { "id": "old-b", "createTime": "same-time", "subIndex": 1 },
                { "id": "old-a", "createTime": "same-time", "subIndex": 0 },
                { "id": "end" }
            ]))
        ));
        assert_eq!(session.selected_ids().unwrap(), ["old-b", "old-a", "end"]);
        session.start_index = 1;
        assert_eq!(session.selected_ids().unwrap(), ["old-a", "end"]);
        session.start_index = 2;
        assert_eq!(session.selected_ids().unwrap(), ["end"]);
    }

    #[test]
    fn retired_and_replaced_previews_cannot_restore_items_or_arm_ids() {
        let mut session = PreviewSession::default();
        let first = session.begin();
        session.retire();
        assert!(!session.receive(first, Ok(json!([{ "id": "late" }]))));
        assert!(session.selected_ids().is_none());
        let next = session.begin();
        assert!(!session.receive(first, Err("old error".into())));
        assert!(session.loading);
        assert!(session.receive(next, Ok(json!([{ "id": "current" }]))));
        assert_eq!(session.selected_ids().unwrap(), ["current"]);
    }

    #[test]
    fn loading_empty_and_invalid_items_cannot_arm_a_partial_queue() {
        let mut session = PreviewSession::default();
        let generation = session.begin();
        assert!(session.selected_ids().is_none());
        session.receive(generation, Ok(json!([])));
        assert!(session.selected_ids().is_none());
        assert!(session.error.is_some());
        let generation = session.begin();
        session.receive(generation, Ok(json!([{ "id": "valid" }, { "id": " " }])));
        assert!(session.selected_ids().is_none());
        assert!(session.error.is_some());
        assert!(session.items.is_empty());
        assert!(!session.select(1));
    }

    #[test]
    fn row_preview_collapses_whitespace_and_truncates_unicode_safely() {
        assert_eq!(
            preview_text(&json!({"summary":"  first\n\tsecond  "})),
            "first second"
        );
        let text = "剪".repeat(49);
        let preview = preview_text(&json!({"displayText": text}));
        assert_eq!(preview.chars().count(), 49);
        assert!(preview.ends_with('…'));
        assert_eq!(
            preview_text(&json!({"displayText": "😀".repeat(25)})),
            "😀".repeat(24) + "…"
        );
        assert_eq!(
            preview_text(&json!({"summary": "   ", "displayText": "must not replace summary"})),
            tr("(空)", "(empty)")
        );
    }

    #[test]
    fn start_failure_keeps_suffix_and_blocks_duplicate_start_or_selection() {
        let mut session = PreviewSession::default();
        let generation = session.begin();
        session.receive(generation, Ok(json!([{ "id": "old" }, { "id": "end" }])));
        assert!(session.select(1));
        let (generation, ids) = session.begin_start().unwrap();
        assert_eq!(ids, ["end"]);
        assert!(session.begin_start().is_none());
        assert!(!session.select(0));
        assert!(session
            .receive_start(generation, Err("failed to arm".into()))
            .is_none());
        assert_eq!(session.error.as_deref(), Some("failed to arm"));
        assert_eq!(session.selected_ids().unwrap(), ["end"]);
        let (retry, ids) = session.begin_start().unwrap();
        assert_eq!(ids, ["end"]);
        assert!(session.error.is_none());
        assert!(session
            .receive_start(generation, Err("old attempt".into()))
            .is_none());
        assert!(session.starting);
        assert!(session.error.is_none());
        let status = json!({"armed": true, "remainingCount": 1});
        assert_eq!(
            session.receive_start(retry, Ok(status.clone())),
            Some(status)
        );
        assert!(!session.active);
        assert!(!session.starting);
        assert!(session.selected_ids().is_none());
    }

    #[test]
    fn old_start_success_or_error_cannot_close_or_overwrite_new_preview() {
        let mut session = PreviewSession::default();
        let generation = session.begin();
        session.receive(generation, Ok(json!([{ "id": "old" }])));
        let (old, _) = session.begin_start().unwrap();
        session.retire();
        let current = session.begin();
        session.receive(current, Ok(json!([{ "id": "current" }])));
        assert!(session
            .receive_start(old, Ok(json!({"armed": true})))
            .is_none());
        assert!(session
            .receive_start(old, Err("stale error".into()))
            .is_none());
        assert!(session.active);
        assert!(session.error.is_none());
        assert_eq!(session.selected_ids().unwrap(), ["current"]);
    }

    #[test]
    fn geometry_caps_list_and_clamps_pointer_anchor_to_viewport() {
        let geometry = panel_geometry((800.0, 600.0), (799.0, 599.0), 50, 16.0, false, false);
        assert_eq!(geometry.width, 280.0);
        assert_eq!(geometry.list_height, 260.0);
        assert_eq!(geometry.height, 368.0);
        assert_eq!(geometry.left, 512.0);
        assert_eq!(geometry.top, 224.0);
        let error = panel_geometry((800.0, 600.0), (-100.0, -100.0), 50, 32.0, false, true);
        assert_eq!(error.left, 8.0);
        assert_eq!(error.top, 8.0);
        assert!(error.height <= 420.0);
    }

    #[test]
    fn geometry_stays_inside_small_viewports_with_loading_and_error() {
        for (vw, vh) in [(0.0, 0.0), (10.0, 10.0), (160.0, 90.0), (220.0, 240.0)] {
            let geometry = panel_geometry((vw, vh), (1000.0, 1000.0), 50, 48.0, true, true);
            assert!(geometry.left >= 0.0 && geometry.top >= 0.0);
            assert!(geometry.width >= 0.0 && geometry.list_height >= 0.0);
            assert!(geometry.left + geometry.width <= vw);
            assert!(geometry.top + geometry.height <= vh);
        }
    }

    #[test]
    fn preview_failure_and_retry_keep_latest_request_owner() {
        let mut session = PreviewSession::default();
        let failed = session.begin();
        assert!(session.receive(failed, Err("database unavailable".into())));
        assert_eq!(session.error.as_deref(), Some("database unavailable"));
        let retry = session.begin();
        assert!(session.error.is_none());
        assert!(!session.receive(failed, Ok(json!([{ "id": "late" }]))));
        assert!(session.loading);
        assert!(session.receive(retry, Ok(json!([{ "id": "retry" }]))));
        assert_eq!(session.selected_ids().unwrap(), ["retry"]);
    }

    #[test]
    fn native_retirement_after_read_discards_preview_success_and_error() {
        for result in [Ok(json!([{ "id": "late" }])), Err("late error".into())] {
            let mut session = PreviewSession::default();
            let generation = session.begin();
            assert_eq!(
                session.reply_disposition(generation, ReplyPhase::Preview, false),
                ReplyDisposition::Retire
            );
            // The native presentation hid/reopened before UI delivery. The
            // Retire branch discards either result without updating content.
            session.retire();
            assert!(!session.receive(generation, result));
            assert!(session.items.is_empty());
            assert!(session.error.is_none());
        }
    }

    #[test]
    fn own_arm_hide_requires_current_completion_before_emitting_started() {
        for result in [Ok(json!({"armed":true})), Err("late arm error".into())] {
            let mut session = PreviewSession::default();
            let preview = session.begin();
            session.receive(preview, Ok(json!([{ "id": "old" }])));
            let (arm, _) = session.begin_start().unwrap();
            // A post-mutate receipt permits the arm's own hide.
            assert_eq!(
                session.reply_disposition(arm, ReplyPhase::Arm, true),
                ReplyDisposition::Apply
            );
            // A later reopen or lease replacement invalidates that receipt.
            assert_eq!(
                session.reply_disposition(arm, ReplyPhase::Arm, false),
                ReplyDisposition::Retire
            );
            session.retire();
            assert!(session.receive_start(arm, result).is_none());
            assert_eq!(
                session.cancel_disposition(arm, false),
                ReplyDisposition::Ignore
            );
            assert!(session.error.is_none());
        }
    }

    #[test]
    fn old_reply_and_mouse_cancel_cannot_retire_a_new_preview_or_retry() {
        let mut session = PreviewSession::default();
        let old_preview = session.begin();
        session.receive(old_preview, Ok(json!([{ "id": "old" }])));
        let (old_arm, _) = session.begin_start().unwrap();
        session.retire();
        let current = session.begin();
        session.receive(current, Ok(json!([{ "id": "current" }])));
        assert_eq!(
            session.reply_disposition(old_arm, ReplyPhase::Arm, false),
            ReplyDisposition::Ignore
        );
        assert_eq!(
            session.reply_disposition(old_preview, ReplyPhase::Preview, false),
            ReplyDisposition::Ignore
        );
        assert_eq!(
            session.cancel_disposition(old_preview, true),
            ReplyDisposition::Ignore
        );
        assert_eq!(
            session.cancel_disposition(current, false),
            ReplyDisposition::Retire
        );
        assert_eq!(
            session.cancel_disposition(current, true),
            ReplyDisposition::Apply
        );
        assert!(session.active);
        assert_eq!(session.selected_ids().unwrap(), ["current"]);
    }

    #[test]
    fn keyboard_buttons_activate_the_focused_control_without_arming_a_row() {
        for key in ["enter", "space"] {
            for control in [
                FifoControl::Row(2),
                FifoControl::Retry,
                FifoControl::Start,
                FifoControl::Cancel,
            ] {
                assert_eq!(
                    button_activation(key, false, Some(control), false),
                    Some(control)
                );
                assert_eq!(button_activation(key, true, Some(control), false), None);
            }
        }
        assert_eq!(
            button_activation("enter", false, None, true),
            Some(FifoControl::Start)
        );
        assert_eq!(button_activation("space", false, None, true), None);
        assert_eq!(button_activation("enter", false, None, false), None);
        assert_eq!(
            button_activation("tab", false, Some(FifoControl::Start), false),
            None
        );
    }

    #[test]
    fn disabled_pending_and_retired_controls_do_not_accept_keyboard_activation() {
        let mut session = PreviewSession::default();
        let preview = session.begin();
        assert!(session.control_enabled(FifoControl::Cancel));
        assert!(!session.control_enabled(FifoControl::Start));
        assert!(!session.control_enabled(FifoControl::Row(0)));
        session.receive(preview, Ok(json!([{ "id": "first" }, { "id": "last" }])));
        session.select(1);
        assert!(session.control_enabled(FifoControl::Row(0)));
        assert!(!session.control_enabled(FifoControl::Row(2)));
        assert!(!session.control_enabled(FifoControl::Retry));
        let (attempt, _) = session.begin_start().unwrap();
        for control in [
            FifoControl::Row(0),
            FifoControl::Retry,
            FifoControl::Start,
            FifoControl::Cancel,
        ] {
            assert!(!session.control_enabled(control));
        }
        session.receive_start(attempt, Err("database busy".into()));
        assert!(session.control_enabled(FifoControl::Retry));
        assert!(session.control_enabled(FifoControl::Start));
        assert_eq!(session.selected_ids().unwrap(), ["last"]);
        session.retire();
        assert!(!session.control_enabled(FifoControl::Cancel));
    }

    #[test]
    fn space_release_requires_original_focus_generation_and_native_presentation() {
        let press = SpacePress {
            generation: 4,
            control: Some(FifoControl::Start),
        };
        assert_eq!(
            press.activation(4, Some(FifoControl::Start), true),
            Some(FifoControl::Start)
        );
        assert_eq!(press.activation(5, Some(FifoControl::Start), true), None);
        assert_eq!(press.activation(4, Some(FifoControl::Cancel), true), None);
        assert_eq!(press.activation(4, Some(FifoControl::Start), false), None);
        assert_eq!(press.activation(4, None, true), None);
        let mut pending = Some(press);
        assert!(pending
            .take()
            .unwrap()
            .activation(4, Some(FifoControl::Start), true)
            .is_some());
        assert!(
            pending.take().is_none(),
            "one Space release cannot activate twice"
        );
    }
}
