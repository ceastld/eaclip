//! Explicit GPUI FIFO keyboard admission. The LL hook only reads cached host
//! ownership and feeds the pure ledger; preview/clipboard checks run at bind,
//! drain and execution on the UI side.
pub use super::native_fifo_keyboard_state::{
    Control as NativeFifoControl, Scope as NativeFifoKeyboardScope,
};
use super::native_fifo_keyboard_state::{Input, Ledger, Stroke};
use super::*;

const RETIRED: &str = "连续粘贴键盘会话已失效，请重新打开预览。";

#[derive(Clone, Debug)]
pub struct NativeFifoKeyboardBinding {
    owner: Arc<()>,
    id: u64,
}

#[derive(Clone, Debug)]
pub struct NativeFifoKey {
    request: NativeFifoPreviewRequest,
    binding: NativeFifoKeyboardBinding,
    stroke: Stroke,
}

impl NativeFifoKey {
    pub fn request(&self) -> &NativeFifoPreviewRequest {
        &self.request
    }
    pub fn epoch(&self) -> u64 {
        self.stroke.epoch()
    }
    pub fn generation(&self) -> u64 {
        self.stroke.generation()
    }
    pub fn focus_epoch(&self) -> u64 {
        self.stroke.focus_epoch()
    }
    pub fn press_id(&self) -> u64 {
        self.stroke.press_id()
    }
    pub fn control(&self) -> NativeFifoControl {
        self.stroke.control()
    }
    pub fn key(&self) -> &'static str {
        self.stroke.key()
    }
    pub fn down(&self) -> bool {
        self.stroke.down()
    }
    pub fn repeat(&self) -> bool {
        self.stroke.repeat()
    }
    pub fn alt(&self) -> bool {
        self.stroke.alt()
    }
    pub fn ctrl(&self) -> bool {
        self.stroke.ctrl()
    }
    pub fn shift(&self) -> bool {
        self.stroke.shift()
    }
    pub fn win(&self) -> bool {
        self.stroke.win()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    hwnd: isize,
    window_epoch: u64,
    presentation_epoch: u64,
    lease: u64,
    popup_hwnd: isize,
    fifo_generation: u64,
    incoming_drag_generation: u64,
    keyboard_target: isize,
}

impl Identity {
    fn same_presentation(self, other: Self) -> bool {
        self.hwnd == other.hwnd
            && self.window_epoch == other.window_epoch
            && self.presentation_epoch == other.presentation_epoch
            && self.lease == other.lease
            && self.popup_hwnd == other.popup_hwnd
            && self.fifo_generation == other.fifo_generation
    }
    fn capture(inner: &HostInner) -> Option<Self> {
        let menu = inner.native_context_menu?;
        (menu.owner == "clipboard" && inner.host_hwnd != 0 && menu.popup_hwnd == inner.host_hwnd)
            .then_some(Self {
                hwnd: inner.host_hwnd,
                window_epoch: inner.chrome_input_epochs[0].window,
                presentation_epoch: inner.chrome_presentation_epochs[0],
                lease: menu.id,
                popup_hwnd: menu.popup_hwnd,
                fifo_generation: inner.fifo_generation,
                incoming_drag_generation: inner.native_incoming_drag_next_id,
                keyboard_target: inner.keyboard_target_hwnd,
            })
    }
}

struct Binding {
    ticket: NativeFifoKeyboardBinding,
    request: NativeFifoPreviewRequest,
    scope: NativeFifoKeyboardScope,
    foreground: isize,
    identity: Identity,
    raw_foreground_epoch: u64,
}

pub(super) struct NativeFifoKeyboard {
    owner: Arc<()>,
    next_id: u64,
    // Retain retirement as a tombstone. A late poll/retry cannot revive the
    // same preview after focus, modal, drag or runtime ownership was lost.
    presentation: Option<Identity>,
    binding: Option<Binding>,
    ledger: Ledger,
}

impl Default for NativeFifoKeyboard {
    fn default() -> Self {
        Self {
            owner: Arc::new(()),
            next_id: 0,
            presentation: None,
            binding: None,
            ledger: Ledger::default(),
        }
    }
}

fn input(ev: KeyEvent, repeat: bool) -> Input {
    Input {
        vk: ev.vk,
        down: ev.down,
        repeat,
        alt: ev.alt,
        ctrl: ev.ctrl,
        shift: ev.shift,
        win: ev.win,
    }
}

fn ready(inner: &HostInner) -> bool {
    inner.window_runtime_ready
        && inner.clipboard_web_view_ready
        && inner.window_visible
        && !inner.is_collapsed
        && !inner.edge_tucked
        && !inner.ui.clipboard_editing
        && !inner.spa_modal_open
        && !inner.ole_drag_active
        && inner.native_incoming_drag.is_none()
        && !inner.move_interaction_active
        && !inner.qp.overlay_visible
        && inner.qp.machine.phase == QuickPastePhase::Idle
        && !inner.qp.mouse.is_held()
        && !inner.super_menu.visible
        && !(inner.ui.panel_visible && inner.ui.panel_fake_focus)
        && !inner.native_context_menu_escape_down
}

fn owns_foreground(inner: &HostInner, foreground: isize) -> bool {
    foreground != 0
        && (foreground == inner.host_hwnd
            || (foreground == inner.keyboard_target_hwnd
                && inner.config.enable_no_focus_mode
                && inner.no_focus_session_active
                && inner.has_host_fake_focus
                && matches!(inner.ui.keyboard_focus_host, None | Some("clipboard"))))
}

fn same_scope(a: &NativeFifoKeyboardScope, b: &NativeFifoKeyboardScope) -> bool {
    a.generation == b.generation
        && a.focus_epoch == b.focus_epoch
        && a.enabled_order == b.enabled_order
        && a.composing == b.composing
        && a.starting == b.starting
}

fn scope_follows(previous: &NativeFifoKeyboardScope, next: &NativeFifoKeyboardScope) -> bool {
    // UI counters wrap, but a delayed older generation/focus snapshot must
    // never replace a newer binding within this same preview presentation.
    let delta = if previous.generation == next.generation {
        next.focus_epoch.wrapping_sub(previous.focus_epoch)
    } else {
        next.generation.wrapping_sub(previous.generation)
    };
    delta < (1_u64 << 63)
}

impl NativeFifoKeyboard {
    pub(super) fn reset_after_input_gap(&mut self) {
        self.retire_binding();
        self.ledger = Ledger::default();
    }
    pub(super) fn retire_binding(&mut self) {
        self.binding = None;
        self.ledger.retire();
    }

    pub(super) fn retire_lease(&mut self, lease: u64) {
        if self
            .presentation
            .is_some_and(|identity| identity.lease == lease)
        {
            self.retire_binding();
        }
    }

    pub(super) fn observe_foreground(&mut self, foreground: isize) {
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.foreground != foreground
                || binding.raw_foreground_epoch != crate::active_window::raw_foreground_epoch())
        {
            self.retire_binding();
        }
    }

    pub(super) fn owns_presentation(&self, inner: &HostInner) -> bool {
        self.presentation
            .zip(Identity::capture(inner))
            .is_some_and(|(a, b)| a.same_presentation(b))
    }

    fn observe(&mut self, identity: Option<Identity>, foreground: isize, permitted: bool) {
        self.observe_foreground(foreground);
        if self.binding.as_ref().is_some_and(|binding| {
            !permitted || Some(binding.identity) != identity || !self.ledger.is_bound()
        }) {
            self.retire_binding();
        }
    }

    fn ticket_current(&self, ticket: &NativeFifoKeyboardBinding) -> bool {
        Arc::ptr_eq(&self.owner, &ticket.owner)
            && self
                .binding
                .as_ref()
                .is_some_and(|binding| binding.ticket.id == ticket.id
                    && binding.raw_foreground_epoch == crate::active_window::raw_foreground_epoch())
    }
}

impl HostState {
    fn native_fifo_foreground(&self) -> isize {
        if crate::clipboard::is_headless() {
            self.inner.lock().last_fg
        } else {
            crate::window_menu::foreground_hwnd()
        }
    }

    /// FIFO keeps its own foreground lifetime. Clipboard list keyboard ownership
    /// can continue across external windows without extending an open FIFO panel.
    pub fn native_fifo_keyboard_target_left(&self) -> bool {
        let foreground = self.native_fifo_foreground();
        super::foreground_left_keyboard_target(&self.inner.lock(), foreground)
    }

    /// Bind after acquiring the FIFO's clipboard menu lease, setting its HWND
    /// and synchronizing its overlay guard. focus_epoch advances only for
    /// external/ordinary focus changes, never while replaying native events.
    pub fn bind_native_fifo_keyboard(
        &self,
        request: &NativeFifoPreviewRequest,
        scope: NativeFifoKeyboardScope,
    ) -> Result<NativeFifoKeyboardBinding, String> {
        self.bind_native_fifo_keyboard_for_foreground(request, scope, self.native_fifo_foreground())
    }

    fn bind_native_fifo_keyboard_for_foreground(
        &self,
        request: &NativeFifoPreviewRequest,
        scope: NativeFifoKeyboardScope,
        foreground: isize,
    ) -> Result<NativeFifoKeyboardBinding, String> {
        if !self.native_fifo_preview_is_current(request) {
            return Err(RETIRED.into());
        }
        let ticket = {
            let mut inner = self.inner.lock();
            // Serialize exact original-request admission with the binding
            // mutation. A precheck alone could lend a late A caller B's token.
            self.verify_native_fifo_preview_locked(&inner, request)?;
            let identity = Identity::capture(&inner).ok_or(RETIRED)?;
            let permitted = ready(&inner) && owns_foreground(&inner, foreground);
            let keyboard = &mut inner.native_fifo_keyboard;
            keyboard.observe(Some(identity), foreground, permitted);
            if !permitted {
                return Err(RETIRED.into());
            }
            let unchanged = if keyboard
                .presentation
                .is_some_and(|old| old.same_presentation(identity))
            {
                let Some(binding) = keyboard.binding.as_ref() else {
                    return Err(RETIRED.into());
                };
                if !scope_follows(&binding.scope, &scope) {
                    return Err(RETIRED.into());
                }
                same_scope(&binding.scope, &scope).then(|| binding.ticket.clone())
            } else {
                keyboard.retire_binding();
                None
            };
            if let Some(ticket) = unchanged {
                ticket
            } else {
                keyboard.next_id = keyboard.next_id.checked_add(1).ok_or(RETIRED)?;
                keyboard.ledger.bind(scope.clone())?;
                let ticket = NativeFifoKeyboardBinding {
                    owner: keyboard.owner.clone(),
                    id: keyboard.next_id,
                };
                keyboard.presentation = Some(identity);
                keyboard.binding = Some(Binding {
                    ticket: ticket.clone(),
                    request: request.clone(),
                    scope,
                    foreground,
                    identity,
                    raw_foreground_epoch: crate::active_window::raw_foreground_epoch(),
                });
                ticket
            }
        };
        // No clipboard access occurs in routing. This closes the bind-side
        // sequence race; execution performs the full guard again.
        if !self.native_fifo_preview_is_current(request) {
            self.retire_native_fifo_keyboard(&ticket);
            return Err(RETIRED.into());
        }
        Ok(ticket)
    }

    /// Retire only this returned binding. Old cleanup cannot touch a new
    /// scope, preview, HostState allocation or presentation.
    pub fn retire_native_fifo_keyboard(&self, ticket: &NativeFifoKeyboardBinding) {
        let mut inner = self.inner.lock();
        if inner.native_fifo_keyboard.ticket_current(ticket) {
            inner.native_fifo_keyboard.retire_binding();
        }
    }

    pub fn take_native_fifo_key(&self) -> Option<NativeFifoKey> {
        let foreground = self.native_fifo_foreground();
        loop {
            let event = {
                let mut inner = self.inner.lock();
                let identity = Identity::capture(&inner);
                let permitted = ready(&inner) && owns_foreground(&inner, foreground);
                let keyboard = &mut inner.native_fifo_keyboard;
                keyboard.observe(identity, foreground, permitted);
                let stroke = keyboard.ledger.pop()?;
                let binding = keyboard.binding.as_ref()?;
                NativeFifoKey {
                    request: binding.request.clone(),
                    binding: binding.ticket.clone(),
                    stroke,
                }
            };
            if self.native_fifo_key_is_current_for_foreground(&event, foreground) {
                return Some(event);
            }
        }
    }

    /// Recheck directly before executing the stamped control. The event
    /// retains its original preview request and physical press identity.
    pub fn native_fifo_key_is_current(&self, event: &NativeFifoKey) -> bool {
        self.native_fifo_key_is_current_for_foreground(event, self.native_fifo_foreground())
    }

    fn native_fifo_key_is_current_for_foreground(
        &self,
        event: &NativeFifoKey,
        foreground: isize,
    ) -> bool {
        if self.input_gap_pending() { return false; }
        let preview_current = self.native_fifo_preview_is_current(&event.request);
        let mut inner = self.inner.lock();
        if self.input_gap_pending() { return false; }
        let identity = Identity::capture(&inner);
        let host_permitted = ready(&inner) && owns_foreground(&inner, foreground);
        let permitted = preview_current && host_permitted;
        let keyboard = &mut inner.native_fifo_keyboard;
        // Checking an old event must not retire a newer binding merely because
        // the event's original clipboard request has since expired.
        if !keyboard.ticket_current(&event.binding) {
            return false;
        }
        keyboard.observe(identity, foreground, permitted);
        permitted
            && keyboard.ticket_current(&event.binding)
            && keyboard.ledger.current(&event.stroke)
    }

    pub(crate) fn notify_native_fifo_foreground(&self, foreground: isize) {
        self.inner
            .lock()
            .native_fifo_keyboard
            .observe_foreground(foreground);
    }




}

// Shared pure ledger routing with an already-owned input guard.
impl HostState {
    pub(super) fn input_dispatch_native_fifo_held_key(
        &self, inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        suspended: bool,
        repeat: bool,
    ) -> bool {
        let identity = Identity::capture(&inner);
        let permitted = !suspended && ready(&inner) && owns_foreground(&inner, foreground);
        let keyboard = &mut inner.native_fifo_keyboard;
        keyboard.observe(identity, foreground, permitted);
        keyboard
            .ledger
            .route_held(input(ev, repeat), permitted && keyboard.binding.is_some())
        }

    pub(super) fn input_route_native_fifo_key(
        &self, inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        repeat: bool,
    ) -> Option<bool> {
        let identity = Identity::capture(&inner);
        let owns = inner.native_fifo_keyboard.owns_presentation(&inner);
        let permitted = ready(&inner) && owns_foreground(&inner, foreground);
        let keyboard = &mut inner.native_fifo_keyboard;
        keyboard.observe(identity, foreground, permitted);
        if !owns {
            return None;
        }
        Some(
            keyboard
                .ledger
                .route(input(ev, repeat), permitted && keyboard.binding.is_some()),
        )
        }
}
