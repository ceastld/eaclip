//! Opt-in keyboard routing for the retained, no-activate history menu. The
//! hook only checks cached ownership and appends to a bounded queue. It never
//! activates a window or executes an action.
use super::*;

const MAX_KEYS: usize = 64;
const KEY_SLOTS: usize = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeContextMenuKeyboardScope {
    pub presentation: u64,
    /// Advance for an external pointer/focus change, not for replayed keys.
    pub focus: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    lease: u64,
    popup: isize,
    owner: isize,
    window_epoch: u64,
    presentation_epoch: u64,
    incoming_drag: u64,
}
impl Identity {
    fn capture(inner: &HostInner) -> Option<Self> {
        let menu = inner.native_context_menu?;
        (menu.owner == "clipboard"
            && menu.popup_hwnd != 0
            && inner.host_hwnd != 0
            && !menu.text_input)
            .then_some(Self {
                lease: menu.id,
                popup: menu.popup_hwnd,
                owner: inner.host_hwnd,
                window_epoch: inner.chrome_input_epochs[0].window,
                presentation_epoch: inner.chrome_presentation_epochs[0],
                incoming_drag: inner.native_incoming_drag_next_id,
            })
    }
}

#[derive(Clone, Debug)]
pub struct NativeContextMenuKey {
    owner: Arc<()>,
    binding: u64,
    identity: Identity,
    scope: NativeContextMenuKeyboardScope,
    key: &'static str,
}
impl NativeContextMenuKey {
    pub fn lease(&self) -> u64 {
        self.identity.lease
    }
    pub fn key(&self) -> &'static str {
        self.key
    }
    pub fn scope(&self) -> NativeContextMenuKeyboardScope {
        self.scope
    }
}

#[derive(Clone, Copy)]
struct Opening {
    lease: u64,
    foreground: isize,
    foreground_epoch: u64,
    retired: bool,
}
#[derive(Clone, Copy)]
struct Binding {
    id: u64,
    identity: Identity,
    scope: NativeContextMenuKeyboardScope,
}
#[derive(Clone, Copy)]
struct Press {
    binding: u64,
    key: &'static str,
    repeatable: bool,
}

pub(super) struct NativeContextMenuKeyboard {
    owner: Arc<()>,
    next_binding: u64,
    opening: Option<Opening>,
    binding: Option<Binding>,
    inbox: VecDeque<NativeContextMenuKey>,
    // Closing/replacing a menu keeps only ownership of already swallowed
    // presses until their key-up. A new presentation cannot inherit a repeat.
    held: [Option<Press>; KEY_SLOTS],
}
impl Default for NativeContextMenuKeyboard {
    fn default() -> Self {
        Self {
            owner: Arc::new(()),
            next_binding: 0,
            opening: None,
            binding: None,
            inbox: VecDeque::with_capacity(MAX_KEYS),
            held: [None; KEY_SLOTS],
        }
    }
}
impl NativeContextMenuKeyboard {
    pub(super) fn begin(&mut self, lease: u64, foreground: isize, foreground_epoch: u64) {
        self.binding = None;
        self.inbox.clear();
        self.opening = Some(Opening {
            lease,
            foreground,
            foreground_epoch,
            retired: false,
        });
    }
    pub(super) fn retire(&mut self) {
        self.binding = None;
        self.inbox.clear();
        if let Some(opening) = self.opening.as_mut() {
            opening.retired = true;
        }
    }
    pub(super) fn retire_lease(&mut self, lease: u64) {
        if self.opening.is_some_and(|opening| opening.lease == lease) {
            self.retire();
        }
    }
    pub(super) fn reset_after_input_gap(&mut self) {
        self.retire();
        self.held.fill(None);
    }
    pub(super) fn observe_foreground(&mut self, foreground: isize) {
        if self
            .opening
            .is_some_and(|opening| opening.foreground != foreground)
        {
            self.retire();
        }
    }
    fn opening_current(
        &self,
        identity: Identity,
        foreground: isize,
        foreground_epoch: u64,
    ) -> bool {
        self.opening.is_some_and(|opening| {
            !opening.retired
                && opening.lease == identity.lease
                && foreground != 0
                && foreground == opening.foreground
                && foreground_epoch == opening.foreground_epoch
        })
    }
    fn current(
        &self,
        identity: Option<Identity>,
        foreground: isize,
        foreground_epoch: u64,
    ) -> bool {
        self.binding.is_some_and(|binding| {
            identity == Some(binding.identity)
                && self.opening_current(binding.identity, foreground, foreground_epoch)
        })
    }
    fn register(
        &mut self,
        identity: Identity,
        scope: NativeContextMenuKeyboardScope,
        foreground: isize,
        foreground_epoch: u64,
    ) -> bool {
        if !self.opening_current(identity, foreground, foreground_epoch) {
            return false;
        }
        if let Some(previous) = self.binding {
            if previous.identity != identity {
                self.retire();
                return false;
            }
            let delta = if scope.presentation == previous.scope.presentation {
                scope.focus.wrapping_sub(previous.scope.focus)
            } else {
                scope.presentation.wrapping_sub(previous.scope.presentation)
            };
            if delta >= (1_u64 << 63) {
                return false;
            }
            if scope == previous.scope {
                return true;
            }
        }
        self.next_binding = self.next_binding.wrapping_add(1);
        self.binding = Some(Binding {
            id: self.next_binding,
            identity,
            scope,
        });
        self.inbox.clear();
        true
    }
    fn push(&mut self, binding: Binding, key: &'static str) {
        if self.inbox.len() == MAX_KEYS {
            self.retire();
            return;
        }
        self.inbox.push_back(NativeContextMenuKey {
            owner: self.owner.clone(),
            binding: binding.id,
            identity: binding.identity,
            scope: binding.scope,
            key,
        });
    }
    fn route(&mut self, ev: KeyEvent, repeat: bool, allowed: bool) -> bool {
        if !allowed && self.binding.is_some() {
            self.retire();
        }
        let Some(slot) = slot(ev.vk) else {
            return false;
        };
        let fresh = ev.down && !repeat;
        if fresh {
            self.held[slot] = None;
        }
        if let Some(press) = self.held[slot] {
            if !ev.down {
                self.held[slot] = None;
            } else if allowed && press.repeatable {
                if let Some(binding) = self.binding.filter(|binding| binding.id == press.binding) {
                    if command(ev).is_some_and(|(key, _)| key == press.key) {
                        self.push(binding, press.key);
                    }
                }
            }
            return true;
        }
        if !fresh || !allowed {
            return false;
        }
        let Some((key, repeatable)) = command(ev) else {
            return false;
        };
        let Some(binding) = self.binding else {
            return false;
        };
        self.held[slot] = Some(Press {
            binding: binding.id,
            key,
            repeatable,
        });
        self.push(binding, key);
        true
    }
}

fn slot(vk: u32) -> Option<usize> {
    Some(match vk {
        0x09 => 0,
        0x0D => 1,
        0x1B => 2,
        0x20 => 3,
        0x23 => 4,
        0x24 => 5,
        0x25 => 6,
        0x26 => 7,
        0x27 => 8,
        0x28 => 9,
        0x2E => 10,
        0x43 => 11,
        0x50 => 12,
        _ => return None,
    })
}
fn command(ev: KeyEvent) -> Option<(&'static str, bool)> {
    if ev.ctrl && !ev.alt && !ev.shift && !ev.win {
        return match ev.vk {
            0x43 => Some(("ctrl-c", false)),
            0x50 => Some(("ctrl-p", false)),
            _ => None,
        };
    }
    if ev.ctrl || ev.alt || ev.win || (ev.shift && ev.vk != 0x09) {
        return None;
    }
    Some(match ev.vk {
        0x09 => ("tab", false),
        0x0D => ("enter", false),
        0x1B => ("escape", false),
        0x20 => ("space", false),
        0x23 => ("end", true),
        0x24 => ("home", true),
        0x25 => ("left", true),
        0x26 => ("up", true),
        0x27 => ("right", true),
        0x28 => ("down", true),
        0x2E => ("delete", false),
        _ => return None,
    })
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
        && !inner.native_fifo_keyboard.owns_presentation(inner)
}

impl HostState {
    pub(super) fn native_context_menu_foreground(&self) -> isize {
        if crate::clipboard::is_headless() {
            self.inner.lock().last_fg
        } else {
            crate::window_menu::foreground_hwnd()
        }
    }
    /// Explicit clipboard-menu opt-in. Ordinary menus keep generic Escape.
    /// Register after setting its HWND; registration never activates a window.
    pub fn register_native_context_menu_keyboard(
        &self,
        lease: u64,
        popup_hwnd: isize,
        scope: NativeContextMenuKeyboardScope,
    ) -> bool {
        let foreground = self.native_context_menu_foreground();
        let foreground_epoch = crate::active_window::raw_foreground_epoch();
        let mut inner = self.inner.lock();
        self.reconcile_missed_input_holding(&mut inner);
        let Some(identity) = Identity::capture(&inner)
            .filter(|identity| identity.lease == lease && identity.popup == popup_hwnd)
        else {
            return false;
        };
        if !ready(&inner) {
            inner.native_context_menu_keyboard.retire();
            return false;
        }
        inner
            .native_context_menu_keyboard
            .register(identity, scope, foreground, foreground_epoch)
    }
    pub fn clear_native_context_menu_keyboard(&self, lease: u64) {
        self.inner
            .lock()
            .native_context_menu_keyboard
            .retire_lease(lease);
    }
    pub fn drain_native_context_menu_keys(&self, lease: u64) -> Vec<NativeContextMenuKey> {
        let foreground = self.native_context_menu_foreground();
        let foreground_epoch = crate::active_window::raw_foreground_epoch();
        let mut inner = self.inner.lock();
        self.reconcile_missed_input_holding(&mut inner);
        if !inner
            .native_context_menu_keyboard
            .opening
            .is_some_and(|opening| opening.lease == lease)
        {
            return Vec::new();
        }
        if !ready(&inner)
            || !inner.native_context_menu_keyboard.current(
                Identity::capture(&inner),
                foreground,
                foreground_epoch,
            )
        {
            inner.native_context_menu_keyboard.retire();
            return Vec::new();
        }
        inner.native_context_menu_keyboard.inbox.drain(..).collect()
    }
    pub fn native_context_menu_key_is_current(&self, event: &NativeContextMenuKey) -> bool {
        let foreground = self.native_context_menu_foreground();
        let foreground_epoch = crate::active_window::raw_foreground_epoch();
        let mut inner = self.inner.lock();
        self.reconcile_missed_input_holding(&mut inner);
        let keyboard = &inner.native_context_menu_keyboard;
        ready(&inner)
            && Arc::ptr_eq(&keyboard.owner, &event.owner)
            && keyboard.current(Identity::capture(&inner), foreground, foreground_epoch)
            && keyboard.binding.is_some_and(|binding| {
                binding.id == event.binding
                    && binding.identity == event.identity
                    && binding.scope == event.scope
            })
    }
    /// Runs before generic context-menu Escape, including held-key drain after
    /// closure. No Win32 queries, UI callbacks or actions occur in this path.
    pub(super) fn input_route_native_context_menu_key(
        &self,
        inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        suspended: bool,
        repeat: bool,
    ) -> bool {
        let allowed = !suspended
            && ready(inner)
            && !inner.native_context_menu_escape_down
            && inner.native_context_menu_keyboard.current(
                Identity::capture(inner),
                foreground,
                crate::active_window::raw_foreground_epoch(),
            );
        let swallowed = inner
            .native_context_menu_keyboard
            .route(ev, repeat, allowed);
        if swallowed && ev.vk == 0x1B && !ev.down {
            inner.native_context_menu_escape_down = false;
        }
        swallowed
    }
}

#[cfg(test)]
#[path = "native_context_menu_keyboard_tests.rs"]
mod tests;
