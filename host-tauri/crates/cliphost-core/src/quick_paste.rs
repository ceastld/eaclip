//! Quick-paste chord / session machine aligned with
//! `src/IntelliTools.ClipHost/Hosting/QuickPaste-commit-logic.md`.

use std::collections::HashSet;
use std::time::Duration;

use crate::model::ClipRecord;

pub const MIN_LIST_LIMIT: i32 = 3;
pub const MAX_LIST_LIMIT: i32 = 50;
pub const DEFAULT_LIST_LIMIT: i32 = 25;
pub const MODIFIER_LATCH_MS: u64 = 280;

pub const VK_SHIFT: u32 = 0x10;
pub const VK_CONTROL: u32 = 0x11;
pub const VK_MENU: u32 = 0x12;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_1: u32 = 0x31;
pub const VK_2: u32 = 0x32;
pub const VK_D: u32 = 0x44;
pub const VK_E: u32 = 0x45;
pub const VK_F: u32 = 0x46;
pub const VK_Q: u32 = 0x51;
pub const VK_LSHIFT: u32 = 0xA0;
pub const VK_RSHIFT: u32 = 0xA1;
pub const VK_LCONTROL: u32 = 0xA2;
pub const VK_RCONTROL: u32 = 0xA3;
pub const VK_LMENU: u32 = 0xA4;
pub const VK_RMENU: u32 = 0xA5;
pub const VK_LWIN: u32 = 0x5B;
pub const VK_RWIN: u32 = 0x5C;
pub const VK_OEM_3: u32 = 0xC0; // `
pub const SESSION_HARD_TIMEOUT: Duration = Duration::from_secs(120);
/// C# `ClipHostQuickPasteOverlayWebViewWindow` wheel hit slop.
pub const WHEEL_HIT_SLOP_PX: i32 = 24;
/// C# `ClipHostQuickPasteOverlayWebViewWindow.StatePushBurstCount`.
pub const STATE_PUSH_BURST_COUNT: u8 = 6;
/// C# `StatePushBurstInterval`.
pub const STATE_PUSH_BURST_INTERVAL: Duration = Duration::from_millis(40);
/// C# `ClipHostQuickPasteOverlayWebViewWindow.PlacementRevealTimeout`.
pub const PLACEMENT_REVEAL_TIMEOUT: Duration = Duration::from_millis(220);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickPastePhase {
    Idle,
    ChordEngaged,
    SessionStarting,
    SessionActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookVerdict {
    Pass,
    Swallow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickPasteCommand {
    None,
    BeginSession,
    Commit {
        paste_enter: bool,
    },
    Cancel,
    MoveSelection {
        delta: i32,
    },
    ArmRowAction {
        shortcut: usize,
    },
    ArmPasteEnter,
    DeleteActive,
    ArmFifo,
    /// Host-queued Esc cancellation while no quick-paste session owns input.
    CancelFifo {
        generation: u64,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub vk: u32,
    pub down: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
    pub now_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickPasteMouseButton {
    X1,
    X2,
}

impl QuickPasteMouseButton {
    pub fn parse_config(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "x1" => Some(Self::X1),
            "x2" => Some(Self::X2),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseGestureAction {
    None,
    Armed,
    BeginSession,
    MoveSelection { delta: i32 },
    Commit,
}

/// The selected side button is a modifier for the wheel. Down passes, as does
/// Up without wheel input, so standalone clicks and holds keep working. Once
/// the wheel engages the chord, its paired Up is consumed even after cancel.
#[derive(Debug, Clone, Default)]
pub struct QuickPasteMouseGesture {
    configured: Option<QuickPasteMouseButton>,
    held: Option<QuickPasteMouseButton>,
    engaged: bool,
    draining: bool,
    swallow_release: bool,
}

impl QuickPasteMouseGesture {
    pub fn configure(&mut self, button: Option<QuickPasteMouseButton>) -> bool {
        if self.configured == button {
            return false;
        }
        self.configured = button;
        self.cancel();
        true
    }

    pub fn held_button(&self) -> Option<QuickPasteMouseButton> {
        self.held
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }

    pub fn is_engaged(&self) -> bool {
        self.engaged
    }

    pub fn is_draining(&self) -> bool {
        self.draining
    }

    pub fn cancel(&mut self) {
        self.engaged = false;
        // A canceled press neither commits nor re-arms before its physical Up.
        // Keep ownership of Up when this press already consumed wheel input.
        self.draining = self.held.is_some();
    }

    pub fn reset_after_input_gap(&mut self) {
        // Up may already have passed while the callback could not take the
        // host lock. Retire the press so a later ordinary click is not drained.
        self.held = None;
        self.engaged = false;
        self.draining = false;
        self.swallow_release = false;
    }

    /// Consume only the paired release of a press that used the wheel chord.
    pub fn handle_button(
        &mut self,
        button: QuickPasteMouseButton,
        down: bool,
    ) -> (HookVerdict, MouseGestureAction) {
        if let Some(held) = self.held {
            if button != held || down {
                return (HookVerdict::Pass, MouseGestureAction::None);
            }
            let action = if self.engaged && !self.draining {
                MouseGestureAction::Commit
            } else {
                MouseGestureAction::None
            };
            let verdict = if self.swallow_release {
                HookVerdict::Swallow
            } else {
                HookVerdict::Pass
            };
            self.held = None;
            self.engaged = false;
            self.draining = false;
            self.swallow_release = false;
            return (verdict, action);
        }
        if !down || self.configured != Some(button) {
            return (HookVerdict::Pass, MouseGestureAction::None);
        }
        self.held = Some(button);
        self.engaged = false;
        self.draining = false;
        self.swallow_release = false;
        (HookVerdict::Pass, MouseGestureAction::Armed)
    }

    pub fn handle_wheel(&mut self, delta: i16) -> (HookVerdict, MouseGestureAction) {
        if delta == 0 || self.held.is_none() || self.draining {
            return (HookVerdict::Pass, MouseGestureAction::None);
        }
        if !self.engaged {
            self.engaged = true;
            self.swallow_release = true;
            return (HookVerdict::Swallow, MouseGestureAction::BeginSession);
        }
        (
            HookVerdict::Swallow,
            MouseGestureAction::MoveSelection {
                delta: mouse_wheel_selection_delta(delta),
            },
        )
    }
}

#[derive(Debug, Clone)]
pub struct QuickPasteMachine {
    pub phase: QuickPastePhase,
    pub commit_when_session_opens: bool,
    pub trigger_released_during_session: bool,
    pub paste_enter_armed: bool,
    paste_enter_shortcut_enabled: bool,
    #[allow(dead_code)]
    last_mod_down_ms: Option<u64>,
    last_mod_up_ms: Option<u64>,
    trigger_vk: u32,
    require_ctrl: bool,
    require_alt: bool,
    require_shift: bool,
    require_win: bool,
    trigger_press_count: i32,
    trigger_key_down: bool,
    trigger_release_owned: bool,
    last_trigger_down_ms: Option<u64>,
    session_keys_down: HashSet<u32>,
    passed_session_keys_down: HashSet<u32>,
    chord_drain_active: bool,
}

impl Default for QuickPasteMachine {
    fn default() -> Self {
        Self::new(VK_OEM_3, true)
    }
}

impl QuickPasteMachine {
    pub fn new(trigger_vk: u32, require_alt: bool) -> Self {
        Self::with_required(trigger_vk, false, require_alt, false, false)
    }

    /// C# `QuickPasteHotkeyMatcher` required modifiers from the configured chord.
    pub fn with_required(
        trigger_vk: u32,
        require_ctrl: bool,
        require_alt: bool,
        require_shift: bool,
        require_win: bool,
    ) -> Self {
        Self {
            phase: QuickPastePhase::Idle,
            commit_when_session_opens: false,
            trigger_released_during_session: false,
            paste_enter_armed: false,
            paste_enter_shortcut_enabled: true,
            last_mod_down_ms: None,
            last_mod_up_ms: None,
            trigger_vk,
            require_ctrl,
            require_alt,
            require_shift,
            require_win,
            trigger_press_count: 0,
            trigger_key_down: false,
            trigger_release_owned: false,
            last_trigger_down_ms: None,
            session_keys_down: HashSet::new(),
            passed_session_keys_down: HashSet::new(),
            chord_drain_active: false,
        }
    }

    pub fn chord_drain_active(&self) -> bool {
        self.chord_drain_active
    }

    pub fn set_chord_drain_active(&mut self, active: bool) {
        self.chord_drain_active = active;
    }

    /// An input gap kept this session open but may have passed a key-up
    /// through unseen. Forget per-key held state so the next press is not
    /// mistaken for auto-repeat and ignored.
    pub fn forget_held_keys_after_input_gap(&mut self) {
        self.trigger_key_down = false;
        self.trigger_release_owned = false;
        self.session_keys_down.clear();
        self.passed_session_keys_down.clear();
    }

    /// C# `QuickPasteCommitPolicy.ShouldSwallowTriggerKeyUp`.
    pub fn should_swallow_trigger_key_up(session_active: bool, chord_drain_active: bool) -> bool {
        session_active || chord_drain_active
    }

    fn session_swallows_trigger_up(&self) -> bool {
        matches!(
            self.phase,
            QuickPastePhase::SessionActive
                | QuickPastePhase::SessionStarting
                | QuickPastePhase::ChordEngaged
        )
    }

    pub fn trigger_press_index(&self) -> i32 {
        self.trigger_press_count.saturating_sub(1)
    }

    pub fn requires_alt(&self) -> bool {
        self.require_alt
    }

    pub fn set_paste_enter_shortcut_enabled(&mut self, enabled: bool) {
        self.paste_enter_shortcut_enabled = enabled;
    }

    pub(crate) fn has_passed_session_key(&self, vk: u32) -> bool {
        self.passed_session_keys_down.contains(&vk)
    }

    pub fn requires_shortcut_menu_disarm(&self) -> bool {
        self.require_alt || self.require_win
    }

    pub fn trigger_vk(&self) -> u32 {
        self.trigger_vk
    }

    /// Compare configured input, without discarding an ongoing chord/session.
    pub fn has_same_binding(&self, other: &Self) -> bool {
        self.trigger_vk == other.trigger_vk
            && self.require_ctrl == other.require_ctrl
            && self.require_alt == other.require_alt
            && self.require_shift == other.require_shift
            && self.require_win == other.require_win
    }

    /// Keep the configured chord; drop session/chord flags (C# EndSession).
    pub fn reset_session(&mut self) {
        // Session teardown cannot give the foreground an Up whose Down we
        // swallowed, or turn a still-held pass-through key into a new shortcut.
        let trigger_key_down = self.trigger_key_down;
        let trigger_release_owned = self.trigger_release_owned;
        let passed_session_keys_down = std::mem::take(&mut self.passed_session_keys_down);
        let paste_enter_shortcut_enabled = self.paste_enter_shortcut_enabled;
        *self = Self::with_required(
            self.trigger_vk,
            self.require_ctrl,
            self.require_alt,
            self.require_shift,
            self.require_win,
        );
        self.trigger_key_down = trigger_key_down;
        self.trigger_release_owned = trigger_release_owned;
        self.passed_session_keys_down = passed_session_keys_down;
        self.paste_enter_shortcut_enabled = paste_enter_shortcut_enabled;
    }

    pub fn begin_mouse_session(&mut self) {
        self.reset_session();
        self.phase = QuickPastePhase::SessionStarting;
        self.trigger_press_count = 1;
    }

    /// Mouse sessions use the same explicit actions without borrowing the
    /// keyboard chord's trigger, modifier-release commit, or modifier latch.
    pub fn handle_mouse_key(&mut self, ev: KeyEvent) -> (HookVerdict, QuickPasteCommand) {
        if !ev.down {
            self.session_keys_down.remove(&ev.vk);
            self.passed_session_keys_down.remove(&ev.vk);
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }
        if self.passed_session_keys_down.contains(&ev.vk) {
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }
        let unmodified = !ev.alt && !ev.ctrl && !ev.shift && !ev.win;
        if unmodified
            && ev.vk == VK_ESCAPE
            && matches!(
                self.phase,
                QuickPastePhase::SessionStarting | QuickPastePhase::SessionActive
            )
        {
            return (HookVerdict::Swallow, self.cancel());
        }
        if unmodified && matches!(self.phase, QuickPastePhase::SessionStarting | QuickPastePhase::SessionActive) {
            return self.handle_session_key_down(ev.vk);
        }
        self.remember_passed_session_key(ev.vk);
        (HookVerdict::Pass, QuickPasteCommand::None)
    }

    pub fn on_session_ui_ready(&mut self) -> QuickPasteCommand {
        if matches!(
            self.phase,
            QuickPastePhase::ChordEngaged | QuickPastePhase::SessionStarting
        ) {
            self.phase = QuickPastePhase::SessionActive;
        }
        if self.commit_when_session_opens {
            self.commit_when_session_opens = false;
            return self.commit();
        }
        QuickPasteCommand::None
    }

    pub fn poll_backup_modifiers(&mut self, modifiers_down: bool) -> QuickPasteCommand {
        // A swallowed bare trigger is not observable through GetAsyncKeyState.
        // Its hook Up commits directly; an input gap cancels the uncertain press.
        if !self.has_required_modifiers()
            || self.phase != QuickPastePhase::SessionActive
            || !self.trigger_released_during_session
        {
            return QuickPasteCommand::None;
        }
        if !modifiers_down {
            return self.commit();
        }
        QuickPasteCommand::None
    }

    pub fn handle(&mut self, ev: KeyEvent) -> (HookVerdict, QuickPasteCommand) {
        if self.is_required_modifier(ev.vk) {
            if ev.down {
                self.last_mod_down_ms = Some(ev.now_ms);
            } else {
                self.last_mod_up_ms = Some(ev.now_ms);
                return self.on_required_modifier_up();
            }
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }

        if ev.vk == self.trigger_vk {
            if ev.down {
                return self.on_trigger_down(&ev);
            }
            return self.on_trigger_up();
        }

        if !ev.down {
            self.session_keys_down.remove(&ev.vk);
            self.passed_session_keys_down.remove(&ev.vk);
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }
        if self.passed_session_keys_down.contains(&ev.vk) {
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }

        if ev.vk == VK_ESCAPE
            && !self.forbidden_down(&ev)
            && matches!(
                self.phase,
                QuickPastePhase::ChordEngaged
                    | QuickPastePhase::SessionStarting
                    | QuickPastePhase::SessionActive
            )
        {
            return (HookVerdict::Swallow, self.cancel());
        }

        if matches!(self.phase, QuickPastePhase::SessionStarting | QuickPastePhase::SessionActive)
            && self.required_down_strict(&ev)
            && !self.forbidden_down(&ev)
        {
            return self.handle_session_key_down(ev.vk);
        }

        self.remember_passed_session_key(ev.vk);
        (HookVerdict::Pass, QuickPasteCommand::None)
    }

    fn remember_passed_session_key(&mut self, vk: u32) {
        if matches!(vk, VK_ESCAPE | VK_E | VK_D | VK_F | VK_Q | VK_1 | VK_2 | VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT) {
            // A chord passed to the foreground owns its repeats until Up,
            // even if its modifiers change or quick paste becomes ready.
            self.passed_session_keys_down.insert(vk);
        }
    }

    fn handle_session_key_down(&mut self, vk: u32) -> (HookVerdict, QuickPasteCommand) {
        if vk == VK_E && !self.paste_enter_shortcut_enabled {
            // A previously accepted press keeps its repeats, but a disabled
            // fresh E belongs to the foreground until its matching release.
            if self.session_keys_down.contains(&vk) {
                return (HookVerdict::Swallow, QuickPasteCommand::None);
            }
            self.remember_passed_session_key(vk);
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }
        let first = self.session_keys_down.insert(vk);
        if vk == VK_E {
            if first {
                self.paste_enter_armed = true;
                return (HookVerdict::Swallow, QuickPasteCommand::ArmPasteEnter);
            }
            return (HookVerdict::Swallow, QuickPasteCommand::None);
        }
        if vk == VK_D {
            return (
                HookVerdict::Swallow,
                if first {
                    QuickPasteCommand::DeleteActive
                } else {
                    QuickPasteCommand::None
                },
            );
        }
        if vk == VK_F {
            return (
                HookVerdict::Swallow,
                if first {
                    QuickPasteCommand::ArmFifo
                } else {
                    QuickPasteCommand::None
                },
            );
        }
        if vk == VK_1 || vk == VK_2 {
            return (
                HookVerdict::Swallow,
                if first {
                    QuickPasteCommand::ArmRowAction {
                        shortcut: (vk - VK_1 + 1) as usize,
                    }
                } else {
                    QuickPasteCommand::None
                },
            );
        }
        if vk == VK_UP || vk == VK_Q {
            return (
                HookVerdict::Swallow,
                if first {
                    QuickPasteCommand::MoveSelection { delta: -1 }
                } else {
                    QuickPasteCommand::None
                },
            );
        }
        if vk == VK_DOWN {
            return (
                HookVerdict::Swallow,
                if first {
                    QuickPasteCommand::MoveSelection { delta: 1 }
                } else {
                    QuickPasteCommand::None
                },
            );
        }
        if vk == VK_LEFT || vk == VK_RIGHT {
            return (HookVerdict::Swallow, QuickPasteCommand::None);
        }

        (HookVerdict::Pass, QuickPasteCommand::None)
    }

    fn on_required_modifier_up(&mut self) -> (HookVerdict, QuickPasteCommand) {
        self.try_clear_chord_drain();
        match self.phase {
            QuickPastePhase::SessionActive => (HookVerdict::Pass, self.commit()),
            QuickPastePhase::ChordEngaged | QuickPastePhase::SessionStarting => {
                self.commit_when_session_opens = true;
                (HookVerdict::Pass, QuickPasteCommand::None)
            }
            QuickPastePhase::Idle => (HookVerdict::Pass, QuickPasteCommand::None),
        }
    }

    fn on_trigger_down(&mut self, ev: &KeyEvent) -> (HookVerdict, QuickPasteCommand) {
        // C#: auto-repeat downs without KeyUp are not extra chord presses.
        if self.trigger_key_down {
            return (
                if self.trigger_release_owned { HookVerdict::Swallow } else { HookVerdict::Pass },
                QuickPasteCommand::None,
            );
        }
        self.trigger_key_down = true;
        if !self.modifiers_ok(ev) {
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }
        self.trigger_release_owned = true;
        // C#: WM_KEYDOWN + WM_SYSKEYDOWN share the same eventTime.
        if ev.now_ms != 0 && self.last_trigger_down_ms == Some(ev.now_ms) {
            return (HookVerdict::Swallow, QuickPasteCommand::None);
        }
        self.last_trigger_down_ms = Some(ev.now_ms);
        self.trigger_press_count = self.trigger_press_count.saturating_add(1);
        if self.phase == QuickPastePhase::Idle {
            // The previous commit may still be queued on the host thread.
            // Each new chord starts at its first press regardless of that queue.
            self.trigger_press_count = 1;
            self.phase = QuickPastePhase::SessionStarting;
            // A latched trigger can arrive after the physical modifier release.
            self.commit_when_session_opens = !self.required_down_strict(ev);
            return (HookVerdict::Swallow, QuickPasteCommand::BeginSession);
        }
        if matches!(
            self.phase,
            QuickPastePhase::SessionActive | QuickPastePhase::SessionStarting
        ) {
            return (
                HookVerdict::Swallow,
                QuickPasteCommand::MoveSelection { delta: 1 },
            );
        }
        (HookVerdict::Swallow, QuickPasteCommand::None)
    }

    fn on_trigger_up(&mut self) -> (HookVerdict, QuickPasteCommand) {
        self.trigger_key_down = false;
        let owned = std::mem::take(&mut self.trigger_release_owned);
        if !owned {
            self.try_clear_chord_drain();
            return (HookVerdict::Pass, QuickPasteCommand::None);
        }
        let session = self.session_swallows_trigger_up();
        if Self::should_swallow_trigger_key_up(session, self.chord_drain_active || owned) {
            // A bare-key binding has no modifier release to finish its session.
            // Its trigger is the held key, including release before loading ends.
            if session && !self.has_required_modifiers() {
                return match self.phase {
                    QuickPastePhase::SessionActive => (HookVerdict::Swallow, self.commit()),
                    QuickPastePhase::ChordEngaged | QuickPastePhase::SessionStarting => {
                        self.commit_when_session_opens = true;
                        (HookVerdict::Swallow, QuickPasteCommand::None)
                    }
                    QuickPastePhase::Idle => unreachable!(),
                };
            }
            if self.phase == QuickPastePhase::SessionActive {
                self.trigger_released_during_session = true;
            } else {
                self.try_clear_chord_drain();
            }
            return (HookVerdict::Swallow, QuickPasteCommand::None);
        }
        self.try_clear_chord_drain();
        (HookVerdict::Pass, QuickPasteCommand::None)
    }

    fn modifiers_ok(&self, ev: &KeyEvent) -> bool {
        if self.forbidden_down(ev) {
            return false;
        }
        if self.required_down_strict(ev) {
            return true;
        }
        if let Some(up) = self.last_mod_up_ms {
            return ev.now_ms.saturating_sub(up) <= MODIFIER_LATCH_MS;
        }
        false
    }

    fn forbidden_down(&self, ev: &KeyEvent) -> bool {
        (!self.require_ctrl && ev.ctrl)
            || (!self.require_alt && ev.alt)
            || (!self.require_shift && ev.shift)
            || (!self.require_win && ev.win)
    }

    fn required_down_strict(&self, ev: &KeyEvent) -> bool {
        (!self.require_ctrl || ev.ctrl)
            && (!self.require_alt || ev.alt)
            && (!self.require_shift || ev.shift)
            && (!self.require_win || ev.win)
    }

    fn is_required_modifier(&self, vk: u32) -> bool {
        (self.require_ctrl && matches!(vk, VK_CONTROL | VK_LCONTROL | VK_RCONTROL))
            || (self.require_alt && matches!(vk, VK_MENU | VK_LMENU | VK_RMENU))
            || (self.require_shift && matches!(vk, VK_SHIFT | VK_LSHIFT | VK_RSHIFT))
            || (self.require_win && matches!(vk, VK_LWIN | VK_RWIN))
    }

    fn has_required_modifiers(&self) -> bool {
        self.require_ctrl || self.require_alt || self.require_shift || self.require_win
    }

    fn trigger_physically_down(&self) -> bool {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            GetAsyncKeyState(self.trigger_vk as i32) < 0
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// C# `QuickPasteHotkeyMatcher.AreRequiredModifiersDown` via `GetAsyncKeyState`.
    /// Bare triggers are swallowed, so no physical modifier can prove they are held.
    pub fn required_modifiers_physically_down_for(&self) -> bool {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            self.held_for_release_with(|vk| GetAsyncKeyState(vk as i32) < 0)
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            false
        }
    }

    #[cfg(any(windows, test))]
    fn held_for_release_with(&self, mut down: impl FnMut(u32) -> bool) -> bool {
        if !self.has_required_modifiers() {
            return false;
        }
        (!self.require_ctrl || down(VK_CONTROL) || down(VK_LCONTROL) || down(VK_RCONTROL))
            && (!self.require_alt || down(VK_MENU) || down(VK_LMENU) || down(VK_RMENU))
            && (!self.require_shift || down(VK_SHIFT) || down(VK_LSHIFT) || down(VK_RSHIFT))
            && (!self.require_win || down(VK_LWIN) || down(VK_RWIN))
    }

    /// C# `QuickPasteHotkeyMatcher.IsChordFullyReleased`.
    pub fn is_chord_fully_released(&self) -> bool {
        if !self.has_required_modifiers() {
            return !self.trigger_key_down;
        }
        !self.trigger_physically_down() && !self.required_modifiers_physically_down_for()
    }

    fn try_clear_chord_drain(&mut self) {
        if self.chord_drain_active && self.is_chord_fully_released() {
            self.chord_drain_active = false;
        }
    }

    fn commit(&mut self) -> QuickPasteCommand {
        let paste_enter = self.paste_enter_armed;
        self.reset();
        QuickPasteCommand::Commit { paste_enter }
    }

    fn cancel(&mut self) -> QuickPasteCommand {
        self.reset();
        QuickPasteCommand::Cancel
    }

    fn reset(&mut self) {
        self.phase = QuickPastePhase::Idle;
        self.commit_when_session_opens = false;
        self.trigger_released_during_session = false;
        self.paste_enter_armed = false;
    }
}

pub fn clamp_list_limit(limit: i32) -> i32 {
    limit.clamp(MIN_LIST_LIMIT, MAX_LIST_LIMIT)
}

/// Recent non-pinned first (capped), then all pinned — C# `QuickPasteSessionItemOrder.SelectItems`.
pub fn select_session_items(source: &[ClipRecord], limit: i32) -> Vec<ClipRecord> {
    let limit = clamp_list_limit(limit) as usize;
    let mut recent: Vec<ClipRecord> = source.iter().filter(|i| !i.is_top).cloned().collect();
    recent.sort_by(|a, b| {
        b.create_time
            .cmp(&a.create_time)
            .then_with(|| a.sub_index.cmp(&b.sub_index))
    });
    recent.truncate(limit);
    let mut pinned: Vec<ClipRecord> = source.iter().filter(|i| i.is_top).cloned().collect();
    pinned.sort_by(|a, b| {
        b.create_time
            .cmp(&a.create_time)
            .then_with(|| a.sub_index.cmp(&b.sub_index))
    });
    recent.extend(pinned);
    recent
}

pub fn get_display_number(ordered: &[ClipRecord], list_index: usize) -> i32 {
    let Some(item) = ordered.get(list_index) else {
        return (list_index as i32) + 1;
    };
    if !item.is_top {
        return ordered
            .iter()
            .take(list_index + 1)
            .filter(|i| !i.is_top)
            .count() as i32;
    }
    let pinned_count = ordered.iter().filter(|i| i.is_top).count() as i32;
    let pinned_position = ordered.iter().take(list_index).filter(|i| i.is_top).count() as i32;
    pinned_count - pinned_position
}

/// Resolve from the next unconsumed selection and this chord's trigger presses.
pub fn resolve_chord_selection_index(
    press_index: i32,
    item_count: i32,
    remember_selection: bool,
    selection_memory_seconds: i32,
    resume_index: i32,
    remembered_age_secs: f64,
) -> usize {
    if item_count <= 0 {
        return 0;
    }
    let press_index = press_index.max(0);
    let mut base_index = 0i32;
    if remember_selection
        && selection_memory_seconds > 0
        && remembered_age_secs <= f64::from(selection_memory_seconds)
        && resume_index >= 0
        && resume_index <= item_count - 1
    {
        base_index = resume_index;
    }
    (base_index + press_index).rem_euclid(item_count) as usize
}

/// C# `ClipHostQuickPasteService.ResolvePasteTargetHwnd` / `IsValidPasteTarget`.
pub fn is_valid_paste_target(hwnd: isize, overlay: isize, visible: bool) -> bool {
    hwnd != 0 && hwnd != overlay && visible
}

pub fn resolve_paste_target_hwnd(
    captured: isize,
    captured_visible: bool,
    foreground: isize,
    foreground_visible: bool,
    last_foreground: isize,
    last_visible: bool,
    overlay: isize,
) -> isize {
    if is_valid_paste_target(captured, overlay, captured_visible) {
        return captured;
    }
    if is_valid_paste_target(foreground, overlay, foreground_visible) {
        return foreground;
    }
    if is_valid_paste_target(last_foreground, overlay, last_visible) {
        return last_foreground;
    }
    0
}

/// C# `QuickPasteCommitPolicy.ShouldConsumeMouseWheel`.
pub fn should_consume_mouse_wheel(
    session_active: bool,
    overlay_ready: bool,
    cursor_inside_overlay: bool,
) -> bool {
    session_active && overlay_ready && cursor_inside_overlay
}

/// C# overlay wheel: positive delta (up) selects previous row.
pub fn mouse_wheel_selection_delta(wheel_delta: i16) -> i32 {
    if wheel_delta > 0 {
        -1
    } else if wheel_delta < 0 {
        1
    } else {
        0
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QpWheelHitRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub visible: bool,
}

/// Convert a logical cluster to the visible physical client area. Validate the
/// original geometry before clipping so an overflow cannot become a valid hit.
pub(crate) fn placement_wheel_hit(
    origin: Option<(i32, i32, i32, i32)>,
    cluster: Option<[f64; 4]>,
    viewport: Option<[f64; 2]>,
    dpr: f64,
    visible: bool,
) -> Result<QpWheelHitRect, &'static str> {
    const INVALID: &str = "invalid-placement";
    fn scaled(value: f64, dpr: f64) -> Result<f64, &'static str> {
        let value = value * dpr;
        if value.is_finite() && value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX) {
            Ok(value)
        } else {
            Err(INVALID)
        }
    }
    if !dpr.is_finite() || dpr <= 0.0 {
        return Err(INVALID);
    }
    let viewport = viewport
        .map(|[width, height]| {
            if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                return Err(INVALID);
            }
            Ok([scaled(width, dpr)?, scaled(height, dpr)?])
        })
        .transpose()?;
    let cluster = cluster
        .map(|[left, top, width, height]| {
            if ![left, top, width, height].into_iter().all(f64::is_finite)
                || width <= 0.0
                || height <= 0.0
            {
                return Err(INVALID);
            }
            // Check raw sizes as well as both endpoints, even when clipping would
            // hide the invalid portion of a source report.
            scaled(width, dpr)?;
            scaled(height, dpr)?;
            Ok([
                scaled(left, dpr)?,
                scaled(top, dpr)?,
                scaled(left + width, dpr)?,
                scaled(top + height, dpr)?,
            ])
        })
        .transpose()?;
    let Some((left, top, width, height)) = origin else {
        return if cluster.is_some() {
            Err(INVALID)
        } else {
            Ok(QpWheelHitRect::default())
        };
    };
    if width <= 0 || height <= 0 {
        return Err(INVALID);
    }
    // An ordinary full-window fallback must also have representable endpoints.
    i32::try_from(
        i64::from(left)
            .checked_add(i64::from(width))
            .ok_or(INVALID)?,
    )
    .map_err(|_| INVALID)?;
    i32::try_from(
        i64::from(top)
            .checked_add(i64::from(height))
            .ok_or(INVALID)?,
    )
    .map_err(|_| INVALID)?;
    let Some([cluster_left, cluster_top, cluster_right, cluster_bottom]) = cluster else {
        return Ok(QpWheelHitRect {
            left,
            top,
            width,
            height,
            visible,
        });
    };
    let [viewport_width, viewport_height] =
        viewport.unwrap_or([f64::from(width), f64::from(height)]);
    let physical_left = cluster_left.max(0.0).round() as i64;
    let physical_top = cluster_top.max(0.0).round() as i64;
    let physical_right = cluster_right
        .min(viewport_width)
        .min(f64::from(width))
        .round() as i64;
    let physical_bottom = cluster_bottom
        .min(viewport_height)
        .min(f64::from(height))
        .round() as i64;
    let clipped_width = physical_right.checked_sub(physical_left).ok_or(INVALID)?;
    let clipped_height = physical_bottom.checked_sub(physical_top).ok_or(INVALID)?;
    if clipped_width <= 0 || clipped_height <= 0 {
        return Err(INVALID);
    }
    let screen_left = i64::from(left).checked_add(physical_left).ok_or(INVALID)?;
    let screen_top = i64::from(top).checked_add(physical_top).ok_or(INVALID)?;
    let screen_right = i64::from(left).checked_add(physical_right).ok_or(INVALID)?;
    let screen_bottom = i64::from(top).checked_add(physical_bottom).ok_or(INVALID)?;
    i32::try_from(screen_right).map_err(|_| INVALID)?;
    i32::try_from(screen_bottom).map_err(|_| INVALID)?;
    Ok(QpWheelHitRect {
        left: i32::try_from(screen_left).map_err(|_| INVALID)?,
        top: i32::try_from(screen_top).map_err(|_| INVALID)?,
        width: i32::try_from(clipped_width).map_err(|_| INVALID)?,
        height: i32::try_from(clipped_height).map_err(|_| INVALID)?,
        visible,
    })
}

pub fn point_inside_wheel_hit(x: i32, y: i32, hit: &QpWheelHitRect, slop: i32) -> bool {
    if !hit.visible || hit.width <= 0 || hit.height <= 0 || slop < 0 {
        return false;
    }
    let (x, y, slop) = (i64::from(x), i64::from(y), i64::from(slop));
    let (left, top) = (i64::from(hit.left), i64::from(hit.top));
    x >= left - slop
        && x < left + i64::from(hit.width) + slop
        && y >= top - slop
        && y < top + i64::from(hit.height) + slop
}

#[cfg(test)]
mod wheel_geometry_tests {
    use super::*;

    fn bounds(hit: QpWheelHitRect) -> (i32, i32, i32, i32, bool) {
        (hit.left, hit.top, hit.width, hit.height, hit.visible)
    }

    #[test]
    fn placement_geometry_keeps_negative_screen_origin_and_rounds_dpr_endpoints() {
        let hit = placement_wheel_hit(
            Some((-200, -100, 400, 200)),
            Some([10.2, 5.2, 20.6, 10.6]),
            Some([300.0, 200.0]),
            1.5,
            true,
        )
        .unwrap();
        assert_eq!(bounds(hit), (-185, -92, 31, 16, true));
        assert!(point_inside_wheel_hit(-185, -92, &hit, 0));
        assert!(point_inside_wheel_hit(-155, -77, &hit, 0));
        assert!(!point_inside_wheel_hit(-154, -77, &hit, 0));
        assert!(!point_inside_wheel_hit(-155, -76, &hit, 0));
    }

    #[test]
    fn placement_geometry_clips_partial_cluster_to_viewport_and_client_before_rounding() {
        let hit = placement_wheel_hit(
            Some((-100, -200, 200, 120)),
            Some([-5.0, 50.0, 110.0, 30.0]),
            Some([90.0, 70.0]),
            2.0,
            true,
        )
        .unwrap();
        assert_eq!(bounds(hit), (-100, -100, 180, 20, true));
        let client_only = placement_wheel_hit(
            Some((-100, -200, 100, 50)),
            Some([90.0, 40.0, 30.0, 20.0]),
            None,
            1.0,
            true,
        )
        .unwrap();
        assert_eq!(bounds(client_only), (-10, -160, 10, 10, true));
        for cluster in [
            [90.0, 0.0, 10.0, 10.0],
            [-20.0, 0.0, 10.0, 10.0],
            [0.0, 70.0, 10.0, 10.0],
            [0.0, 0.0, 0.1, 1.0],
        ] {
            assert_eq!(
                placement_wheel_hit(
                    Some((0, 0, 200, 120)),
                    Some(cluster),
                    Some([90.0, 70.0]),
                    2.0,
                    true
                )
                .unwrap_err(),
                "invalid-placement"
            );
        }
    }

    #[test]
    fn placement_geometry_rejects_nonfinite_zero_and_raw_overflow_before_clipping() {
        let origin = Some((0, 0, 100, 100));
        for dpr in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(placement_wheel_hit(origin, None, None, dpr, true).is_err());
        }
        for cluster in [
            [f64::NAN, 0.0, 1.0, 1.0],
            [0.0, f64::INFINITY, 1.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, -1.0],
            [0.0, 0.0, f64::MAX, 1.0],
            [f64::from(i32::MAX), 0.0, 1.0, 1.0],
            [0.0, 0.0, f64::from(i32::MAX), 1.0],
        ] {
            assert!(placement_wheel_hit(origin, Some(cluster), None, 2.0, true).is_err());
        }
        for viewport in [
            [0.0, 1.0],
            [1.0, -1.0],
            [f64::NAN, 1.0],
            [1.0, f64::INFINITY],
            [f64::MAX, 1.0],
        ] {
            assert!(placement_wheel_hit(origin, None, Some(viewport), 1.0, true).is_err());
        }
        for origin in [
            (0, 0, 0, 1),
            (0, 0, 1, -1),
            (i32::MAX, 0, 1, 1),
            (0, i32::MAX, 1, 1),
        ] {
            assert!(placement_wheel_hit(Some(origin), None, None, 1.0, true).is_err());
        }
    }

    #[test]
    fn placement_geometry_missing_origin_never_reuses_hit_and_full_fallback_keeps_visibility() {
        assert_eq!(
            bounds(placement_wheel_hit(None, None, None, 1.0, true).unwrap()),
            (0, 0, 0, 0, false)
        );
        assert!(placement_wheel_hit(None, Some([0.0, 0.0, 1.0, 1.0]), None, 1.0, true).is_err());
        assert_eq!(
            bounds(
                placement_wheel_hit(Some((-50, -40, 30, 20)), None, Some([1.0, 1.0]), 2.0, false)
                    .unwrap()
            ),
            (-50, -40, 30, 20, false)
        );
        let hit = placement_wheel_hit(
            Some((i32::MAX - 10, i32::MIN, 10, 10)),
            Some([0.0, 0.0, 10.0, 10.0]),
            None,
            1.0,
            true,
        )
        .unwrap();
        assert_eq!(bounds(hit), (i32::MAX - 10, i32::MIN, 10, 10, true));
    }

    #[test]
    fn wheel_hit_extreme_endpoints_never_overflow_and_negative_slop_never_hits() {
        let upper = QpWheelHitRect {
            left: i32::MAX - 2,
            top: i32::MAX - 2,
            width: 10,
            height: 10,
            visible: true,
        };
        assert!(point_inside_wheel_hit(i32::MAX, i32::MAX, &upper, i32::MAX));
        assert!(point_inside_wheel_hit(i32::MAX, i32::MAX, &upper, 0));
        let lower = QpWheelHitRect {
            left: i32::MIN,
            top: i32::MIN,
            width: 10,
            height: 10,
            visible: true,
        };
        assert!(point_inside_wheel_hit(i32::MIN, i32::MIN, &lower, i32::MAX));
        assert!(!point_inside_wheel_hit(i32::MIN + 10, i32::MIN, &lower, 0));
        assert!(!point_inside_wheel_hit(i32::MIN, i32::MIN, &lower, -1));
        let hidden = QpWheelHitRect {
            visible: false,
            ..lower
        };
        assert!(!point_inside_wheel_hit(i32::MIN, i32::MIN, &hidden, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClipRecord;
    use chrono::Utc;

    fn ev(vk: u32, down: bool, alt: bool, now_ms: u64) -> KeyEvent {
        KeyEvent {
            vk,
            down,
            alt,
            ctrl: false,
            shift: false,
            win: false,
            now_ms,
        }
    }

    fn ev_mods(
        vk: u32,
        down: bool,
        alt: bool,
        ctrl: bool,
        shift: bool,
        win: bool,
        now_ms: u64,
    ) -> KeyEvent {
        KeyEvent {
            vk,
            down,
            alt,
            ctrl,
            shift,
            win,
            now_ms,
        }
    }

    #[test]
    fn quick_paste_mouse_selected_buttons_keep_wheel_and_release_order() {
        for button in [QuickPasteMouseButton::X1, QuickPasteMouseButton::X2] {
            let mut gesture = QuickPasteMouseGesture::default();
            assert!(gesture.configure(Some(button)));
            assert_eq!(
                gesture.handle_button(button, true),
                (HookVerdict::Pass, MouseGestureAction::Armed)
            );
            assert_eq!(gesture.held_button(), Some(button));
            assert!(gesture.is_held());
            assert!(!gesture.is_engaged());
            assert_eq!(
                gesture.handle_button(button, true),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
            assert_eq!(
                gesture.handle_wheel(0),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
            // No worker/UI callback is needed between these physical events.
            assert_eq!(
                gesture.handle_wheel(-120),
                (HookVerdict::Swallow, MouseGestureAction::BeginSession)
            );
            assert!(gesture.is_engaged());
            assert!(
                !gesture.configure(Some(button)),
                "unchanged configuration preserves this gesture"
            );
            assert_eq!(
                gesture.handle_wheel(-120),
                (
                    HookVerdict::Swallow,
                    MouseGestureAction::MoveSelection { delta: 1 }
                )
            );
            assert_eq!(
                gesture.handle_wheel(120),
                (
                    HookVerdict::Swallow,
                    MouseGestureAction::MoveSelection { delta: -1 }
                )
            );
            assert_eq!(
                gesture.handle_button(button, false),
                (HookVerdict::Swallow, MouseGestureAction::Commit)
            );
            assert!(!gesture.is_held());
            assert!(!gesture.is_engaged());
            assert!(!gesture.is_draining());
            assert_eq!(
                gesture.handle_button(button, false),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
            assert_eq!(
                gesture.handle_wheel(-120),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
            // Only the release that completes the wheel chord is owned.
            assert_eq!(
                gesture.handle_button(button, true),
                (HookVerdict::Pass, MouseGestureAction::Armed)
            );
            assert_eq!(
                gesture.handle_button(button, false),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
        }
    }

    #[test]
    fn quick_paste_mouse_side_button_alone_always_passes() {
        assert_eq!(
            QuickPasteMouseButton::parse_config(" X1 "),
            Some(QuickPasteMouseButton::X1)
        );
        assert_eq!(
            QuickPasteMouseButton::parse_config("x2"),
            Some(QuickPasteMouseButton::X2)
        );
        for value in ["none", "", "middle", "x3"] {
            assert_eq!(QuickPasteMouseButton::parse_config(value), None);
        }
        let mut gesture = QuickPasteMouseGesture::default();
        for button in [QuickPasteMouseButton::X1, QuickPasteMouseButton::X2] {
            assert_eq!(
                gesture.handle_button(button, true),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
            assert_eq!(
                gesture.handle_button(button, false),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
        }
        assert_eq!(
            gesture.handle_wheel(-120),
            (HookVerdict::Pass, MouseGestureAction::None)
        );
        gesture.configure(Some(QuickPasteMouseButton::X1));
        // Click or press-and-hold without wheel: nothing is consumed or queued.
        assert_eq!(
            gesture.handle_button(QuickPasteMouseButton::X1, true),
            (HookVerdict::Pass, MouseGestureAction::Armed)
        );
        for down in [true, false] {
            assert_eq!(
                gesture.handle_button(QuickPasteMouseButton::X2, down),
                (HookVerdict::Pass, MouseGestureAction::None)
            );
            assert_eq!(gesture.held_button(), Some(QuickPasteMouseButton::X1));
        }
        assert_eq!(
            gesture.handle_button(QuickPasteMouseButton::X1, false),
            (HookVerdict::Pass, MouseGestureAction::None)
        );
        assert_eq!(
            gesture.handle_button(QuickPasteMouseButton::X1, false),
            (HookVerdict::Pass, MouseGestureAction::None)
        );
        assert_eq!(
            gesture.handle_wheel(-120),
            (HookVerdict::Pass, MouseGestureAction::None),
            "wheel after release is ordinary input"
        );
    }

    #[test]
    fn quick_paste_mouse_cancel_and_config_change_drain_original_release() {
        for engaged in [false, true] {
            for replacement in [
                Some(QuickPasteMouseButton::X1),
                None,
                Some(QuickPasteMouseButton::X2),
            ] {
                let mut gesture = QuickPasteMouseGesture::default();
                gesture.configure(Some(QuickPasteMouseButton::X1));
                gesture.handle_button(QuickPasteMouseButton::X1, true);
                if engaged {
                    gesture.handle_wheel(-120);
                }
                gesture.cancel();
                assert!(gesture.is_held());
                assert!(gesture.is_draining());
                assert!(!gesture.is_engaged());
                assert_eq!(
                    gesture.configure(replacement),
                    replacement != Some(QuickPasteMouseButton::X1)
                );
                assert_eq!(
                    gesture.handle_wheel(-120),
                    (HookVerdict::Pass, MouseGestureAction::None)
                );
                assert_eq!(
                    gesture.handle_button(QuickPasteMouseButton::X1, true),
                    (HookVerdict::Pass, MouseGestureAction::None)
                );
                assert_eq!(
                    gesture.handle_button(QuickPasteMouseButton::X2, true),
                    (HookVerdict::Pass, MouseGestureAction::None)
                );
                assert_eq!(
                    gesture.handle_button(QuickPasteMouseButton::X1, false),
                    (
                        if engaged { HookVerdict::Swallow } else { HookVerdict::Pass },
                        MouseGestureAction::None,
                    )
                );
                assert!(!gesture.is_held());
                assert!(!gesture.is_draining());
                let expected = if replacement == Some(QuickPasteMouseButton::X2) {
                    (HookVerdict::Pass, MouseGestureAction::Armed)
                } else {
                    (HookVerdict::Pass, MouseGestureAction::None)
                };
                assert_eq!(
                    gesture.handle_button(QuickPasteMouseButton::X2, true),
                    expected
                );
            }
        }
        // Configuration replacement itself cancels an engaged press.
        let mut gesture = QuickPasteMouseGesture::default();
        gesture.configure(Some(QuickPasteMouseButton::X1));
        gesture.handle_button(QuickPasteMouseButton::X1, true);
        gesture.handle_wheel(120);
        gesture.configure(Some(QuickPasteMouseButton::X2));
        assert!(gesture.is_draining());
        assert!(!gesture.is_engaged());
        assert_eq!(
            gesture.handle_button(QuickPasteMouseButton::X1, false),
            (HookVerdict::Swallow, MouseGestureAction::None)
        );
    }

    #[test]
    fn quick_paste_mouse_keyboard_commands_do_not_borrow_the_chord() {
        let mut machine = QuickPasteMachine::with_required(VK_OEM_3, true, true, false, false);
        let binding = machine.clone();
        machine.begin_mouse_session();
        assert!(machine.has_same_binding(&binding));
        assert_eq!(machine.trigger_press_index(), 0);
        assert_eq!(machine.phase, QuickPastePhase::SessionStarting);
        assert!(!machine.commit_when_session_opens);
        assert_eq!(
            machine.handle_mouse_key(ev(VK_MENU, false, false, 1)),
            (HookVerdict::Pass, QuickPasteCommand::None)
        );
        assert_eq!(machine.on_session_ui_ready(), QuickPasteCommand::None);
        for (vk, command) in [
            (VK_E, QuickPasteCommand::ArmPasteEnter),
            (VK_D, QuickPasteCommand::DeleteActive),
            (VK_F, QuickPasteCommand::ArmFifo),
            (VK_Q, QuickPasteCommand::MoveSelection { delta: -1 }),
            (VK_1, QuickPasteCommand::ArmRowAction { shortcut: 1 }),
            (VK_2, QuickPasteCommand::ArmRowAction { shortcut: 2 }),
            (VK_UP, QuickPasteCommand::MoveSelection { delta: -1 }),
            (VK_DOWN, QuickPasteCommand::MoveSelection { delta: 1 }),
        ] {
            assert_eq!(
                machine.handle_mouse_key(ev(vk, true, false, 2)),
                (HookVerdict::Swallow, command)
            );
            assert_eq!(
                machine.handle_mouse_key(ev(vk, true, false, 3)),
                (HookVerdict::Swallow, QuickPasteCommand::None)
            );
            assert_eq!(
                machine.handle_mouse_key(ev(vk, false, false, 4)),
                (HookVerdict::Pass, QuickPasteCommand::None)
            );
        }
        for event in [
            ev(VK_OEM_3, true, false, 5),
            ev(VK_OEM_3, false, false, 6),
            ev(VK_MENU, false, false, 7),
            ev_mods(VK_ESCAPE, true, true, false, false, false, 8),
            ev_mods(VK_D, true, false, true, false, false, 9),
        ] {
            assert_eq!(
                machine.handle_mouse_key(event),
                (HookVerdict::Pass, QuickPasteCommand::None)
            );
            assert_eq!(machine.phase, QuickPastePhase::SessionActive);
        }
        // Release the passed Alt+Esc before beginning a new plain Esc press.
        machine.handle_mouse_key(ev(VK_ESCAPE, false, false, 9));
        assert_eq!(
            machine.handle_mouse_key(ev(VK_ESCAPE, true, false, 10)),
            (HookVerdict::Swallow, QuickPasteCommand::Cancel)
        );
        machine.begin_mouse_session();
        assert_eq!(
            machine.handle_mouse_key(ev(VK_ESCAPE, true, false, 11)),
            (HookVerdict::Swallow, QuickPasteCommand::Cancel)
        );
    }

    #[test]
    fn alt_backtick_hold_then_alt_up_commits() {
        let mut m = QuickPasteMachine::default();
        let (v, cmd) = m.handle(ev(VK_MENU, true, true, 0));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::None);
        let (v, cmd) = m.handle(ev(VK_OEM_3, true, true, 10));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::BeginSession);
        assert_eq!(m.on_session_ui_ready(), QuickPasteCommand::None);
        assert_eq!(m.phase, QuickPastePhase::SessionActive);
        let (v, cmd) = m.handle(ev(VK_MENU, false, false, 400));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::Commit { paste_enter: false });
        assert_eq!(m.phase, QuickPastePhase::Idle);
    }

    #[test]
    fn alt_up_before_session_ready_commits_when_ui_opens() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 5));
        let (v, cmd) = m.handle(ev(VK_MENU, false, false, 8));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::None);
        assert!(m.commit_when_session_opens);
        assert_eq!(
            m.on_session_ui_ready(),
            QuickPasteCommand::Commit { paste_enter: false }
        );
    }

    #[test]
    fn trigger_up_then_modifier_poll_commits() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.on_session_ui_ready();
        let (v, cmd) = m.handle(ev(VK_OEM_3, false, true, 80));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::None);
        assert!(m.trigger_released_during_session);
        assert_eq!(
            m.poll_backup_modifiers(false),
            QuickPasteCommand::Commit { paste_enter: false }
        );
    }

    #[test]
    fn esc_cancels_even_without_alt() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.on_session_ui_ready();
        let (v, cmd) = m.handle(ev(VK_ESCAPE, true, false, 40));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::Cancel);
    }

    #[test]
    fn alt_e_arms_paste_enter_then_alt_up_commits() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.on_session_ui_ready();
        let (_, cmd) = m.handle(ev(VK_E, true, true, 30));
        assert_eq!(cmd, QuickPasteCommand::ArmPasteEnter);
        let (_, cmd) = m.handle(ev(VK_MENU, false, false, 50));
        assert_eq!(cmd, QuickPasteCommand::Commit { paste_enter: true });
    }

    #[test]
    fn session_list_recent_then_pinned() {
        let mut a = ClipRecord::new_text("a", "x");
        a.create_time = Utc::now();
        let mut b = ClipRecord::new_text("b", "x");
        b.create_time = Utc::now() + chrono::Duration::seconds(1);
        let mut pin = ClipRecord::new_text("pin", "x");
        pin.is_top = true;
        let ordered = select_session_items(&[a, b.clone(), pin.clone()], 25);
        assert_eq!(ordered[0].body_text.as_deref(), Some("b"));
        assert!(ordered.last().unwrap().is_top);
        assert_eq!(get_display_number(&ordered, 0), 1);
    }

    #[test]
    fn ctrl_oem3_hold_then_ctrl_up_commits() {
        let mut m = QuickPasteMachine::with_required(VK_OEM_3, true, false, false, false);
        let (v, cmd) = m.handle(ev_mods(VK_CONTROL, true, false, true, false, false, 0));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::None);
        let (v, cmd) = m.handle(ev_mods(VK_OEM_3, true, false, true, false, false, 10));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::BeginSession);
        assert_eq!(m.on_session_ui_ready(), QuickPasteCommand::None);
        let (v, cmd) = m.handle(ev_mods(VK_CONTROL, false, false, false, false, false, 400));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::Commit { paste_enter: false });
    }

    #[test]
    fn session_command_keys_pass_when_required_modifiers_are_up() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.on_session_ui_ready();
        let (v, cmd) = m.handle(ev(VK_D, true, false, 40));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::None);
        m.handle(ev(VK_D, false, false, 45));
        let (v, cmd) = m.handle(ev(VK_D, true, true, 50));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::DeleteActive);
    }

    #[test]
    fn alt_chord_rejects_extra_ctrl() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        let (v, cmd) = m.handle(ev_mods(VK_OEM_3, true, true, true, false, false, 10));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::None);
        assert_eq!(m.phase, QuickPastePhase::Idle);
    }

    #[test]
    fn session_actions_require_exact_modifiers() {
        let actions = [VK_D, VK_E, VK_F, VK_Q, VK_1, VK_2, VK_UP, VK_DOWN, VK_LEFT, VK_RIGHT];
        for required in [[true, false, false, false], [false, false, false, true]] {
            for extra in 0..4 {
                if required[extra] { continue; }
                for vk in actions {
                    let [alt, ctrl, shift, win] = required;
                    let mut machine = QuickPasteMachine::with_required(VK_OEM_3, ctrl, alt, shift, win);
                    machine.handle(ev_mods(VK_OEM_3, true, alt, ctrl, shift, win, 1));
                    machine.on_session_ui_ready();
                    let mut modifiers = required;
                    modifiers[extra] = true;
                    let [alt, ctrl, shift, win] = modifiers;
                    assert_eq!(
                        machine.handle(ev_mods(vk, true, alt, ctrl, shift, win, 2)),
                        (HookVerdict::Pass, QuickPasteCommand::None),
                        "extra modifier {extra} must not invoke action {vk:#x}",
                    );
                    assert_eq!(machine.phase, QuickPastePhase::SessionActive);
                }
            }
        }
        let mut win = QuickPasteMachine::with_required(VK_OEM_3, false, false, false, true);
        win.handle(ev_mods(VK_OEM_3, true, false, false, false, true, 1));
        win.on_session_ui_ready();
        assert_eq!(win.handle(ev_mods(VK_E, true, false, false, false, true, 2)),
            (HookVerdict::Swallow, QuickPasteCommand::ArmPasteEnter));
        assert_eq!(win.handle(ev_mods(VK_LWIN, false, false, false, false, false, 3)),
            (HookVerdict::Pass, QuickPasteCommand::Commit { paste_enter: true }));
    }

    #[test]
    fn win_d_passes_through_quick_paste_lifecycle() {
        for mut machine in [QuickPasteMachine::default(), QuickPasteMachine::new(VK_OEM_3, false)] {
            let desktop = ev_mods(VK_D, true, false, false, false, true, 1);
            assert_eq!(machine.handle(desktop), (HookVerdict::Pass, QuickPasteCommand::None));
            let alt = machine.requires_alt();
            machine.handle(ev(VK_OEM_3, true, alt, 2));
            machine.on_session_ui_ready();
            assert_eq!(machine.handle(desktop), (HookVerdict::Pass, QuickPasteCommand::None));
            machine.handle(ev(VK_ESCAPE, true, alt, 3));
            assert_eq!(machine.phase, QuickPastePhase::Idle);
            assert_eq!(machine.handle(desktop), (HookVerdict::Pass, QuickPasteCommand::None));
        }
    }

    #[test]
    fn modified_escape_passes_without_canceling_quick_paste() {
        for mut machine in [QuickPasteMachine::default(), QuickPasteMachine::new(VK_OEM_3, false)] {
            let alt = machine.requires_alt();
            machine.handle(ev(VK_OEM_3, true, alt, 1));
            for ready in [false, true] {
                if ready { machine.on_session_ui_ready(); }
                assert_eq!(machine.handle(ev_mods(VK_ESCAPE, true, alt, false, false, true, 2)),
                    (HookVerdict::Pass, QuickPasteCommand::None));
                assert_eq!(machine.phase, if ready { QuickPastePhase::SessionActive }
                    else { QuickPastePhase::SessionStarting });
            }
            machine.handle(ev(VK_ESCAPE, false, false, 3));
            assert_eq!(machine.handle(ev(VK_ESCAPE, true, false, 3)),
                (HookVerdict::Swallow, QuickPasteCommand::Cancel));
        }
    }

    #[test]
    fn bare_trigger_release_commits_once_before_or_after_session_ready() {
        for ready_before_release in [false, true] {
            let mut machine = QuickPasteMachine::new(VK_OEM_3, false);
            assert_eq!(machine.handle(ev(VK_OEM_3, true, false, 1)),
                (HookVerdict::Swallow, QuickPasteCommand::BeginSession));
            if ready_before_release {
                assert_eq!(machine.on_session_ui_ready(), QuickPasteCommand::None);
            }
            assert_eq!(machine.handle(ev(VK_OEM_3, false, false, 2)),
                (HookVerdict::Swallow, if ready_before_release {
                    QuickPasteCommand::Commit { paste_enter: false }
                } else { QuickPasteCommand::None }));
            if !ready_before_release {
                assert_eq!(machine.on_session_ui_ready(), QuickPasteCommand::Commit { paste_enter: false });
            }
            assert_eq!(machine.phase, QuickPastePhase::Idle);
            assert_eq!(machine.handle(ev(VK_OEM_3, false, false, 3)),
                (HookVerdict::Pass, QuickPasteCommand::None));
            assert_eq!(machine.on_session_ui_ready(), QuickPasteCommand::None);
        }
    }

    #[test]
    fn bare_trigger_backup_never_infers_release_from_swallowed_key_state() {
        let mut machine = QuickPasteMachine::new(VK_OEM_3, false);
        assert!(!machine.held_for_release_with(|_| panic!("bare trigger must not query OS state")));
        assert!(machine.is_chord_fully_released());
        machine.handle(ev(VK_OEM_3, true, false, 1));
        machine.on_session_ui_ready();
        assert!(!machine.is_chord_fully_released());
        assert!(!machine.held_for_release_with(|_| panic!("bare trigger must not query OS state")));
        assert_eq!(machine.poll_backup_modifiers(true), QuickPasteCommand::None);
        assert_eq!(machine.poll_backup_modifiers(false), QuickPasteCommand::None);
        assert_eq!(machine.handle(ev(VK_OEM_3, false, false, 2)),
            (HookVerdict::Swallow, QuickPasteCommand::Commit { paste_enter: false }));
        assert!(machine.is_chord_fully_released());
        assert_eq!(machine.poll_backup_modifiers(false), QuickPasteCommand::None);
        machine.reset_session();
        assert!(machine.is_chord_fully_released(), "teardown cannot leave a bare binding held forever");
    }

    #[test]
    fn trigger_release_requires_an_owned_down_even_during_session_or_drain() {
        let mut machine = QuickPasteMachine::default();
        machine.handle(ev(VK_OEM_3, true, true, 1));
        machine.on_session_ui_ready();
        assert_eq!(machine.handle(ev(VK_OEM_3, false, true, 2)).0, HookVerdict::Swallow);
        assert_eq!(machine.handle(ev(VK_OEM_3, false, true, 3)).0, HookVerdict::Pass);
        assert_eq!(machine.handle(ev_mods(VK_OEM_3, true, true, false, false, true, 4)).0,
            HookVerdict::Pass);
        assert_eq!(machine.handle(ev(VK_OEM_3, false, true, 5)).0, HookVerdict::Pass);
        machine.set_chord_drain_active(true);
        assert_eq!(machine.handle(ev(VK_OEM_3, false, true, 6)).0, HookVerdict::Pass);
    }

    #[test]
    fn trigger_ownership_survives_cancel_and_session_reset() {
        let mut machine = QuickPasteMachine::default();
        machine.handle(ev(VK_OEM_3, true, true, 1));
        machine.handle(ev(VK_ESCAPE, true, true, 2));
        machine.reset_session();
        assert_eq!(machine.handle(ev_mods(VK_OEM_3, true, true, false, false, true, 3)),
            (HookVerdict::Swallow, QuickPasteCommand::None));
        assert_eq!(machine.handle(ev(VK_OEM_3, false, false, 4)),
            (HookVerdict::Swallow, QuickPasteCommand::None));
        assert_eq!(machine.handle(ev(VK_OEM_3, false, false, 5)).0, HookVerdict::Pass);
    }

    #[test]
    fn passed_trigger_repeat_cannot_be_adopted_after_modifiers_change() {
        let mut machine = QuickPasteMachine::default();
        assert_eq!(machine.handle(ev_mods(VK_OEM_3, true, true, true, false, false, 1)).0,
            HookVerdict::Pass);
        assert_eq!(machine.handle(ev(VK_OEM_3, true, true, 2)),
            (HookVerdict::Pass, QuickPasteCommand::None));
        assert_eq!(machine.handle(ev(VK_OEM_3, false, true, 3)).0, HookVerdict::Pass);
        assert_eq!(machine.handle(ev(VK_OEM_3, true, true, 4)),
            (HookVerdict::Swallow, QuickPasteCommand::BeginSession));
    }

    #[test]
    fn passed_session_command_repeats_stay_with_foreground_until_up() {
        for source in 0..3 {
            for (vk, command) in [(VK_D, QuickPasteCommand::DeleteActive),
                (VK_E, QuickPasteCommand::ArmPasteEnter),
                (VK_1, QuickPasteCommand::ArmRowAction { shortcut: 1 })]
            {
                let mut machine = QuickPasteMachine::new(VK_OEM_3, source == 0);
                if source == 2 { machine.begin_mouse_session(); }
                else { machine.handle(ev(VK_OEM_3, true, source == 0, 1)); }
                machine.on_session_ui_ready();
                let handle = |machine: &mut QuickPasteMachine, event| {
                    if source == 2 { machine.handle_mouse_key(event) } else { machine.handle(event) }
                };
                assert_eq!(handle(&mut machine, ev_mods(vk, true, source == 0, false, false, true, 2)),
                    (HookVerdict::Pass, QuickPasteCommand::None));
                assert_eq!(handle(&mut machine, ev(vk, true, source == 0, 3)),
                    (HookVerdict::Pass, QuickPasteCommand::None));
                assert_eq!(handle(&mut machine, ev(vk, false, source == 0, 4)).0, HookVerdict::Pass);
                assert_eq!(handle(&mut machine, ev(vk, true, source == 0, 5)),
                    (HookVerdict::Swallow, command));
            }
        }
    }

    #[test]
    fn disabled_paste_enter_passes_until_release_across_setting_changes() {
        for source in 0..3 {
            for ready in [false, true] {
                let mut machine = QuickPasteMachine::new(VK_OEM_3, source == 0);
                machine.set_paste_enter_shortcut_enabled(false);
                if source == 2 { machine.begin_mouse_session(); }
                else { machine.handle(ev(VK_OEM_3, true, source == 0, 1)); }
                if ready { machine.on_session_ui_ready(); }
                let handle = |machine: &mut QuickPasteMachine, down, time| {
                    let event = ev(VK_E, down, source == 0, time);
                    if source == 2 { machine.handle_mouse_key(event) } else { machine.handle(event) }
                };
                assert_eq!(handle(&mut machine, true, 2), (HookVerdict::Pass, QuickPasteCommand::None));
                assert!(!machine.paste_enter_armed);
                machine.set_paste_enter_shortcut_enabled(true);
                assert_eq!(handle(&mut machine, true, 3), (HookVerdict::Pass, QuickPasteCommand::None));
                assert!(!machine.paste_enter_armed);
                assert_eq!(handle(&mut machine, false, 4).0, HookVerdict::Pass);
                assert_eq!(handle(&mut machine, true, 5), (HookVerdict::Swallow, QuickPasteCommand::ArmPasteEnter));
                machine.set_paste_enter_shortcut_enabled(false);
                assert_eq!(handle(&mut machine, true, 6), (HookVerdict::Swallow, QuickPasteCommand::None));
                assert_eq!(handle(&mut machine, false, 7).0, HookVerdict::Pass);
                assert_eq!(handle(&mut machine, true, 8), (HookVerdict::Pass, QuickPasteCommand::None));
            }
        }
    }

    #[test]
    fn disabling_paste_enter_does_not_disable_an_explicit_e_trigger() {
        let mut machine = QuickPasteMachine::new(VK_E, true);
        machine.set_paste_enter_shortcut_enabled(false);
        assert_eq!(machine.handle(ev(VK_E, true, true, 1)),
            (HookVerdict::Swallow, QuickPasteCommand::BeginSession));
        assert_eq!(machine.handle(ev(VK_E, false, true, 2)),
            (HookVerdict::Swallow, QuickPasteCommand::None));
    }

    #[test]
    fn second_trigger_down_without_up_is_auto_repeat() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        let (v, cmd) = m.handle(ev(VK_OEM_3, true, true, 10));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::BeginSession);
        let (v, cmd) = m.handle(ev(VK_OEM_3, true, true, 20));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::None);
        assert_eq!(m.trigger_press_index(), 0);
    }

    #[test]
    fn second_physical_trigger_advances_selection() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.handle(ev(VK_OEM_3, false, true, 20));
        m.on_session_ui_ready();
        let (v, cmd) = m.handle(ev(VK_OEM_3, true, true, 40));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::MoveSelection { delta: 1 });
        assert_eq!(m.trigger_press_index(), 1);
    }

    #[test]
    fn duplicate_syskey_same_event_time_is_swallowed() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.handle(ev(VK_OEM_3, false, true, 11));
        m.on_session_ui_ready();
        let (_, cmd) = m.handle(ev(VK_OEM_3, true, true, 50));
        assert_eq!(cmd, QuickPasteCommand::MoveSelection { delta: 1 });
        let (v, cmd) = m.handle(ev(VK_OEM_3, true, true, 50));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::None);
    }

    #[test]
    fn session_command_auto_repeat_swallows_without_mutating() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_MENU, true, true, 0));
        m.handle(ev(VK_OEM_3, true, true, 10));
        m.on_session_ui_ready();
        let (v, cmd) = m.handle(ev(VK_D, true, true, 20));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::DeleteActive);
        let (v, cmd) = m.handle(ev(VK_D, true, true, 30));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::None);
    }

    #[test]
    fn keyboard_digits_arm_configured_slots_while_arrows_move_selection() {
        let mut machine = QuickPasteMachine::default();
        machine.handle(ev(VK_MENU, true, true, 0));
        machine.handle(ev(VK_OEM_3, true, true, 10));
        machine.on_session_ui_ready();
        for (vk, command) in [
            (VK_1, QuickPasteCommand::ArmRowAction { shortcut: 1 }),
            (VK_2, QuickPasteCommand::ArmRowAction { shortcut: 2 }),
            (VK_F, QuickPasteCommand::ArmFifo),
            (VK_Q, QuickPasteCommand::MoveSelection { delta: -1 }),
            (VK_UP, QuickPasteCommand::MoveSelection { delta: -1 }),
            (VK_DOWN, QuickPasteCommand::MoveSelection { delta: 1 }),
        ] {
            assert_eq!(
                machine.handle(ev(vk, true, true, 20)),
                (HookVerdict::Swallow, command)
            );
            assert_eq!(
                machine.handle(ev(vk, true, true, 30)),
                (HookVerdict::Swallow, QuickPasteCommand::None)
            );
            machine.handle(ev(vk, false, true, 40));
        }
        assert_eq!(
            machine.handle(ev(VK_MENU, false, false, 50)),
            (
                HookVerdict::Pass,
                QuickPasteCommand::Commit { paste_enter: false }
            )
        );
    }

    #[test]
    fn chord_selection_index_uses_explicit_resume_index() {
        assert_eq!(resolve_chord_selection_index(0, 10, false, 10, 5, 1.0), 0);
        assert_eq!(resolve_chord_selection_index(2, 10, false, 10, 5, 1.0), 2);
        assert_eq!(resolve_chord_selection_index(0, 5, true, 0, 2, 0.0), 0);
        assert_eq!(resolve_chord_selection_index(0, 10, true, 10, 3, 3.0), 3);
        assert_eq!(resolve_chord_selection_index(1, 10, true, 10, 3, 3.0), 4);
        assert_eq!(resolve_chord_selection_index(0, 5, true, 10, 4, 1.0), 4);
        assert_eq!(resolve_chord_selection_index(2, 5, true, 10, 3, 1.0), 0);
        assert_eq!(resolve_chord_selection_index(0, 5, true, 10, 3, 11.0), 0);
        assert_eq!(resolve_chord_selection_index(0, 3, true, 10, 5, 1.0), 0);
        assert_eq!(resolve_chord_selection_index(2, 0, true, 10, 1, 0.0), 0);
    }

    #[test]
    fn mouse_wheel_policy_matches_csharp() {
        assert!(should_consume_mouse_wheel(true, true, true));
        assert!(!should_consume_mouse_wheel(true, true, false));
        assert!(!should_consume_mouse_wheel(true, false, true));
        assert!(!should_consume_mouse_wheel(false, true, true));
        assert_eq!(mouse_wheel_selection_delta(120), -1);
        assert_eq!(mouse_wheel_selection_delta(-120), 1);
        assert_eq!(mouse_wheel_selection_delta(0), 0);
        let hit = QpWheelHitRect {
            left: 100,
            top: 100,
            width: 200,
            height: 300,
            visible: true,
        };
        assert!(point_inside_wheel_hit(100, 100, &hit, WHEEL_HIT_SLOP_PX));
        assert!(point_inside_wheel_hit(76, 100, &hit, WHEEL_HIT_SLOP_PX));
        assert!(!point_inside_wheel_hit(75, 100, &hit, WHEEL_HIT_SLOP_PX));
    }

    #[test]
    fn trigger_key_up_passes_when_idle_without_drain() {
        let mut m = QuickPasteMachine::default();
        let (v, cmd) = m.handle(ev(VK_OEM_3, false, false, 1));
        assert_eq!(v, HookVerdict::Pass);
        assert_eq!(cmd, QuickPasteCommand::None);
    }

    #[test]
    fn chord_drain_swallows_trigger_up_then_clears_when_released() {
        let mut m = QuickPasteMachine::default();
        m.handle(ev(VK_OEM_3, true, true, 0));
        m.reset_session();
        m.set_chord_drain_active(true);
        let (v, cmd) = m.handle(ev(VK_OEM_3, false, true, 1));
        assert_eq!(v, HookVerdict::Swallow);
        assert_eq!(cmd, QuickPasteCommand::None);
        if m.is_chord_fully_released() {
            assert!(!m.chord_drain_active());
        }
    }

    #[test]
    fn swallow_trigger_up_policy_matches_csharp() {
        assert!(!QuickPasteMachine::should_swallow_trigger_key_up(
            false, false
        ));
        assert!(QuickPasteMachine::should_swallow_trigger_key_up(
            true, false
        ));
        assert!(QuickPasteMachine::should_swallow_trigger_key_up(
            false, true
        ));
        assert!(QuickPasteMachine::should_swallow_trigger_key_up(true, true));
    }

    #[test]
    fn paste_target_prefers_captured_then_foreground_then_last() {
        assert_eq!(
            resolve_paste_target_hwnd(10, true, 20, true, 30, true, 99),
            10
        );
        assert_eq!(
            resolve_paste_target_hwnd(99, true, 20, true, 30, true, 99),
            20
        );
        assert_eq!(
            resolve_paste_target_hwnd(10, false, 20, true, 30, true, 99),
            20
        );
        assert_eq!(
            resolve_paste_target_hwnd(10, false, 99, true, 30, true, 99),
            30
        );
        assert_eq!(resolve_paste_target_hwnd(0, true, 0, true, 0, true, 1), 0);
    }
}
