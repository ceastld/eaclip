//! A bounded keyboard lease for the no-activate file line. The UI explicitly
//! arms it while the pointer is over the line, or after keyboard navigation.
//! The hook only uses cached ownership; it never queries files or invokes UI.
use super::*;

const MAX_KEYS: usize = 64;

#[derive(Clone, Debug)]
pub struct NativeFileLineKey {
    owner: Arc<()>,
    binding: u64,
    presentation: u64,
    key: &'static str,
    ctrl: bool,
}

impl NativeFileLineKey {
    pub fn presentation(&self) -> u64 { self.presentation }
    pub fn key(&self) -> &'static str { self.key }
    pub fn ctrl(&self) -> bool { self.ctrl }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Opening {
    popup: isize,
    presentation: u64,
    foreground: isize,
    foreground_epoch: u64,
}

#[derive(Clone)]
struct Press {
    event: NativeFileLineKey,
    repeatable: bool,
}

pub(super) struct NativeFileLineKeyboard {
    owner: Arc<()>,
    opening: Option<Opening>,
    binding: u64,
    retired: bool,
    regions: Vec<(i32, i32, i32, i32)>,
    inbox: VecDeque<NativeFileLineKey>,
    held: HashMap<u32, Press>,
}

impl Default for NativeFileLineKeyboard {
    fn default() -> Self {
        Self {
            owner: Arc::new(()), opening: None, binding: 0, retired: true,
            regions: Vec::new(), inbox: VecDeque::with_capacity(MAX_KEYS),
            held: HashMap::with_capacity(12),
        }
    }
}

impl NativeFileLineKeyboard {
    pub(super) fn retire(&mut self) {
        self.retired = true;
        self.inbox.clear();
        // Accepted physical presses still own their release after dismissal.
    }

    pub(super) fn reset_after_input_gap(&mut self) {
        self.retire();
        self.held.clear();
    }

    pub(super) fn observe_foreground(&mut self, foreground: isize) {
        if self.opening.is_some_and(|opening| opening.foreground != foreground
            || opening.foreground_epoch != crate::active_window::raw_foreground_epoch()) {
            self.retire();
        }
    }

    pub(super) fn outside_click(&mut self, message: u32, x: i32, y: i32) {
        if matches!(message, 0x0201 | 0x0204 | 0x020B)
            && !self.regions.iter().any(|&(left, top, width, height)| {
                i64::from(x) >= i64::from(left) && i64::from(y) >= i64::from(top)
                    && i64::from(x) < i64::from(left) + i64::from(width)
                    && i64::from(y) < i64::from(top) + i64::from(height)
            }) {
            self.retire();
        }
    }

    fn current(&self, foreground: isize, foreground_epoch: u64) -> bool {
        !self.retired && self.opening.is_some_and(|opening| {
            foreground != 0 && opening.foreground == foreground
                && opening.foreground_epoch == foreground_epoch
        })
    }

    fn push(&mut self, event: NativeFileLineKey) {
        if self.inbox.len() >= MAX_KEYS { self.retire(); }
        else { self.inbox.push_back(event); }
    }

    fn route_held(&mut self, ev: KeyEvent, repeat: bool, allowed: bool) -> bool {
        if ev.down && !repeat {
            self.held.remove(&ev.vk);
            return false;
        }
        let Some(press) = self.held.get(&ev.vk).cloned() else { return false };
        if !ev.down { self.held.remove(&ev.vk); }
        else if allowed && press.repeatable && press.event.binding == self.binding
            && command(ev).is_some_and(|(key, _)| key == press.event.key) {
            self.push(press.event);
        }
        true
    }

    fn accept(&mut self, ev: KeyEvent, repeat: bool) -> bool {
        if !ev.down || repeat { return false; }
        let Some((key, repeatable)) = command(ev) else {
            // Typing/IME or a foreground edit shortcut ends this activation.
            // A polling repaint cannot steal its following letters/digits.
            if !matches!(ev.vk, 0x10..=0x12 | 0x5B..=0x5C | 0xA0..=0xA5) {
                self.retire();
            }
            return false;
        };
        let Some(opening) = self.opening.filter(|_| !self.retired) else { return false };
        let event = NativeFileLineKey {
            owner: self.owner.clone(), binding: self.binding,
            presentation: opening.presentation, key, ctrl: ev.ctrl,
        };
        self.held.insert(ev.vk, Press { event: event.clone(), repeatable });
        self.push(event);
        true
    }
}

fn command(ev: KeyEvent) -> Option<(&'static str, bool)> {
    if ev.alt || ev.shift || ev.win { return None; }
    if ev.ctrl {
        return match ev.vk { 0x0D => Some(("Enter", false)), 0x5A => Some(("Undo", false)), _ => None };
    }
    Some(match ev.vk {
        0x08 => ("Backspace", false), 0x0D => ("Enter", false),
        0x1B => ("Escape", false), 0x20 => ("Space", false),
        0x23 => ("End", true), 0x24 => ("Home", true),
        0x25 => ("ArrowLeft", true), 0x26 => ("ArrowUp", true),
        0x27 => ("ArrowRight", true), 0x28 => ("ArrowDown", true),
        0x2E => ("Delete", false), _ => return None,
    })
}

fn ready(inner: &HostInner) -> bool {
    inner.window_runtime_ready && inner.extras.file_menu.top_edge_line
        && !inner.ole_drag_active && inner.native_incoming_drag.is_none()
        && !inner.move_interaction_active && !inner.spa_modal_open && !inner.spa_overlay_open
        && !inner.ui.clipboard_editing && inner.ui.keyboard_focus_host.is_none()
        && inner.native_context_menu.is_none()
        && !inner.qp.overlay_visible && inner.qp.machine.phase == QuickPastePhase::Idle
        && !inner.qp.mouse.is_held() && !inner.super_menu.visible && !inner.super_menu.loading
        && !inner.reopen.visible && !inner.file_browser.visible
        && !(inner.ui.panel_visible && inner.ui.panel_fake_focus)
}

impl HostState {
    /// Absolute physical rectangles are supplied after window placement. A
    /// continuous active interval is one lease: text, an outside click, or a
    /// foreground change retires it until active=false/new presentation.
    pub fn register_native_file_line_keyboard(&self, popup_hwnd: isize, presentation: u64,
        active: bool, regions: &[(i32, i32, i32, i32)]) -> bool {
        let foreground = self.native_super_menu_foreground();
        let foreground_epoch = crate::active_window::raw_foreground_epoch();
        let mut inner = self.inner.lock();
        self.reconcile_missed_input_holding(&mut inner);
        let allowed = ready(&inner);
        let keyboard = &mut inner.native_file_line_keyboard;
        if !active || popup_hwnd == 0 || regions.is_empty() {
            keyboard.retire();
            keyboard.opening = None;
            keyboard.regions.clear();
            return false;
        }
        let same = keyboard.opening.is_some_and(|opening|
            opening.popup == popup_hwnd && opening.presentation == presentation);
        if same {
            keyboard.observe_foreground(foreground);
        } else {
            keyboard.retire();
            keyboard.binding = keyboard.binding.wrapping_add(1);
            keyboard.opening = Some(Opening { popup: popup_hwnd, presentation, foreground, foreground_epoch });
            keyboard.retired = false;
        }
        if !allowed { keyboard.retire(); }
        keyboard.regions.clear();
        keyboard.regions.extend(regions.iter().copied().filter(|&(_, _, w, h)| w > 0 && h > 0).take(16));
        if keyboard.regions.is_empty() { keyboard.retire(); }
        keyboard.current(foreground, foreground_epoch)
    }

    pub fn clear_native_file_line_keyboard(&self, presentation: u64) {
        let mut inner = self.inner.lock();
        let keyboard = &mut inner.native_file_line_keyboard;
        if keyboard.opening.is_some_and(|opening| opening.presentation == presentation) {
            keyboard.retire();
            keyboard.opening = None;
            keyboard.regions.clear();
        }
    }

    pub fn drain_native_file_line_keys(&self, presentation: u64) -> Vec<NativeFileLineKey> {
        let foreground = self.native_super_menu_foreground();
        let mut inner = self.inner.lock();
        self.reconcile_missed_input_holding(&mut inner);
        let allowed = ready(&inner);
        let keyboard = &mut inner.native_file_line_keyboard;
        keyboard.observe_foreground(foreground);
        if !allowed { keyboard.retire(); }
        if !keyboard.current(foreground, crate::active_window::raw_foreground_epoch())
            || !keyboard.opening.is_some_and(|opening| opening.presentation == presentation) {
            return Vec::new();
        }
        keyboard.inbox.drain(..).collect()
    }

    pub fn native_file_line_key_is_current(&self, event: &NativeFileLineKey) -> bool {
        if self.input_gap_pending() { return false; }
        let foreground = self.native_super_menu_foreground();
        let inner = self.inner.lock();
        let keyboard = &inner.native_file_line_keyboard;
        ready(&inner) && Arc::ptr_eq(&keyboard.owner, &event.owner)
            && keyboard.current(foreground, crate::active_window::raw_foreground_epoch())
            && keyboard.binding == event.binding
            && keyboard.opening.is_some_and(|opening| opening.presentation == event.presentation)
    }

    pub(super) fn input_drain_native_file_line_key(&self, inner: &mut HostInner,
        ev: KeyEvent, foreground: isize, suspended: bool, repeat: bool) -> bool {
        let allowed = !suspended && ready(inner)
            && inner.native_file_line_keyboard.current(foreground, crate::active_window::raw_foreground_epoch());
        let keyboard = &mut inner.native_file_line_keyboard;
        if !allowed { keyboard.retire(); }
        keyboard.route_held(ev, repeat, allowed)
    }

    pub(super) fn input_route_native_file_line_key(&self, inner: &mut HostInner,
        ev: KeyEvent, foreground: isize, repeat: bool) -> Option<bool> {
        if !ready(inner) || !inner.native_file_line_keyboard.current(foreground, crate::active_window::raw_foreground_epoch()) {
            inner.native_file_line_keyboard.retire();
            return None;
        }
        // Even unsupported keys go directly to the foreground, not a hidden
        // clipboard list's fake-focus fallback.
        let swallowed = inner.native_file_line_keyboard.accept(ev, repeat);
        if !swallowed && ev.down && input_routing::external_clipboard_input_ends_digits(ev) {
            set_clipboard_keyboard_mode_locked(inner, ClipboardKeyboardMode::Released);
        }
        Some(swallowed)
    }
}

#[cfg(test)]
#[path = "native_file_line_keyboard_tests.rs"]
mod tests;
