//! Input-only routing. The native callback owns the actual HostInner guard
//! throughout a decision; it never checks a gate and enters a blocking router.
//! Ordinary business entry points retain their existing scheduling/behavior.
use super::*;

/// These three physical keys must keep their down/up ownership even when a
/// host/chrome lock is busy. An observed pass-through Down can never turn into
/// a shortcut on repeat after a modal closes or a new presentation appears.
#[derive(Default)]
pub(crate) struct NativeHistoryShortcutKeys {
    keys: [std::sync::atomic::AtomicU8; 3],
}

impl NativeHistoryShortcutKeys {
    fn key(&self, vk: u32) -> Option<&std::sync::atomic::AtomicU8> {
        self.keys.get(match vk { 0x41 => 0, 0x43 => 1, 0x50 => 2, _ => return None })
    }

    /// (fresh Down, draining a previously swallowed Down).
    pub(crate) fn observe(&self, event: KeyEvent) -> (bool, bool) {
        let Some(key) = self.key(event.vk) else { return (false, false); };
        let previous = if event.down { key.fetch_or(1, Ordering::AcqRel) }
            else { key.swap(0, Ordering::AcqRel) };
        (event.down && previous == 0, previous & 2 != 0)
    }

    fn claim(&self, vk: u32) -> bool {
        self.key(vk).is_some_and(|key|
            key.compare_exchange(1, 3, Ordering::AcqRel, Ordering::Acquire).is_ok())
    }
}

pub(super) fn native_history_shortcuts_available(inner: &HostInner) -> bool {
    inner.native_history_shortcuts_enabled
        && inner.window_runtime_ready && inner.window_visible && inner.clipboard_web_view_ready
        && inner.no_focus_session_active && inner.clipboard_keyboard_mode == ClipboardKeyboardMode::Full
        && is_clipboard_chrome_route(&inner.ui.clipboard_route)
        && !inner.ui.clipboard_editing && !inner.spa_modal_open && !inner.spa_overlay_open
        && !native_context_menu_for(inner, "clipboard") && inner.native_reveal_in_flight == 0
        && !inner.ole_drag_active && inner.native_incoming_drag.is_none()
        && inner.qp.machine.phase == QuickPastePhase::Idle && !inner.qp.overlay_visible
        && !inner.super_menu.visible && !inner.super_menu.loading
        && !inner.reopen.visible
        && !(inner.ui.panel_visible && inner.ui.panel_fake_focus)
}

impl HostState {
    /// Read-only status for the native clipboard's number-paste indicator.
    /// Modifier chords remain transient pass-throughs; this reports ordinary
    /// number-key ownership without queueing a key or changing fake focus.
    pub fn clipboard_digit_routing_active(&self) -> bool {
        let inner = self.inner.lock();
        if !inner.config.using_number_key_to_paste || !inner.window_runtime_ready || !inner.window_visible
            || !inner.no_focus_session_active || !inner.clipboard_web_view_ready
            || inner.clipboard_keyboard_mode == ClipboardKeyboardMode::Released
            || inner.clipboard_search_active
            || !is_clipboard_chrome_route(&inner.ui.clipboard_route)
            || inner.spa_modal_open || inner.spa_overlay_open
            || inner.ui.clipboard_editing
            || inner.native_reveal_in_flight > 0
            || native_context_menu_for(&inner, "clipboard")
            || inner.ole_drag_active || inner.native_incoming_drag.is_some()
            || inner.qp.machine.phase != QuickPastePhase::Idle || inner.qp.overlay_visible
            || inner.super_menu.visible || inner.super_menu.loading
            || inner.reopen.visible
            || (inner.ui.panel_visible && inner.ui.panel_fake_focus)
            || !(inner.has_host_fake_focus || inner.pointer_in_host)
        {
            return false;
        }
        let foreground = crate::window_menu::foreground_hwnd();
        ![inner.host_hwnd, inner.panel_hwnd, inner.main_hwnd].into_iter().any(|owner| {
            crate::window_chrome::is_in_move_size(owner)
                || crate::host_hit_test::is_owned_dialog(owner, foreground)
        })
    }

    pub(super) fn try_input_chrome_scope(
        &self,
        inner: &HostInner,
    ) -> Option<crate::window_chrome::InputChromeScope> {
        let scope = crate::window_chrome::try_input_scope([
            inner.host_hwnd,
            inner.panel_hwnd,
            inner.main_hwnd,
        ]);
        if scope.is_none() {
            self.mark_input_gap();
        }
        scope
    }

    pub(super) fn input_gap_pending(&self) -> bool {
        self.input_was_busy.load(Ordering::Acquire) || crate::hooks::has_registry_input_gap()
    }
    pub(crate) fn try_dispatch_input_key(&self, event: KeyEvent) -> bool {
        self.try_dispatch_input_key_for_foreground(event, crate::window_menu::foreground_hwnd())
    }

    pub(crate) fn native_history_shortcut_keys(&self) -> &Arc<NativeHistoryShortcutKeys> {
        &self.native_history_shortcut_keys
    }

    /// The installed hook has already observed this event before touching the
    /// host registry. Never observe it twice, which would turn Down into repeat.
    pub(crate) fn try_dispatch_observed_input_key(&self, event: KeyEvent, history_shortcut_fresh: bool) -> bool {
        self.try_dispatch_observed_input_key_for_foreground(
            event, crate::window_menu::foreground_hwnd(), history_shortcut_fresh)
    }

    pub(super) fn try_dispatch_input_key_for_foreground(
        &self,
        event: KeyEvent,
        foreground: isize,
    ) -> bool {
        let (history_shortcut_fresh, history_shortcut_owned) =
            self.native_history_shortcut_keys.observe(event);
        if history_shortcut_owned {
            return true;
        }
        self.try_dispatch_observed_input_key_for_foreground(event, foreground, history_shortcut_fresh)
    }

    fn try_dispatch_observed_input_key_for_foreground(
        &self,
        event: KeyEvent,
        foreground: isize,
        history_shortcut_fresh: bool,
    ) -> bool {
        let _callback = crate::hooks::input_callback_scope();
        let missed = input_gaps::GapInput::Key { vk: event.vk, down: event.down };
        let Some(mut inner) = self.inner.try_lock() else {
            self.mark_input_gap();
            self.input_gaps.record_host_busy(missed, self.inner.holder());
            return false;
        };
        let Some(_chrome) = self.try_input_chrome_scope(&inner) else {
            self.input_gaps.record_chrome_busy(missed);
            return false;
        };
        self.reconcile_missed_input_locked(&mut inner);
        self.input_dispatch_low_level_key_for_foreground(&mut inner, event, foreground, history_shortcut_fresh)
    }

    pub(crate) fn try_dispatch_input_mouse(&self, message: u32, x: i32, y: i32, data: u32) -> bool {
        let _callback = crate::hooks::input_callback_scope();
        let Some(mut inner) = self.inner.try_lock() else {
            self.mark_input_gap();
            self.input_gaps.record_host_busy(input_gaps::GapInput::Mouse, self.inner.holder());
            return false;
        };
        let Some(_chrome) = self.try_input_chrome_scope(&inner) else {
            self.input_gaps.record_chrome_busy(input_gaps::GapInput::Mouse);
            return false;
        };
        self.reconcile_missed_input_locked(&mut inner);
        self.input_dispatch_native_context_menu_mouse(&mut inner, message, x, y);
        inner.native_file_line_keyboard.outside_click(message, x, y);
        if message == 0x0204 || message == 0x0205 {
            let canceled = self
                .input_dispatch_low_level_quick_paste_right_button(&mut inner, message == 0x0204);
            if canceled || message == 0x0205 {
                return canceled;
            }
        }
        if message == 0x020B || message == 0x020C {
            let button = match (data >> 16) as u16 {
                1 => Some(QuickPasteMouseButton::X1),
                2 => Some(QuickPasteMouseButton::X2),
                _ => None,
            };
            if let Some(button) = button {
                if self.input_dispatch_low_level_side_button(&mut inner, button, message == 0x020B)
                {
                    return true;
                }
            }
        }
        if message == 0x020A {
            let delta = ((data >> 16) & 0xFFFF) as u16 as i16;
            if self.input_dispatch_low_level_wheel(&mut inner, delta, x, y) {
                return true;
            }
        }
        let inside = self.input_point_in_host(&mut inner, x, y);
        self.input_set_pointer_in_host(&mut inner, inside);
        if matches!(message, 0x0201 | 0x0202 | 0x0204) {
            self.input_dispatch_super_menu_mouse_at(&mut inner, message, x, y, Instant::now());
        }
        match message {
            0x0201 => self.input_notify_outside_click_at(&mut inner, x, y),
            0x0204 => self.input_notify_panel_outside_click_at(&mut inner, x, y),
            _ => {}
        }
        false
    }

    pub(crate) fn try_notify_input_foreground(&self, foreground: isize) {
        let _callback = crate::hooks::input_callback_scope();
        if let Some(mut inner) = self.inner.try_lock() {
            inner
                .super_menu
                .native_keyboard
                .observe_foreground(foreground);
            inner.native_fifo_keyboard.observe_foreground(foreground);
            inner.native_file_line_keyboard.observe_foreground(foreground);
        } else {
            self.mark_input_gap();
            self.input_gaps.record_host_busy(input_gaps::GapInput::Foreground, self.inner.holder());
        }
    }

    /// Returns false when a gap retired quick paste input, so work queued for
    /// it is void; true when there was no gap or a held session survived it.
    pub(super) fn reconcile_missed_input(&self) -> bool {
        if crate::hooks::has_registry_input_gap() {
            self.input_was_busy.store(true, Ordering::Release);
            crate::hooks::take_registry_input_gap();
        }
        if !self.input_was_busy.load(Ordering::Acquire) {
            return true;
        }
        let mut inner = self.inner.lock();
        self.reconcile_missed_input_locked(&mut inner)
    }

    /// `reconcile_missed_input` for a caller that already holds HostInner.
    pub(super) fn reconcile_missed_input_holding(&self, inner: &mut HostInner) -> bool {
        if crate::hooks::take_registry_input_gap() {
            self.input_was_busy.store(true, Ordering::Release);
        }
        self.reconcile_missed_input_locked(inner)
    }

    pub(super) fn reconcile_missed_input_locked(&self, inner: &mut HostInner) -> bool {
        if !self.input_was_busy.swap(false, Ordering::AcqRel) {
            return true;
        }
        // A passed-through release must never become a delayed commit. Retire
        // previous ownership instead of replaying the dropped key/button.
        inner.keyboard_keys_down.fill(false);
        inner.native_context_menu_escape_down = false;
        inner.qp.escape_down = false;
        inner.qp.mouse_cancel_right_down = false;
        inner.caps_guide_until_keyup = false;
        inner.native_fifo_keyboard.reset_after_input_gap();
        inner.native_context_menu_keyboard.reset_after_input_gap();
        inner.native_file_line_keyboard.reset_after_input_gap();
        inner.super_menu.native_keyboard.reset_after_input_gap();
        for window in ["clipboard", "panel", "main"] {
            drop_queued_window_input(inner, window);
        }
        self.input_cancel_text_instruction(inner);
        // The hook only try-locks, so a busy core turns any key into a gap.
        // A keyboard chord whose modifiers are still physically down was not
        // released: keep its opening or open session and its queued work
        // instead of closing the overlay while Alt is held. A missed release
        // still retires it below.
        let keep_qp = qp_session_survives_input_gap(
            inner.qp.machine.phase,
            inner.qp.hook_source == QuickPasteInputSource::Keyboard,
            inner.qp.mouse.is_held(),
            || {
                !crate::clipboard::is_headless()
                    && inner.qp.machine.required_modifiers_physically_down_for()
            },
        );
        let queued = inner.qp.pending_commands.len();
        if queued != 0 || inner.qp.machine.phase != QuickPastePhase::Idle || inner.qp.mouse.is_held() {
            self.input_gaps.record_quick_paste(keep_qp, if keep_qp { 0 } else { queued });
        }
        if keep_qp {
            inner.qp.machine.forget_held_keys_after_input_gap();
        } else {
            // Accepted FIFO cancellation has its own captured generation and
            // cannot inject input. A gap retires paste work, not this undo.
            let mut fifo_cancels = std::mem::take(&mut inner.qp.pending_commands);
            fifo_cancels.retain(|pending|
                matches!(pending.command, QuickPasteCommand::CancelFifo { .. }));
            if inner.qp.machine.phase != QuickPastePhase::Idle || inner.qp.mouse.is_held()
                || inner.qp.overlay_visible
            {
                Self::input_qp_end_session_locked(inner, true, None);
            }
            inner.qp.pending_commands = fifo_cancels;
            inner.qp.machine.forget_held_keys_after_input_gap();
            inner.qp.machine.set_chord_drain_active(false);
        }
        inner.qp.mouse.reset_after_input_gap();
        keep_qp
    }

    fn input_hide_snapshot(inner: &HostInner) -> HostHideSnapshot {
        Self::hide_snapshot_locked(inner, inner.input_can_tuck)
    }

    fn input_super_menu_hide_session(&self, inner: &mut HostInner) {
        if let Some(id) = inner.super_menu.session_id.as_deref() {
            self.menus.try_retire(id);
            inner.input_menu_cleanup.push(id.to_owned());
            inner.keyboard_wakeup.notify_one();
        }
        if inner.super_menu.visible {
            super_menu_clear_session(inner);
        } else {
            inner.super_menu.native_keyboard.retire_presentation();
            inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
            inner.super_menu.loading = false;
        }
    }

    pub(super) fn poll_input_cleanup(&self) {
        let pending = std::mem::take(&mut self.inner.lock().input_menu_cleanup);
        for id in pending {
            self.menus.close(&id);
        }
    }

    pub(super) fn input_dispatch_low_level_key_for_foreground(
        &self,
        inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        history_shortcut_fresh: bool,
    ) -> bool {
        // A page can finish while Tauri is still creating other WebViews. Do
        // not consume input until its queued commands can actually be handled.
        let (input_suspended, hwnds, repeat) = {
            // Track releases even while hidden, modal or not yet ready. A held
            // key must not become a fresh execution when a list reappears.
            let repeat = inner
                .keyboard_keys_down
                .get_mut(ev.vk as usize)
                .map(|down| {
                    let repeated = ev.down && *down && !history_shortcut_fresh;
                    *down = ev.down;
                    repeated
                })
                .unwrap_or(false);
            (
                !inner.window_runtime_ready
                    || inner.ole_drag_active
                    || inner.native_incoming_drag.is_some(),
                [inner.host_hwnd, inner.panel_hwnd, inner.main_hwnd],
                repeat,
            )
        };
        let input_suspended =
            input_suspended || hwnds.into_iter().any(crate::window_chrome::is_in_move_size);
        if self.input_dispatch_native_fifo_held_key(inner, ev, foreground, input_suspended, repeat)
        {
            self.input_cancel_text_instruction(inner);
            return true;
        }
        // A swallowed native menu press owns its repeats/release even after
        // hide, root replacement, a foreground change, or another popup opens.
        if self.input_dispatch_native_super_menu_held_key(
            inner,
            ev,
            foreground,
            input_suspended,
            repeat,
        ) {
            self.input_cancel_text_instruction(inner);
            return true;
        }
        if Self::input_qp_drain_escape_locked(inner, ev) {
            self.input_cancel_text_instruction(inner);
            return true;
        }
        if self.input_drain_native_file_line_key(inner, ev, foreground, input_suspended, repeat) {
            self.input_cancel_text_instruction(inner);
            return true;
        }
        if self.input_route_native_context_menu_key(inner, ev, foreground, input_suspended, repeat) {
            self.input_cancel_text_instruction(inner);
            return true;
        }
        if self.input_dispatch_native_context_menu_escape(inner, ev) {
            return true;
        }
        // Windows owns input during its move/resize loop. A partial external
        // text trigger must not survive that change of input ownership.
        if input_suspended {
            self.input_cancel_text_instruction(inner);
            return false;
        }
        if ev.down {
            inner.vv_tab_pending = None;
            inner.vv_tab_generation = inner.vv_tab_generation.wrapping_add(1);
        }
        // Only keys passed to the foreground app may form its vv/vvv prefix.
        // In particular, letters used for no-focus search never reached it.
        let swallowed =
            self.input_route_low_level_key_for_foreground(inner, ev, foreground, repeat, history_shortcut_fresh);
        if inner.file_browser.visible && inner.file_browser.hwnd != 0 && foreground == inner.file_browser.hwnd {
            self.input_cancel_text_instruction(inner);
            return swallowed;
        }
        if !ev.down {
            return swallowed;
        }
        let action = {
            if swallowed || !inner.config.enable_vv_tab_text_instruction {
                inner.vv_tab.reset();
                inner.vv_tab_foreground = None;
                None
            } else {
                if inner.vv_tab_foreground != Some(foreground) {
                    inner.vv_tab.reset();
                    inner.vv_tab_foreground = Some(foreground);
                }
                inner.vv_tab.on_keydown(
                    ev.vk,
                    ev.alt || ev.ctrl || ev.shift || ev.win,
                    Instant::now(),
                )
            }
        };
        if let Some(crate::vv_tab::VvTabAction::Show { backspaces }) = action {
            if let Ok(target) = InputTarget::capture(foreground) {
                // SendInput can reenter hooks. The worker verifies this same
                // target and input generation before sending any backspaces.

                inner.vv_tab_pending = Some(PendingTextInstruction {
                    backspaces,
                    target,
                    hwnd: foreground,
                    queued_at: Instant::now(),
                    generation: inner.vv_tab_generation,
                });
                inner.keyboard_wakeup.notify_one();
                return true;
            }
        }
        swallowed
    }

    pub(super) fn input_route_low_level_key_for_foreground(
        &self,
        inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        repeat: bool,
        history_shortcut_fresh: bool,
    ) -> bool {
        // BeginSession takes over Escape before the candidate worker can show
        // its overlay. Preserve native held-press drains in the caller, but
        // route a fresh Escape through that existing QP session first.
        let qp_escape_first = ev.vk == 0x1B
            && (inner.qp.escape_down
                || (ev.down && !repeat
                    && !ev.ctrl && !ev.alt && !ev.shift && !ev.win
                    && (inner.qp.machine.phase != QuickPastePhase::Idle
                        || inner.qp.overlay_visible)));
        let qp_result = qp_escape_first.then(|| self.input_qp_handle_key(inner, ev));
        if qp_result
            .as_ref()
            .and_then(|result| result.get("verdict"))
            .and_then(Value::as_str)
            == Some("Swallow")
        {
            return true;
        }
        let owners = { [inner.host_hwnd, inner.panel_hwnd, inner.main_hwnd] };
        let owned_dialog = owners
            .into_iter()
            .any(|owner| crate::host_hit_test::is_owned_dialog(owner, foreground));
        let (native_menu, qp_visible) = {
            (
                inner.super_menu.native_keyboard.owns_presentation(&inner),
                inner.qp.overlay_visible,
            )
        };
        if ev.vk == 0x1B
            && !ev.ctrl
            && !ev.alt
            && !ev.shift
            && !ev.win
            && !owned_dialog
            && !qp_visible
        {
            if let Some(swallowed) =
                self.input_route_native_super_menu_key(inner, ev, foreground, repeat)
            {
                return swallowed;
            }
        }
        if ev.down
            && ev.vk == 0x1B
            && !ev.ctrl
            && !ev.alt
            && !ev.shift
            && !ev.win
            && !owned_dialog
            && !native_menu
            && (inner.super_menu.visible || inner.super_menu.loading)
        {
            // C# Super Menu overlay `OnEscKeyDown` swallows Esc and HideSession.
            self.input_super_menu_hide_session(inner);
            return true;
        }
        let result = qp_result.unwrap_or_else(|| self.input_qp_handle_key(inner, ev));
        if result.get("verdict").and_then(|v| v.as_str()) == Some("Swallow") {
            return true;
        }
        // Quick paste is the top keyboard layer while opening, visible, or
        // awaiting its release worker. Unsupported/disabled keys belong to
        // the foreground, never to a still-visible list or panel behind it.
        if inner.qp.machine.phase != QuickPastePhase::Idle || inner.qp.overlay_visible {
            return false;
        }
        // Native file/settings dialogs must receive ordinary keys, even if the
        // clipboard or a panel still has fake focus. Global quick paste stays above.
        if owned_dialog || (inner.reopen.visible && inner.reopen.hwnd != 0 && foreground == inner.reopen.hwnd)
            || (inner.file_browser.visible && inner.file_browser.hwnd != 0 && foreground == inner.file_browser.hwnd) {
            return false;
        }
        if let Some(swallowed) =
            self.input_route_native_super_menu_key(inner, ev, foreground, repeat)
        {
            return swallowed;
        }
        if let Some(swallowed) = self.input_route_native_fifo_key(inner, ev, foreground, repeat) {
            return swallowed;
        }
        if let Some(swallowed) = self.input_route_native_file_line_key(inner, ev, foreground, repeat) {
            return swallowed;
        }
        // Modified Escape belongs to the foreground application/OS (including
        // Shift+Escape). An active quick-paste session kept its explicit routing above.
        if ev.vk == 0x1B && (ev.ctrl || ev.alt || ev.shift || ev.win) {
            return false;
        }
        // A panel layer owns ordinary input. Do not let a paused panel fall
        // through to a simultaneously visible clipboard's fake-focus routing.
        {
            if inner.ui.panel_visible
                && inner.ui.panel_fake_focus
                && (inner.ui.panel_spa_overlay_open || native_context_menu_for(&inner, "panel"))
            {
                return false;
            }
        }
        if ev.down
            && ev.vk == 0x1B
            && !ev.alt
            && !ev.ctrl
            && !ev.shift
            && !ev.win
            && self.input_try_hide_panel_on_escape(inner)
        {
            return true;
        }
        // The panel owns ordinary input throughout its fake-focus session.
        // Unsupported keys, including modified navigation, must pass through
        // instead of operating the still-visible clipboard behind it.
        // Native Esc above remains available even while its page is loading.
        let panel_owns_input = { inner.ui.panel_visible && inner.ui.panel_fake_focus };
        if panel_owns_input {
            return self.input_try_panel_no_focus_navigation(inner, ev, repeat);
        }
        if self.input_try_clipboard_no_focus_key(inner, ev, foreground, repeat, history_shortcut_fresh) {
            return true;
        }
        if ev.down
            && ev.vk == 0x1B
            && !ev.ctrl
            && !ev.alt
            && !ev.shift
            && !ev.win
            && self.input_queue_fifo_escape_cancel(inner, foreground, repeat)
        {
            return true;
        }
        if !ev.down {
            return false;
        }
        let using_digits = inner.config.using_number_key_to_paste;
        let mut snap = Self::input_hide_snapshot(inner);
        // Preserve the no-focus handler's pass-through decisions in the fallback policy.
        snap.modifier_blocking = ev.ctrl || ev.alt || ev.win;
        snap.defer_host_keyboard_routing = snap.spa_modal_open
            || inner.clipboard_keyboard_mode != ClipboardKeyboardMode::Full;
        snap.is_digit_key = using_digits && digit_0_to_9(ev.vk).is_some();
        snap.is_escape = ev.vk == 0x1B;
        snap.is_navigation_key = is_navigation_key(ev.vk);
        let decision = evaluate(HostHideTrigger::KeyDown, &snap);
        Self::apply_hide_decision_locked(inner, &decision, HostHideTrigger::KeyDown);
        match decision.keyboard_routing {
            crate::hide_policy::HostKeyboardRouting::SwallowDigit => {
                if let Some(n) = csharp_digit_key_value(ev.vk) {
                    self.input_route_digit(inner, n, repeat);
                }
                true
            }
            crate::hide_policy::HostKeyboardRouting::SwallowNavigation => {
                if ev.vk == 0x0D && repeat {
                    return true;
                }
                if let Some(key) = web_navigation_key_name(ev.vk) {
                    push_chrome(
                        inner,
                        ChromeCommand::ClipboardNavKey {
                            key,
                            shift: ev.shift,
                        },
                    );
                }
                true
            }
            crate::hide_policy::HostKeyboardRouting::SwallowEscapeHide => true,
            _ => false,
        }
    }

    pub(super) fn input_queue_fifo_escape_cancel(
        &self,
        inner: &mut HostInner,
        foreground: isize,
        repeat: bool,
    ) -> bool {
        let owners = { [inner.host_hwnd, inner.panel_hwnd, inner.main_hwnd] };
        for owner in owners {
            match self.input_dismiss_relation(inner, owner, foreground) {
                Some(false) => {}
                Some(true) | None => return false,
            }
        }

        if !inner.qp_hook_enabled
            || inner.qp.machine.phase != QuickPastePhase::Idle
            || !inner.fifo.as_ref().is_some_and(|ids| !ids.is_empty())
            || inner.move_interaction_active
            || (inner.window_visible
                && (inner.spa_modal_open
                    || inner.spa_overlay_open
                    || native_context_menu_for(&inner, "clipboard")
                    || inner.ui.clipboard_editing
                    || inner.is_active))
            || (inner.ui.panel_visible
                && (inner.ui.panel_spa_overlay_open || native_context_menu_for(&inner, "panel")))
        {
            return false;
        }
        if !repeat {
            let generation = inner.qp.hook_generation;
            let cancel_generation = inner.qp.cancel_generation;
            let fifo_generation = inner.fifo_generation;
            inner
                .qp
                .pending_commands
                .push_back(PendingQuickPasteCommand {
                    command: QuickPasteCommand::CancelFifo {
                        generation: fifo_generation,
                    },
                    source: QuickPasteInputSource::Keyboard,
                    generation,
                    cancel_generation,
                    press_index: 0,
                    origin: None,
                    disarm_shortcut_menu: false,
                });
            inner.keyboard_wakeup.notify_one();
        }
        true
    }

    pub(super) fn input_dispatch_low_level_side_button(
        &self,
        inner: &mut HostInner,
        button: QuickPasteMouseButton,
        down: bool,
    ) -> bool {
        let released_mouse_session =
            matches!(inner.qp.hook_source, QuickPasteInputSource::Mouse(_))
                && inner
                    .qp
                    .released_mouse_generations
                    .contains(&inner.qp.hook_generation);
        if down
            && !inner.qp.mouse.is_held()
            && (!inner.window_runtime_ready
                || inner.move_interaction_active
                || inner.ole_drag_active
                || inner.native_incoming_drag.is_some()
                || (inner.qp.machine.phase != QuickPastePhase::Idle && !released_mouse_session))
        {
            return false;
        }
        let (verdict, action) = inner.qp.mouse.handle_button(button, down);
        match action {
            MouseGestureAction::Armed => {
                inner.qp.mouse_press_foreground = crate::window_menu::foreground_hwnd();
            }
            MouseGestureAction::Commit => {
                let generation = inner.qp.hook_generation;
                inner.qp.released_mouse_generations.insert(generation);
                let paste_enter = inner.qp.machine.paste_enter_armed;
                Self::qp_queue_mouse_command(
                    inner,
                    QuickPasteCommand::Commit { paste_enter },
                    None,
                );
            }
            _ => {}
        }

        if action == MouseGestureAction::Armed {
            // Receive the chord's wheel events ahead of hooks installed since boot.
            crate::hooks::request_quick_paste_mouse_priority();
        }
        verdict == HookVerdict::Swallow
    }

    pub(super) fn input_dispatch_low_level_quick_paste_right_button(
        &self,
        inner: &mut HostInner,
        down: bool,
    ) -> bool {
        if !down {
            return std::mem::take(&mut inner.qp.mouse_cancel_right_down);
        }
        if inner.qp.mouse_cancel_right_down {
            return true;
        }
        if !matches!(inner.qp.hook_source, QuickPasteInputSource::Mouse(_))
            || !matches!(
                inner.qp.machine.phase,
                QuickPastePhase::SessionStarting | QuickPastePhase::SessionActive
            )
        {
            return false;
        }
        inner.qp.mouse_cancel_right_down = true;
        // Retire loading and already drained worker commands atomically at
        // Down. HideQuickPaste is sent to the UI; no input or window API runs
        // on the low-level hook thread. Cancel also drains the held side key.
        Self::input_qp_end_session_locked(inner, true, None);
        true
    }

    pub(super) fn input_dispatch_low_level_wheel(
        &self,
        inner: &mut HostInner,
        delta: i16,
        screen_x: i32,
        screen_y: i32,
    ) -> bool {
        if delta == 0 {
            return false;
        }

        let button = inner.qp.mouse.held_button();
        let (verdict, action) = inner.qp.mouse.handle_wheel(delta);
        match action {
            MouseGestureAction::BeginSession => {
                let Some(button) = button else {
                    return false;
                };
                inner.qp.machine.begin_mouse_session();
                inner.native_fifo_keyboard.retire_binding();
                inner.qp.hook_generation = inner.qp.hook_generation.wrapping_add(1);
                inner.qp.hook_fifo_generation = inner.fifo.as_ref().filter(|ids| !ids.is_empty())
                    .map(|_| inner.fifo_generation);
                inner.qp.hook_source = QuickPasteInputSource::Mouse(button);
                let origin = Some((inner.qp.mouse_press_foreground, Some((screen_x, screen_y))));
                Self::qp_queue_mouse_command(inner, QuickPasteCommand::BeginSession, origin);
            }
            MouseGestureAction::MoveSelection { delta } => {
                Self::qp_queue_mouse_command(
                    inner,
                    QuickPasteCommand::MoveSelection { delta },
                    None,
                );
            }
            _ => {}
        }
        if verdict == HookVerdict::Swallow {
            return true;
        }
        if matches!(inner.qp.hook_source, QuickPasteInputSource::Mouse(_)) {
            return false;
        }
        let (session_active, overlay_ready, inside) = {
            let session_active = inner.qp.overlay_visible
                && matches!(
                    inner.qp.machine.phase,
                    QuickPastePhase::SessionActive | QuickPastePhase::SessionStarting
                );
            let overlay_ready = inner.qp.overlay_ready && inner.qp.overlay_opacity > 0.0;
            let inside =
                point_inside_wheel_hit(screen_x, screen_y, &inner.qp_wheel_hit, WHEEL_HIT_SLOP_PX);
            (session_active, overlay_ready, inside)
        };
        if !should_consume_mouse_wheel(session_active, overlay_ready, inside) {
            return false;
        }
        let step = mouse_wheel_selection_delta(delta);
        if step != 0 {
            // Like keyboard navigation, queue the move in input order. Building
            // the full state payload (including pin-provider process queries)
            // inside WH_MOUSE_LL can time out and silently lose the hook.
            let generation = inner.qp.session_generation;
            let cancel_generation = inner.qp.cancel_generation;
            let press_index = inner.qp.machine.trigger_press_index();
            inner
                .qp
                .pending_commands
                .push_back(PendingQuickPasteCommand {
                    command: QuickPasteCommand::MoveSelection { delta: step },
                    source: QuickPasteInputSource::Keyboard,
                    generation,
                    cancel_generation,
                    press_index,
                    origin: None,
                    disarm_shortcut_menu: false,
                });
            inner.keyboard_wakeup.notify_one();
        }
        true
    }

    pub(super) fn input_try_clipboard_no_focus_key(
        &self,
        inner: &mut HostInner,
        ev: KeyEvent,
        _foreground: isize,
        repeat: bool,
        history_shortcut_fresh: bool,
    ) -> bool {
        let session = { inner.no_focus_session_active && inner.window_visible };
        if !session {
            return false;
        }
        // Releasing CapsLock ends its guide even when foreground changed before
        // KeyUp. Clipboard participation itself can span that foreground change.
        if ev.vk == 0x14 && !ev.down {
            inner.caps_guide_until_keyup = false;
        }
        if inner.clipboard_keyboard_mode == ClipboardKeyboardMode::Released {
            return false;
        }
        if inner.clipboard_keyboard_mode == ClipboardKeyboardMode::DigitsOnly {
            if !ev.down { return false; }
            // This physical press belongs to the external app, including the
            // first letter/IME key of abc123 and shifted number-row symbols.
            if external_clipboard_input_ends_digits(ev) {
                set_clipboard_keyboard_mode_locked(inner, ClipboardKeyboardMode::Released);
                return false;
            }
            if inner.spa_modal_open || inner.spa_overlay_open
                || native_context_menu_for(inner, "clipboard") || !inner.clipboard_web_view_ready
                || !self.input_clipboard_keys_engaged(inner)
            {
                return false;
            }
            if ev.ctrl || ev.alt || ev.shift || ev.win { return false; }
            if ev.vk == 0x1B {
                self.input_queue_clipboard_handle_escape(inner);
                return true;
            }
            if inner.config.using_number_key_to_paste && !inner.clipboard_search_active
                && is_clipboard_chrome_route(&inner.ui.clipboard_route)
            {
                if let Some(digit) = csharp_digit_key_value(ev.vk) {
                    self.input_route_digit(inner, digit, repeat);
                    return true;
                }
            }
            return false;
        }
        if ev.vk == 0x14 {
            let engaged = self.input_clipboard_keys_engaged(inner);
            if ev.down && engaged {
                inner.caps_guide_until_keyup = true;
            }
            return false;
        }
        if !ev.down {
            return false;
        }
        let (modal, guide) = {
            (
                inner.spa_modal_open
                    || inner.spa_overlay_open
                    || native_context_menu_for(&inner, "clipboard"),
                inner.caps_guide_until_keyup || caps_lock_physically_down(),
            )
        };
        if modal {
            return false;
        }
        // Do not relinquish no-activate for search while its page is loading:
        // the UI consumer rejects input commands until this WebView is ready.
        if !inner.clipboard_web_view_ready {
            return false;
        }
        // Only the explicit Ctrl+F chord activates search. Extra modifiers must
        // remain available to the foreground app instead of stealing input focus.
        let engaged = self.input_clipboard_keys_engaged(inner);
        if engaged && ev.ctrl && !ev.alt && !ev.shift && !ev.win
            && native_history_shortcuts_available(inner)
        {
            let command = match ev.vk { 0x41 => Some("SelectAll"), 0x43 => Some("Copy"),
                0x50 => Some("Pin"), _ => None };
            if let Some(key) = command {
                if !history_shortcut_fresh || !self.native_history_shortcut_keys.claim(ev.vk) {
                    return false;
                }
                push_chrome(inner, ChromeCommand::ClipboardNavKey { key, shift: false });
                return true;
            }
        }
        if engaged && ev.ctrl && !ev.alt && !ev.shift && !ev.win && ev.vk == 0x46 {
            self.input_request_host_keyboard_focus(inner, "clipboard");
            push_chrome(inner, ChromeCommand::ClipboardActivateSearch);
            return true;
        }
        if ev.ctrl || ev.alt || ev.win {
            return false;
        }
        if !engaged {
            return false;
        }
        if ev.vk == 0x0D && repeat {
            return true;
        }
        if let Some(n) = csharp_digit_key_value(ev.vk) {
            self.input_route_digit(inner, n, repeat);
            return true;
        }
        if guide && is_caps_guide_letter(ev.vk) {
            if ev.vk == 0x51 {
                // C# Caps+Q calls `QueueHandleEscapeFromLowLevelHook`, not an immediate hide.
                self.input_queue_clipboard_handle_escape(inner);
                return true;
            }
            if ev.vk == 0x4F {
                push_chrome(inner, ChromeCommand::ClipboardSearchBackspace);
                return true;
            }
            if let Some(key) = caps_guide_nav_key(ev.vk) {
                push_chrome(
                    inner,
                    ChromeCommand::ClipboardNavKey {
                        key,
                        shift: ev.shift,
                    },
                );
                return true;
            }
            return true;
        }
        if ev.vk == 0x1B {
            self.input_queue_clipboard_handle_escape(inner);
            return true;
        }
        if ev.vk == 0x09 {
            push_chrome(
                inner,
                ChromeCommand::ClipboardSwitchTab { reverse: ev.shift },
            );
            return true;
        }
        if ev.vk == 0x08 {
            push_chrome(inner, ChromeCommand::ClipboardSearchBackspace);
            return true;
        }
        if let Some(key) = web_navigation_key_name(ev.vk) {
            push_chrome(
                inner,
                ChromeCommand::ClipboardNavKey {
                    key,
                    shift: ev.shift,
                },
            );
            return true;
        }
        if let Some(ch) = search_char_from_vk(ev.vk, ev.shift) {
            push_chrome(inner, ChromeCommand::ClipboardAppendSearch(ch));
            return true;
        }
        false
    }

    pub(super) fn input_queue_clipboard_handle_escape(&self, inner: &mut HostInner) {
        inner.caps_guide_until_keyup = false;
        push_chrome(inner, ChromeCommand::ClipboardHandleEscape);
    }

    pub(super) fn input_clipboard_keys_engaged(&self, inner: &mut HostInner) -> bool {
        let (fake, hwnd, fold, flagged, visible, session) = {
            (
                inner.has_host_fake_focus,
                inner.host_hwnd,
                DismissBehavior::parse(&inner.config.dismiss_behavior) == DismissBehavior::Fold,
                inner.pointer_in_host,
                inner.window_visible,
                inner.no_focus_session_active,
            )
        };
        if !session || !visible {
            return false;
        }
        if inner.clipboard_keyboard_mode == ClipboardKeyboardMode::Released {
            return false;
        }
        if fake {
            return true;
        }
        if hwnd == 0 || crate::clipboard::is_headless() {
            return flagged;
        }
        #[cfg(windows)]
        {
            let Some((x, y)) = cursor_screen_point() else {
                return false;
            };
            if fold {
                return self.input_point_in_host(inner, x, y);
            }
            return input_hit(hwnd, x, y);
        }
        #[cfg(not(windows))]
        {
            let _ = (hwnd, fold);
            flagged
        }
    }

    pub(super) fn input_try_panel_no_focus_navigation(
        &self,
        inner: &mut HostInner,
        ev: KeyEvent,
        repeat: bool,
    ) -> bool {
        if !ev.down || ev.alt || ev.ctrl || ev.shift || ev.win {
            return false;
        }
        let (visible, session, fake) = {
            (
                inner.ui.panel_visible,
                inner.ui.panel_no_focus_session,
                inner.ui.panel_fake_focus,
            )
        };
        if !visible || !session || !fake {
            return false;
        }
        // C# nav keys wait for WebView2. Esc hide is handled earlier and does not.
        if !inner.panel_web_view_ready {
            return false;
        }
        if ev.vk == 0x0D && repeat {
            return true;
        }
        let Some(key) = web_navigation_key_name(ev.vk) else {
            return false;
        };
        push_chrome(inner, ChromeCommand::PanelNavKey(key));
        true
    }

    pub(super) fn input_dispatch_native_context_menu_escape(
        &self,
        inner: &mut HostInner,
        ev: KeyEvent,
    ) -> bool {
        if ev.vk != 0x1B {
            return false;
        }

        // A held Escape that closed the menu must not then close its owner.
        if inner.native_context_menu_escape_down {
            if !ev.down {
                inner.native_context_menu_escape_down = false;
            }
            return true;
        }
        // GPUI FIFO owns its paired Escape and IME/start guards. A retired
        // opt-in cannot fall back to generic menu Escape on the same lease.
        if inner.native_fifo_keyboard.owns_presentation(&inner) {
            return false;
        }
        if !ev.down || ev.ctrl || ev.alt || ev.shift || ev.win {
            return false;
        }
        let Some(menu) = inner.native_context_menu else {
            return false;
        };
        // Editable popups receive Escape at their actual focused input. IME
        // composition can start/end between polls, so a cached composing bit
        // cannot safely decide whether this press should close the popup.
        if menu.text_input {
            return false;
        }
        release_native_context_menu_locked(inner, menu.id);
        inner.native_context_menu_escape_down = true;
        push_chrome(inner, ChromeCommand::DismissContextMenu(menu.id));
        true
    }

    pub(super) fn input_dispatch_native_context_menu_mouse(
        &self,
        inner: &mut HostInner,
        message: u32,
        x: i32,
        y: i32,
    ) {
        if !matches!(message, 0x0201 | 0x0204) {
            return;
        }
        let menu = inner.native_context_menu;
        let Some(menu) = menu else {
            return;
        };
        let inside = native_context_menu_hit(menu.popup_hwnd, x, y)
            || native_context_menu_trigger_hit(menu.trigger, native_context_menu_owner_hwnd(inner, menu.owner), x, y);
        if inside {
            return;
        }

        if release_native_context_menu_locked(inner, menu.id) {
            push_chrome(inner, ChromeCommand::DismissContextMenu(menu.id));
        }
    }

    pub(super) fn input_point_in_host(&self, inner: &mut HostInner, x: i32, y: i32) -> bool {
        let (hwnd, pad, use_tree, visible) = {
            let behavior = DismissBehavior::parse(&inner.config.dismiss_behavior);
            (
                inner.host_hwnd,
                crate::hide_policy::pointer_area_ext_px(behavior),
                crate::hide_policy::pointer_uses_host_tree(behavior),
                inner.window_visible,
            )
        };
        // C# `IsPointerEngagingHost`: `!IsVisible` is never inside.
        if !visible {
            return false;
        }
        if hwnd == 0 {
            return inner.pointer_in_host;
        }
        #[cfg(windows)]
        unsafe {
            if inner.edge_tucked {
                return crate::host_hit_test::screen_point_engages_edge_strip(x, y, hwnd);
            }
            // EdgeHide must respect foreign windows covering the host. The tree
            // helper already handles native chrome and transparent fall-through.
            if use_tree {
                return input_hit(hwnd, x, y);
            }
            // A visible owned popup extends the pointer area. Fold/Hide still
            // reject stale WebView child bounds and retain HWND + AreaExt below.
            if input_hit(hwnd, x, y) {
                return true;
            }
            use windows::Win32::Foundation::{HWND, RECT};
            use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
            let mut rc = RECT::default();
            GetWindowRect(HWND(hwnd as *mut _), &mut rc).is_ok()
                && crate::hide_policy::point_in_window_rect(
                    x, y, rc.left, rc.top, rc.right, rc.bottom, pad,
                )
        }
        #[cfg(not(windows))]
        {
            let _ = (x, y, pad, use_tree);
            inner.pointer_in_host
        }
    }

    pub(super) fn input_set_pointer_in_host(&self, inner: &mut HostInner, inside: bool) {
        if inner.pointer_in_host
            && !inside
            && inner.no_focus_session_active
            && !inner.has_host_fake_focus
        {
            // Hover alone may own list input. Leaving ends that ownership, so
            // returning to the same pinned window cannot revive its old Enter.
            drop_queued_clipboard_list_input(inner);
        }
        inner.pointer_in_host = inside;
        // C# EdgeHide's MouseEntered restores the existing window; it does not
        // start a new no-focus session or give hover persistent fake focus.
        if inside
            && inner.window_visible
            && inner.edge_tucked
            && !inner.is_collapsed
            && !clipboard_move_active(&inner)
        {
            inner.edge_tucked = false;
            push_chrome(inner, ChromeCommand::RestoreEdge);
        }
    }

    pub(super) fn input_cancel_text_instruction(&self, inner: &mut HostInner) {
        inner.vv_tab.reset();
        inner.vv_tab_foreground = None;
        inner.vv_tab_pending = None;
        inner.vv_tab_generation = inner.vv_tab_generation.wrapping_add(1);
    }

    pub(super) fn input_notify_outside_click_at(&self, inner: &mut HostInner, x: i32, y: i32) {
        self.input_cancel_text_instruction(inner);
        self.input_apply_clipboard_outside_click(inner, x, y);
        self.input_apply_panel_outside_click(inner, x, y);
    }

    pub(super) fn input_notify_panel_outside_click_at(
        &self,
        inner: &mut HostInner,
        x: i32,
        y: i32,
    ) {
        self.input_cancel_text_instruction(inner);
        // Clipboard PreviewMouseDown still reacquires fake focus for an internal
        // right click. It must not turn an external right click into dismissal.
        self.input_apply_clipboard_mouse_down(inner, x, y, false);
        self.input_apply_panel_outside_click(inner, x, y);
    }

    pub(super) fn input_apply_clipboard_outside_click(
        &self,
        inner: &mut HostInner,
        x: i32,
        y: i32,
    ) {
        self.input_apply_clipboard_mouse_down(inner, x, y, true);
    }

    pub(super) fn input_apply_clipboard_mouse_down(
        &self,
        inner: &mut HostInner,
        x: i32,
        y: i32,
        dismiss_outside: bool,
    ) {
        let pinned = {
            if !inner.window_visible
                || !inner.no_focus_session_active
                || inner.move_interaction_active
            {
                return;
            }
            inner.window_pinned
        };
        let hit = super::input_companion_cache::input_window_from_point(x, y);
        let Some(inside) = self.input_dismiss_relation(inner, inner.host_hwnd, hit) else {
            return;
        };
        if inside {
            set_clipboard_keyboard_mode_locked(inner, ClipboardKeyboardMode::Full);
        } else if inner.clipboard_keyboard_mode == ClipboardKeyboardMode::Full {
            set_clipboard_keyboard_mode_locked(inner, ClipboardKeyboardMode::DigitsOnly);
        }
        // External clicks preserve only number paste; pinning still controls
        // dismissal, and right clicks still never dismiss the clipboard.
        if !inside && (pinned || !dismiss_outside) {
            return;
        }
        if inside {
            // A new internal click supersedes old deactivation even while an
            // overlay's policy gate preserves the current fake-focus value.
            inner.clipboard_deactivation_pending = false;
        }
        let mut snap = Self::input_hide_snapshot(inner);
        snap.pointer_in_host = inside;
        let decision = evaluate(HostHideTrigger::OutsideClick, &snap);
        Self::apply_hide_decision_locked(inner, &decision, HostHideTrigger::OutsideClick);
    }

    pub(super) fn input_apply_panel_outside_click(&self, inner: &mut HostInner, x: i32, y: i32) {
        let (visible, session, pinned, hwnd, overlay) = {
            (
                inner.ui.panel_visible,
                inner.ui.panel_no_focus_session,
                inner.ui.panel_pinned,
                inner.panel_hwnd,
                inner.ui.panel_spa_overlay_open
                    || native_context_menu_for(&inner, "panel")
                    || native_incoming_drag_for(&inner, "panel"),
            )
        };
        if !visible || !session || overlay || crate::window_chrome::is_in_move_size(hwnd) {
            return;
        }
        let hit = super::input_companion_cache::input_window_from_point(x, y);
        let Some(inside) = self.input_dismiss_relation(inner, hwnd, hit) else {
            return;
        };
        let lost_fake_focus = {
            let was = inner.ui.panel_fake_focus;
            inner.ui.panel_fake_focus = inside;
            let lost = was && !inside;
            if lost {
                drop_queued_panel_navigation(inner);
            }
            lost
        };
        if lost_fake_focus && !pinned {
            self.input_apply_panel_hide(inner);
        }
    }


    pub(super) fn input_dispatch_super_menu_mouse_at(
        &self,
        inner: &mut HostInner,
        msg: u32,
        screen_x: i32,
        screen_y: i32,
        now: Instant,
    ) {
        let Some(kind) = crate::window_menu::super_menu_mouse_kind(msg) else {
            return;
        };
        let dismiss = {
            if !inner.super_menu.visible {
                return;
            }
            let armed = inner
                .super_menu
                .shown_at
                .is_some_and(|at| crate::window_menu::super_menu_outside_click_armed(at, now));
            let hitting = crate::window_menu::screen_point_hits_overlay(
                inner.super_menu_hwnd,
                screen_x,
                screen_y,
            );
            crate::window_menu::apply_super_menu_outside_click(
                kind,
                hitting,
                armed,
                now,
                &mut inner.super_menu.content_suppress_until,
            )
        };
        if dismiss {
            self.input_super_menu_hide_session(inner);
        }
    }

    pub(super) fn input_request_host_keyboard_focus(
        &self,
        inner: &mut HostInner,
        source: &'static str,
    ) {
        let (visible, no_focus) = match source {
            "clipboard" => (inner.window_visible, inner.no_focus_session_active),
            "panel" => (inner.ui.panel_visible, inner.ui.panel_no_focus_session),
            "main" => (inner.ui.main_visible, false),
            _ => return,
        };
        if !visible {
            return;
        }
        let activate = no_focus || inner.ui.keyboard_focus_host != Some(source);
        if activate {
            // A newer DOM input request supersedes queued focus/search work
            // from the previous window before the UI thread can execute it.
            for epoch in &mut inner.chrome_input_epochs {
                epoch.window = epoch.window.wrapping_add(1);
            }
            inner
                .chrome
                .retain(|command| command.input_window().is_none());
        }
        inner.ui.keyboard_focus_host = Some(source);
        if activate || source != "clipboard" {
            end_no_focus_session_locked(inner);
            // Initialize the guard only for a new handoff. Like C#'s early
            // return after ExitNoFocusSessionForKeyboardInput, repeated DOM
            // focusin must not revive fake focus or erase a deferred blur.
            inner.has_host_fake_focus = source == "clipboard" && inner.config.enable_no_focus_mode;
            inner.is_active = source == "clipboard";
            inner.foreground_is_host_related = source == "clipboard";
            mark_host_no_activate_dirty(inner);
        }
        // Even a repeated request releases input from another newly shown
        // no-focus window, without recreating this input owner's fake focus.
        inner.ui.panel_no_focus_session = false;
        if activate || source != "panel" {
            inner.ui.panel_fake_focus = source == "panel";
        }
        if activate {
            let command = if source == "clipboard" {
                ChromeCommand::ClipboardKeyboardFocus
            } else {
                ChromeCommand::HostKeyboardFocus(source)
            };
            push_chrome(inner, command);
        }
    }

    pub(super) fn input_try_hide_panel_on_escape(&self, inner: &mut HostInner) -> bool {
        let (visible, session, fake) = {
            (
                inner.ui.panel_visible,
                inner.ui.panel_no_focus_session,
                inner.ui.panel_fake_focus,
            )
        };
        if !visible || !session || !fake {
            return false;
        }
        self.input_apply_panel_hide(inner);
        true
    }

    pub(super) fn input_apply_panel_hide(&self, inner: &mut HostInner) {
        drop_queued_window_input(inner, "panel");
        cancel_window_presentation(inner, "panel");
        inner.ui.panel_visible = false;
        inner.ui.panel_no_focus_session = false;
        inner.ui.panel_fake_focus = false;
        push_chrome(inner, ChromeCommand::HidePanel);
    }

    pub(super) fn input_route_digit(&self, inner: &mut HostInner, n: u8, repeat: bool) {
        // C# `DispatchRouteDigitKeyToWebViewAsync` — SPA `routeDigitKey` (visible slot / search).

        push_chrome(inner, ChromeCommand::RouteDigit { digit: n, repeat });
        inner.capture.push("digitKey", json!({ "digit": n }));
    }

    fn input_qp_handle_key(&self, inner: &mut HostInner, ev: KeyEvent) -> Value {
        let (verdict, cmd) = {
            if Self::input_qp_drain_escape_locked(inner, ev) {
                return json!({ "verdict": "Swallow", "command": "None" });
            }
            if !inner.qp_hook_enabled
                && matches!(inner.qp.machine.phase, QuickPastePhase::Idle)
                && !inner.qp.mouse.is_held()
                && !inner.qp.overlay_visible
            {
                return json!({ "verdict": "Pass", "command": "None" });
            }
            let mouse_session = matches!(inner.qp.hook_source, QuickPasteInputSource::Mouse(_))
                && inner.qp.machine.phase != QuickPastePhase::Idle;
            if inner.qp.mouse.is_held() && !mouse_session {
                if ev.down
                    && ev.vk == crate::quick_paste::VK_ESCAPE
                    && !ev.alt
                    && !ev.ctrl
                    && !ev.shift
                    && !ev.win
                {
                    inner.qp.mouse.cancel();
                }
                return json!({ "verdict": "Pass", "command": "None" });
            }
            let paste_enter_enabled = inner.qp_config.enable_paste_enter_shortcut;
            inner.qp.machine.set_paste_enter_shortcut_enabled(paste_enter_enabled);
            let (verdict, cmd) = if Self::input_qp_pending_escape_locked(inner, ev) {
                (crate::quick_paste::HookVerdict::Swallow, QuickPasteCommand::Cancel)
            } else if mouse_session {
                if !inner.qp.mouse.is_engaged() {
                    return json!({ "verdict": "Pass", "command": "None" });
                }
                inner.qp.machine.handle_mouse_key(ev)
            } else {
                inner.qp.machine.handle(ev)
            };
            let disarm_shortcut_menu = !mouse_session
                && ev.down
                && inner.qp.machine.requires_shortcut_menu_disarm()
                && !ev.ctrl
                && ev.vk == inner.qp.machine.trigger_vk()
                && matches!(
                    cmd,
                    QuickPasteCommand::BeginSession | QuickPasteCommand::MoveSelection { .. }
                );
            if cmd == QuickPasteCommand::Cancel {
                Self::input_qp_cancel_escape_locked(inner, true);
                return json!({ "verdict": "Swallow", "command": "Cancel" });
            }
            let press_index = inner.qp.machine.trigger_press_index();
            let origin = if cmd == QuickPasteCommand::BeginSession {
                inner.native_fifo_keyboard.retire_binding();
                inner.qp.hook_generation = inner.qp.hook_generation.wrapping_add(1);
                inner.qp.hook_fifo_generation = inner.fifo.as_ref().filter(|ids| !ids.is_empty())
                    .map(|_| inner.fifo_generation);
                inner.qp.hook_source = QuickPasteInputSource::Keyboard;
                Some((
                    crate::window_menu::foreground_hwnd(),
                    crate::window_place::cursor_pos(),
                ))
            } else {
                None
            };
            if cmd == QuickPasteCommand::ArmPasteEnter {
                // Resolve the setting before a release can queue Commit.
                inner.qp.machine.paste_enter_armed = inner.qp_config.enable_paste_enter_shortcut;
            }
            if cmd != QuickPasteCommand::None {
                // WH_KEYBOARD_LL must return before the UI writes the clipboard
                // and injects paste. Never synchronously wait for UI work here.
                let generation = inner.qp.hook_generation;
                let cancel_generation = inner.qp.cancel_generation;
                let source = inner.qp.hook_source;
                inner
                    .qp
                    .pending_commands
                    .push_back(PendingQuickPasteCommand {
                        command: cmd,
                        generation,
                        press_index,
                        origin,
                        source,
                        cancel_generation,
                        disarm_shortcut_menu,
                    });
                // A retained permit also covers work queued while the serial
                // worker is busy. Never execute input inside the native hook.
                inner.keyboard_wakeup.notify_one();
            } else if let Some((fg, anchor)) = origin {
                inner.qp.captured_fg = fg;
                inner.qp.captured_anchor = anchor;
            }
            (verdict, cmd)
        };
        if cmd == QuickPasteCommand::BeginSession {
            // The physical trigger runs on our hook thread. Reclaim mouse hook
            // order once per session, after releasing the host state lock.
            crate::hooks::request_quick_paste_mouse_priority();
        }
        json!({ "verdict": format!("{verdict:?}"), "command": format!("{cmd:?}") })
    }

    pub(super) fn input_qp_drain_escape_locked(inner: &mut HostInner, ev: KeyEvent) -> bool {
        if ev.vk != crate::quick_paste::VK_ESCAPE || !inner.qp.escape_down {
            return false;
        }
        // The accepted cancel owns this physical press through release, even
        // after its overlay has gone or another keyboard layer has appeared.
        if !ev.down { inner.qp.escape_down = false; }
        true
    }

    pub(super) fn input_qp_pending_escape_locked(inner: &HostInner, ev: KeyEvent) -> bool {
        ev.down && ev.vk == crate::quick_paste::VK_ESCAPE
            && !ev.ctrl && !ev.alt && !ev.shift && !ev.win
            && !inner.qp.machine.has_passed_session_key(ev.vk)
            && inner.qp.machine.phase == QuickPastePhase::Idle && inner.qp.overlay_visible
    }

    pub(super) fn input_qp_cancel_escape_locked(inner: &mut HostInner, defer: bool) -> Option<u64> {
        // Before Begin runs only the hook has a queue identity. Once loading
        // starts, its bound FIFO identity wins; never look up a newer queue at
        // the time the cancellation worker eventually executes.
        let fifo_generation = inner.qp.fifo_generation.or(inner.qp.hook_fifo_generation);
        Self::input_qp_end_session_locked(inner, true, None);
        inner.qp.escape_down = true;
        if let Some(generation) = fifo_generation.filter(|_| defer) {
            inner.qp.pending_commands.push_back(PendingQuickPasteCommand {
                command: QuickPasteCommand::CancelFifo { generation },
                generation: inner.qp.hook_generation,
                press_index: 0,
                origin: None,
                source: inner.qp.hook_source,
                cancel_generation: inner.qp.cancel_generation,
                disarm_shortcut_menu: false,
            });
        }
        inner.keyboard_wakeup.notify_one();
        fifo_generation
    }

    fn input_qp_end_session_locked(
        inner: &mut HostInner,
        cancel_pending: bool,
        expected_cancel_generation: Option<u64>,
    ) -> Option<isize> {
        if expected_cancel_generation
            .is_some_and(|generation| generation != inner.qp.cancel_generation)
        {
            return None;
        }
        if cancel_pending {
            inner.qp.captured_target = None;
            inner.qp.cancel_generation = inner.qp.cancel_generation.wrapping_add(1);
            inner.qp.pending_commands.clear();
            inner.qp.mouse.cancel();
            inner.qp.released_mouse_generations.clear();
        }
        let generation = inner.qp.session_generation;
        inner.qp.released_mouse_generations.remove(&generation);
        let fifo_armed = inner.fifo.as_ref().is_some_and(|ids| !ids.is_empty());
        let remember_ok =
            inner.qp_config.remember_selection && inner.qp_config.selection_memory_seconds > 0;
        if fifo_armed {
            // C#: keep prior memory while FIFO is armed.
        } else if remember_ok && !inner.qp.items.is_empty() {
            let idx = inner.qp.active_index as i32;
            if idx >= 0 && (idx as usize) < inner.qp.items.len() {
                // Closing alone never consumes this item. Advance only after
                // the native paste succeeds, leaving cancellation/failure here.
                inner.qp.remembered = Some(QuickPasteSelectionMemory {
                    next_index: idx,
                    at: Instant::now(),
                    session_revision: inner.qp.session_revision,
                });
            } else {
                inner.qp.remembered = None;
            }
        } else if !remember_ok {
            inner.qp.remembered = None;
        }
        // A later chord may already be queued while this session is ending.
        // Do not erase its key state or early-release commit flag.
        if cancel_pending || inner.qp.session_generation == inner.qp.hook_generation {
            let drain = inner.qp.hook_source == QuickPasteInputSource::Keyboard
                && !inner.qp.machine.is_chord_fully_released();
            inner.qp.machine.reset_session();
            inner.qp.machine.set_chord_drain_active(drain);
        }
        inner.qp.captured_fg = 0;
        inner.qp.captured_anchor = None;
        inner.qp.overlay_place = None;
        inner.qp.overlay_visible = false;
        inner.qp.overlay_ready = false;
        inner.qp.overlay_opacity = 0.0;
        inner.qp.awaiting_reveal = false;
        inner.qp.reveal_deadline = None;
        inner.qp.push_burst_left = 0;
        inner.qp.push_burst_next = None;
        inner.qp.armed = None;
        inner.qp.fifo_arm_only_this_session = false;
        inner.qp.fifo_generation = None;
        inner.qp.hook_fifo_generation = None;
        inner.qp.session_started = None;
        inner.qp.items.clear();
        inner.qp.items_json = None;
        inner.qp_wheel_hit.visible = false;
        inner.qp.state_revision += 1;
        push_chrome(inner, ChromeCommand::HideQuickPaste);

        Some(inner.qp_hwnd)
    }
}

/// External selection/navigation remains available without cancelling number
/// paste. Text and actual editing commit this presentation to pass-through.
pub(super) fn external_clipboard_input_ends_digits(ev: KeyEvent) -> bool {
    if ev.win { return false; }
    // AltGr is reported as Ctrl+Alt. A printable press still belongs to the
    // external editor; merely holding either modifier does not end the scope.
    if ev.ctrl && ev.alt {
        return csharp_digit_key_value(ev.vk).is_some() || matches!(ev.vk,
            0x20 | 0x41..=0x5A | 0x6A..=0x6F | 0xBA..=0xC0 | 0xDB..=0xDF
            | 0xE2 | 0xE5 | 0xE7);
    }
    if ev.alt { return false; }
    if ev.ctrl {
        return matches!(ev.vk, 0x08 | 0x0D | 0x20 | 0x2E | 0x46 | 0x56 | 0x58 | 0x59 | 0x5A);
    }
    if ev.shift && csharp_digit_key_value(ev.vk).is_some() { return true; }
    matches!(ev.vk,
        0x08 | 0x0D | 0x20 | 0x2E | 0x41..=0x5A | 0x6A..=0x6F
        | 0x15 | 0x1C | 0x1D | 0x1F | 0xBA..=0xC0 | 0xDB..=0xDF
        | 0xE2 | 0xE5 | 0xE7)
}

// HWND/PID/owner queries only: no WM_GETTEXT, process handles, Toolhelp snapshots,
// expression evaluation, activation, clipboard access or synchronous window send.
fn input_hit(host: isize, x: i32, y: i32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{Foundation::POINT, UI::WindowsAndMessaging::*};
        if host == 0 {
            return false;
        }
        let hit = WindowFromPoint(POINT { x, y });
        input_related_with_companions(host, hit.0 as isize, false)
    }
    #[cfg(not(windows))]
    {
        let _ = (host, x, y);
        false
    }
}

pub(super) fn input_related(host: isize, other: isize) -> bool {
    input_related_with_companions(host, other, true)
}

fn input_related_with_companions(host: isize, other: isize, companions: bool) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
        if host == 0 || other == 0 {
            return false;
        }
        if host == other {
            return true;
        }
        let own = HWND(host as *mut _);
        let mut current = HWND(other as *mut _);
        if GetAncestor(current, GA_ROOT) == own || IsChild(own, current).as_bool() {
            return true;
        }
        let mut process = 0;
        GetWindowThreadProcessId(current, Some(&mut process));
        if companions && process == std::process::id() {
            return true;
        }
        for _ in 0..16 {
            let Ok(owner) = GetWindow(current, GW_OWNER) else {
                break;
            };
            if owner.0.is_null() || owner == current {
                break;
            }
            if owner == own {
                return true;
            }
            current = owner;
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = (host, other, companions);
        false
    }
}

/// An opening or open keyboard session survives an input gap only while its
/// chord is still physically held. Mouse chords keep their own recovery.
fn qp_session_survives_input_gap(
    phase: QuickPastePhase,
    keyboard_chord: bool,
    mouse_held: bool,
    modifiers_physically_down: impl FnOnce() -> bool,
) -> bool {
    matches!(phase, QuickPastePhase::SessionStarting | QuickPastePhase::SessionActive)
        && keyboard_chord
        && !mouse_held
        && modifiers_physically_down()
}

#[cfg(test)]
mod input_gap_tests {
    use super::*;

    #[test]
    fn held_keyboard_chord_survives_gap_while_opening_or_open() {
        let survives = |phase, keyboard, mouse, held: bool| {
            qp_session_survives_input_gap(phase, keyboard, mouse, || held)
        };
        for phase in [QuickPastePhase::SessionStarting, QuickPastePhase::SessionActive] {
            assert!(survives(phase, true, false, true));
            // A missed Alt release must never become a kept (later committed) session.
            assert!(!survives(phase, true, false, false));
            assert!(!survives(phase, false, false, true));
            assert!(!survives(phase, true, true, true));
        }
        for phase in [QuickPastePhase::Idle, QuickPastePhase::ChordEngaged] {
            assert!(!survives(phase, true, false, true));
        }
    }

    #[test]
    fn modifiers_are_not_read_unless_the_session_could_survive() {
        let read = std::cell::Cell::new(false);
        assert!(!qp_session_survives_input_gap(QuickPastePhase::Idle, true, false, || {
            read.set(true);
            true
        }));
        assert!(!read.get());
    }
}
