//! Input accepted by a native panel or Super Menu keeps that surface's own
//! lifetime. Paste follows the current external receiver without borrowing the
//! clipboard window's ticket.

use super::*;

const PANEL_INPUT_RETIRED: &str = "面板输入会话已失效，已停止粘贴。";
const SUPER_MENU_INPUT_RETIRED: &str = "Menu session has ended.";

/// Menu nodes can carry paths without a record token. Resolve them through the
/// selected core repository before passing any filename to clipboard/OS code.
fn native_menu_output_files(store:&AppStore,paths:&[String])->Result<Vec<String>,String>{
    if !store.is_core_store(){return Ok(paths.to_vec());}
    let mut record=ClipRecord::new_text("","native-menu-output");
    record.kind=ClipKind::FileClipItem;record.file_paths=paths.to_vec();
    Ok(store.core_io_clip(&record).map_err(|e|e.to_string())?.file_paths)
}

#[derive(Debug, Clone)]
pub struct NativePanelPasteRequest {
    owner: usize,
    source_hwnd: isize,
    target: Option<InputTarget>,
    input_epoch: ChromeInputEpoch,
    folder_id: Option<String>,
    no_focus: bool,
    fake_focus: bool,
    keyboard_focus_host: Option<&'static str>,
    task: Option<ChromeTask>,
}

#[derive(Debug, Clone)]
pub struct NativeSuperMenuPasteRequest {
    owner: usize,
    source_hwnd: isize,
    registry_target: isize,
    target: Option<InputTarget>,
    session_id: String,
    generation: i32,
    keyboard: Option<NativeSuperMenuKeyIdentity>,
}

#[derive(Debug, Clone)]
struct NativeSuperMenuKeyIdentity {
    epoch: u64,
    foreground: isize,
    scope: NativeSuperMenuKeyScope,
    incoming_drag_generation: u64,
}

/// The same accepted menu ticket also owns non-input actions such as an
/// opacity slider. Its optional target must not become a receiver requirement.
pub type NativeSuperMenuRequest = NativeSuperMenuPasteRequest;

/// Only key behaviors present in the WebView product are claimed. A Value
/// scope names its row so switching sliders cannot commit another row's keyup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSuperMenuKeyScope {
    Dismiss,
    WindowHub,
    ClipboardHub,
    Value(String),
}

impl NativeSuperMenuKeyScope {
    fn key(&self, vk: u32) -> Option<&'static str> {
        match (self, vk) {
            (_, 0x1B) => Some("Escape"),
            (Self::WindowHub | Self::Value(_), 0x25) => Some("ArrowLeft"),
            (Self::WindowHub | Self::Value(_), 0x27) => Some("ArrowRight"),
            (Self::ClipboardHub, 0x26) => Some("ArrowUp"),
            (Self::ClipboardHub, 0x28) => Some("ArrowDown"),
            (Self::WindowHub | Self::ClipboardHub, 0x0D) => Some("Enter"),
            (Self::Value(_), 0x24) => Some("Home"),
            (Self::Value(_), 0x23) => Some("End"),
            _ => None,
        }
    }
}

/// Constructed only by the low-level hook after accepting the native binding.
/// Consumers carry this event intact; they must never recapture a newer menu.
#[derive(Debug, Clone)]
pub struct NativeSuperMenuKey {
    request: NativeSuperMenuRequest,
    scope: NativeSuperMenuKeyScope,
    epoch: u64,
    foreground: isize,
    key: &'static str,
    down: bool,
    repeat: bool,
}

impl NativeSuperMenuKey {
    pub fn request(&self) -> &NativeSuperMenuRequest {
        &self.request
    }
    pub fn scope(&self) -> &NativeSuperMenuKeyScope {
        &self.scope
    }
    pub fn key(&self) -> &'static str {
        self.key
    }
    pub fn down(&self) -> bool {
        self.down
    }
    pub fn repeat(&self) -> bool {
        self.repeat
    }
}

#[derive(Clone)]
struct NativeSuperMenuKeyBinding {
    request: NativeSuperMenuRequest,
    scope: NativeSuperMenuKeyScope,
    foreground: isize,
    raw_foreground_epoch: u64,
}

struct NativeSuperMenuRestoreResume {
    epoch: u64,
    foreground: isize,
    raw_foreground_epoch: u64,
    incoming_drag_generation: u64,
}

struct NativeSuperMenuDismissedRestore {
    request: NativeSuperMenuRequest,
    hidden_generation: i32,
    raw_foreground_epoch: u64,
}

#[derive(Default)]
pub(super) struct NativeSuperMenuKeyboard {
    // Keep the opt-in presentation after foreground retirement. It must not
    // fall back to WebView Esc or clipboard fake-focus keys while still shown.
    presentation: Option<NativeSuperMenuRequest>,
    binding: Option<NativeSuperMenuKeyBinding>,
    epoch: u64,
    inbox: VecDeque<NativeSuperMenuKey>,
    held: HashMap<u32, NativeSuperMenuKey>,
    incoming_drag_generation: u64,
    restore_resume: Option<NativeSuperMenuRestoreResume>,
    focused_input: bool,
    dismissed_restore: Option<NativeSuperMenuDismissedRestore>,
}

impl NativeSuperMenuKeyboard {
    pub(super) fn reset_after_input_gap(&mut self) {
        self.retire_binding();
        self.held.clear();
    }
    pub(super) fn retire_binding(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.binding = None;
        self.inbox.clear();
        self.restore_resume = None;
        self.focused_input = false;
        self.dismissed_restore = None;
        // Held accepted presses survive retirement strictly as release drains.
    }

    pub(super) fn retire_presentation(&mut self) {
        self.retire_binding();
        self.presentation = None;
    }

    /// Hiding increments the core generation and destroys the registry. Keep
    /// one exact handoff authorization only for an explicitly focused native
    /// presentation; ordinary no-focus categories retain their old contract.
    pub(super) fn retire_dismissed_presentation(
        &mut self, generation: i32, session_id: Option<&str>, source_hwnd: isize,
    ) {
        let dismissed = self.presentation.as_ref().filter(|request| {
            self.focused_input && self.binding.is_none() && request.keyboard.is_none()
                && request.generation == generation
                && Some(request.session_id.as_str()) == session_id
                && request.source_hwnd != 0 && request.source_hwnd == source_hwnd
                && request.target.is_some()
        }).cloned();
        self.retire_presentation();
        self.dismissed_restore = dismissed.map(|request| NativeSuperMenuDismissedRestore {
            request,
            hidden_generation: generation.wrapping_add(1),
            raw_foreground_epoch: crate::active_window::raw_foreground_epoch(),
        });
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

    fn observe_incoming_drag(&mut self, generation: u64) {
        if self.binding.is_some() && self.incoming_drag_generation != generation {
            self.retire_binding();
        }
    }

    pub(super) fn owns_presentation(&self, inner: &HostInner) -> bool {
        self.presentation
            .as_ref()
            .is_some_and(|request| Self::presentation_matches(inner, request))
    }

    fn presentation_matches(inner: &HostInner, request: &NativeSuperMenuRequest) -> bool {
        request.source_hwnd != 0
            && request.source_hwnd == inner.super_menu_hwnd
            && inner.super_menu.visible
            && inner.super_menu.generation == request.generation
            && inner.super_menu.session_id.as_deref() == Some(request.session_id.as_str())
    }

    fn current(&self, key: &NativeSuperMenuKey, foreground: isize) -> bool {
        key.epoch == self.epoch
            && key.foreground == foreground
            && self.binding.as_ref().is_some_and(|binding| {
                binding.foreground == foreground
                    && binding.raw_foreground_epoch == crate::active_window::raw_foreground_epoch()
                    && binding.scope == key.scope
                    && binding.request.session_id == key.request.session_id
                    && binding.request.generation == key.request.generation
                    && binding.request.source_hwnd == key.request.source_hwnd
            })
    }

    fn enqueue(&mut self, key: NativeSuperMenuKey) {
        // Preserve every navigation/slider repeat and its paired release.
        // If the owning UI stops draining, retire rather than retaining only
        // a terminal up that could commit without its original local downs.
        if self.inbox.len() >= 64 {
            self.retire_binding();
        } else {
            self.inbox.push_back(key);
        }
    }

    fn route_held(
        &mut self,
        ev: KeyEvent,
        foreground: isize,
        dispatch_allowed: bool,
        repeat: bool,
    ) -> bool {
        // A fresh press must never inherit an old drain (e.g. a release was
        // observed by another priority route). Only physical repeats/releases
        // belong to the stored press.
        if ev.down && !repeat {
            self.held.remove(&ev.vk);
            return false;
        }
        let Some(mut key) = self.held.get(&ev.vk).cloned() else {
            return false;
        };
        if !ev.down {
            self.held.remove(&ev.vk);
        }
        if dispatch_allowed
            && self.current(&key, foreground)
            && (!ev.down || !(ev.ctrl || ev.alt || ev.shift || ev.win))
        {
            key.down = ev.down;
            key.repeat = ev.down;
            // Execution/cancel is once per physical press. Navigation and
            // slider repeats remain local UI work; only keyup commits a value.
            if !ev.down || !matches!(key.key, "Enter" | "Escape") {
                self.enqueue(key);
            }
        }
        true
    }

    fn accept_down(&mut self, ev: KeyEvent, repeat: bool) -> bool {
        if !ev.down || repeat || ev.ctrl || ev.alt || ev.shift || ev.win {
            return false;
        }
        let Some(binding) = self.binding.as_ref() else {
            return false;
        };
        let Some(key) = binding.scope.key(ev.vk) else {
            return false;
        };
        let mut request = binding.request.clone();
        request.keyboard = Some(NativeSuperMenuKeyIdentity {
            epoch: self.epoch,
            foreground: binding.foreground,
            scope: binding.scope.clone(),
            incoming_drag_generation: self.incoming_drag_generation,
        });
        let event = NativeSuperMenuKey {
            request,
            scope: binding.scope.clone(),
            epoch: self.epoch,
            foreground: binding.foreground,
            key,
            down: true,
            repeat: false,
        };
        self.held.insert(ev.vk, event.clone());
        self.enqueue(event);
        true
    }
}

impl HostState {
    pub(super) fn native_super_menu_foreground(&self) -> isize {
        if crate::clipboard::is_headless() {
            self.inner.lock().last_fg
        } else {
            crate::window_menu::foreground_hwnd()
        }
    }

    /// Explicit GPUI opt-in. set_super_menu_hwnd alone never takes over the
    /// Tauri WebView route. Call on the owning UI thread, outside the hook.
    pub fn bind_native_super_menu_keyboard(
        &self,
        request: &NativeSuperMenuRequest,
        scope: NativeSuperMenuKeyScope,
    ) -> Result<(), String> {
        self.bind_native_super_menu_keyboard_for_foreground(
            request,
            scope,
            self.native_super_menu_foreground(),
        )
    }

    /// A focused text search owns its IME and Escape locally. Keep this native
    /// presentation opted in, but retire the global key binding and its queue.
    pub fn use_focused_native_super_menu_input(
        &self,
        request: &NativeSuperMenuRequest,
    ) -> Result<(), String> {
        self.verify_super_menu_lifecycle(request)?;
        let mut inner = self.inner.lock();
        Self::verify_super_menu_presentation(&inner, request, request.registry_target as i64)?;
        let keyboard = &mut inner.super_menu.native_keyboard;
        if keyboard.presentation.as_ref().is_some_and(|old| {
            old.session_id == request.session_id && old.generation == request.generation
                && old.source_hwnd == request.source_hwnd
        }) && keyboard.binding.is_none() {
            keyboard.focused_input = true;
            return Ok(());
        }
        keyboard.retire_presentation();
        keyboard.presentation = Some(request.clone());
        keyboard.focused_input = true;
        Ok(())
    }

    fn verify_dismissed_native_super_menu_target(
        &self,
        request: &NativeSuperMenuPasteRequest,
        consume: bool,
    ) -> Result<InputTarget, String> {
        if request.keyboard.is_some() || request.owner != Arc::as_ptr(&self.inner) as usize {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        let target = request.target.ok_or(SUPER_MENU_INPUT_RETIRED)?;
        target.verify_identity()?;
        let foreground = self.native_super_menu_foreground();
        let mut inner = self.inner.lock();
        let valid = !inner.super_menu.visible && !inner.super_menu.loading
            && inner.super_menu.session_id.is_none()
            && request.source_hwnd != 0 && foreground == request.source_hwnd
            && inner.super_menu_hwnd == request.source_hwnd
            && inner.super_menu.native_keyboard.dismissed_restore.as_ref().is_some_and(|dismissed| {
                inner.super_menu.generation == dismissed.hidden_generation
                    && dismissed.raw_foreground_epoch == crate::active_window::raw_foreground_epoch()
                    && dismissed.request.owner == request.owner
                    && dismissed.request.source_hwnd == request.source_hwnd
                    && dismissed.request.registry_target == request.registry_target
                    && dismissed.request.target == request.target
                    && dismissed.request.session_id == request.session_id
                    && dismissed.request.generation == request.generation
            });
        if !valid {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        if consume {
            inner.super_menu.native_keyboard.dismissed_restore = None;
        }
        Ok(target)
    }

    /// Complete only the just-dismissed focused surface's foreground handoff.
    /// This cannot revive a session, retarget a newer menu, or grab foreground
    /// from an external application selected while the hide was pending.
    pub fn restore_dismissed_native_super_menu_target(
        &self,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<(), String> {
        self.verify_dismissed_native_super_menu_target(request, false)?;
        let host = self.clone();
        let request = request.clone();
        self.run_input(move || {
            let target = host.verify_dismissed_native_super_menu_target(&request, true)?;
            target.activate()
        })
    }

    /// Hand foreground back to the accepted original receiver without copying
    /// or pasting. A third-party foreground change cancels this fixed-window
    /// handoff instead of activating a stale target over the user's choice.
    pub fn restore_native_super_menu_target(
        &self,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<(), String> {
        // A presentation request is required. A taken keyboard event cannot
        // shed its epoch identity in order to resurrect its former binding.
        if request.keyboard.is_some() {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        self.use_focused_native_super_menu_input(request)?;
        let target = self.activate_native_menu_target(request)?;
        self.verify_super_menu_lifecycle(request)?;
        target.verify()?;
        let foreground = self.native_super_menu_foreground();
        let mut inner = self.inner.lock();
        Self::verify_super_menu_presentation(&inner, request, request.registry_target as i64)?;
        if foreground != target.hwnd() {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        let incoming_drag_generation = inner.native_incoming_drag_next_id;
        let keyboard = &mut inner.super_menu.native_keyboard;
        if !keyboard.presentation.as_ref().is_some_and(|presentation| {
            presentation.session_id == request.session_id
                && presentation.generation == request.generation
                && presentation.source_hwnd == request.source_hwnd
        }) || keyboard.binding.is_some() {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        keyboard.restore_resume = Some(NativeSuperMenuRestoreResume {
            epoch: keyboard.epoch,
            foreground,
            raw_foreground_epoch: crate::active_window::raw_foreground_epoch(),
            incoming_drag_generation,
        });
        Ok(())
    }

    /// Explicitly resume no-focus keys after a successful fixed-target
    /// handoff. Consume its one-use foreground/epoch fence before binding.
    pub fn resume_native_super_menu_keyboard(
        &self,
        request: &NativeSuperMenuRequest,
        scope: NativeSuperMenuKeyScope,
    ) -> Result<(), String> {
        if request.keyboard.is_some() {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        self.verify_super_menu_lifecycle(request)?;
        let foreground = self.native_super_menu_foreground();
        let mut inner = self.inner.lock();
        Self::verify_super_menu_presentation(&inner, request, request.registry_target as i64)?;
        let incoming_drag_generation = inner.native_incoming_drag_next_id;
        if inner.ole_drag_active || inner.native_incoming_drag.is_some()
            || inner.native_context_menu.is_some() || inner.qp.overlay_visible {
            inner.super_menu.native_keyboard.restore_resume = None;
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        let keyboard = &mut inner.super_menu.native_keyboard;
        let restored = keyboard.restore_resume.take().ok_or(SUPER_MENU_INPUT_RETIRED)?;
        if restored.epoch != keyboard.epoch || keyboard.binding.is_some()
            || restored.foreground != foreground
            || restored.raw_foreground_epoch != crate::active_window::raw_foreground_epoch()
            || restored.incoming_drag_generation != incoming_drag_generation
            || !keyboard.presentation.as_ref().is_some_and(|presentation| {
                presentation.session_id == request.session_id
                    && presentation.generation == request.generation
                    && presentation.source_hwnd == request.source_hwnd
            }) {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        // Old accepted keys remain release drains, with their old epoch. No
        // queued/taken event can become current in this new binding.
        keyboard.retire_binding();
        keyboard.incoming_drag_generation = incoming_drag_generation;
        keyboard.binding = Some(NativeSuperMenuKeyBinding {
            request: request.clone(), scope, foreground,
            raw_foreground_epoch: crate::active_window::raw_foreground_epoch(),
        });
        Ok(())
    }

    fn bind_native_super_menu_keyboard_for_foreground(
        &self,
        request: &NativeSuperMenuRequest,
        scope: NativeSuperMenuKeyScope,
        foreground: isize,
    ) -> Result<(), String> {
        self.verify_super_menu_lifecycle(request)?;
        // No receiver is required for Escape/copy/opacity. When there is a
        // registry target, do not take keys from an unrelated application.
        if foreground == 0
            || (request.registry_target != 0
                && foreground != request.registry_target
                && foreground != request.source_hwnd)
        {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        let mut inner = self.inner.lock();
        Self::verify_super_menu_presentation(&inner, request, request.registry_target as i64)?;
        if inner.ole_drag_active
            || inner.native_incoming_drag.is_some()
            || inner.native_context_menu.is_some()
            || inner.qp.overlay_visible
        {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        let incoming_drag_generation = inner.native_incoming_drag_next_id;
        let keyboard = &mut inner.super_menu.native_keyboard;
        keyboard.observe_incoming_drag(incoming_drag_generation);
        if let Some(presentation) = &keyboard.presentation {
            if presentation.session_id != request.session_id
                || presentation.generation != request.generation
                || presentation.source_hwnd != request.source_hwnd
            {
                keyboard.retire_presentation();
            } else {
                keyboard.observe_foreground(foreground);
                // Returning foreground or a delayed scope update cannot revive
                // an input binding that already lost its original foreground.
                let Some(binding) = &keyboard.binding else {
                    return Err(SUPER_MENU_INPUT_RETIRED.into());
                };
                if binding.scope == scope {
                    return Ok(());
                }
                keyboard.retire_binding();
            }
        }
        keyboard.presentation = Some(request.clone());
        keyboard.incoming_drag_generation = incoming_drag_generation;
        keyboard.binding = Some(NativeSuperMenuKeyBinding {
            request: request.clone(),
            scope,
            foreground,
            raw_foreground_epoch: crate::active_window::raw_foreground_epoch(),
        });
        Ok(())
    }

    /// Poll from the GPUI event loop. This validates registry/foreground off
    /// the low-level hook; queued work from old roots/scopes is dropped.
    pub fn take_native_super_menu_key(&self) -> Option<NativeSuperMenuKey> {
        self.take_native_super_menu_key_for_foreground(self.native_super_menu_foreground())
    }

    fn take_native_super_menu_key_for_foreground(
        &self,
        foreground: isize,
    ) -> Option<NativeSuperMenuKey> {
        loop {
            let event = {
                let mut inner = self.inner.lock();
                inner
                    .super_menu
                    .native_keyboard
                    .observe_foreground(foreground);
                inner.super_menu.native_keyboard.inbox.pop_front()?
            };
            if self.native_super_menu_key_is_current_for_foreground(&event, foreground) {
                return Some(event);
            }
        }
    }

    /// Recheck immediately before UI execution, carrying the hook's intact
    /// private ticket rather than capturing the menu currently being shown.
    pub fn native_super_menu_key_is_current(&self, event: &NativeSuperMenuKey) -> bool {
        self.native_super_menu_key_is_current_for_foreground(
            event,
            self.native_super_menu_foreground(),
        )
    }

    fn native_super_menu_key_is_current_for_foreground(
        &self,
        event: &NativeSuperMenuKey,
        foreground: isize,
    ) -> bool {
        if self.input_gap_pending() { return false; }
        let lifecycle = self.verify_super_menu_lifecycle(&event.request).is_ok();
        let mut inner = self.inner.lock();
        if self.input_gap_pending() { return false; }
        let permitted = inner.window_runtime_ready
            && !inner.ole_drag_active
            && inner.native_incoming_drag.is_none()
            && inner.native_context_menu.is_none()
            && !inner.qp.overlay_visible;
        let keyboard = &mut inner.super_menu.native_keyboard;
        keyboard.observe_foreground(foreground);
        if (!lifecycle || !permitted) && event.epoch == keyboard.epoch {
            keyboard.retire_binding();
        }
        lifecycle && permitted && keyboard.current(event, foreground)
    }

    // These two hook helpers use only cached state, a small owned queue and the
    // passed foreground. No menu registry, database, clipboard, COM or activation.



}

impl HostState {
    /// No history reads, clipboard writes, activation or input on the UI thread.
    pub fn capture_native_panel_paste(
        &self,
        task: Option<ChromeTask>,
    ) -> Result<NativePanelPasteRequest, String> {
        self.capture_native_panel_request(task, true)
    }

    /// Copy/navigation/open actions retain panel ownership even when there is
    /// no external input destination. A usable destination is captured when
    /// available; a later text/Paste branch can follow a new external receiver.
    pub fn capture_native_panel_action(
        &self,
        task: Option<ChromeTask>,
    ) -> Result<NativePanelPasteRequest, String> {
        self.capture_native_panel_request(task, false)
    }

    fn capture_native_panel_request(
        &self,
        task: Option<ChromeTask>,
        require_target: bool,
    ) -> Result<NativePanelPasteRequest, String> {
        if task.is_some_and(|task| {
            task.command.input_window() != Some("panel") || !self.chrome_task_input_allowed(task)
        }) {
            return Err(PANEL_INPUT_RETIRED.into());
        }
        let (
            source_hwnd,
            input_epoch,
            folder_id,
            no_focus,
            fake_focus,
            keyboard_focus_host,
            remembered_target,
        ) = {
            let inner = self.inner.lock();
            (
                inner.panel_hwnd,
                inner.chrome_input_epochs[1],
                inner.ui.panel_folder_id.clone(),
                inner.ui.panel_no_focus_session,
                inner.ui.panel_fake_focus,
                inner.ui.keyboard_focus_host,
                inner.last_non_host_fg,
            )
        };
        let foreground = native_paste_foreground();
        let target_hwnd = if crate::clipboard::is_headless() {
            remembered_target
        } else if foreground != 0 && !foreground_is_own_window(&self.inner.lock(), foreground) {
            foreground
        } else {
            self.last_window_except(source_hwnd)
        };
        let target = if target_hwnd == 0 {
            Err("找不到可接收输入的目标窗口。".to_string())
        } else {
            InputTarget::capture(target_hwnd)
        };
        let target = if require_target {
            Some(target?)
        } else {
            target.ok()
        };
        let request = NativePanelPasteRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            source_hwnd,
            target,
            input_epoch,
            folder_id,
            no_focus,
            fake_focus,
            keyboard_focus_host,
            task,
        };
        self.verify_native_panel_lifecycle(&request)?;
        if require_target {
            self.verify_native_panel_target(&request, foreground)?;
        }
        Ok(request)
    }

    fn verify_native_panel_request(
        &self,
        request: &NativePanelPasteRequest,
        foreground: isize,
    ) -> Result<(), String> {
        self.verify_native_panel_lifecycle(request)?;
        self.verify_native_panel_target(request, foreground)
    }

    fn verify_native_panel_lifecycle(
        &self,
        request: &NativePanelPasteRequest,
    ) -> Result<(), String> {
        if request.owner != Arc::as_ptr(&self.inner) as usize
            || request
                .task
                .is_some_and(|task| !self.chrome_task_input_allowed(task))
        {
            return Err(PANEL_INPUT_RETIRED.into());
        }
        let inner = self.inner.lock();
        let epoch = inner.chrome_input_epochs[1];
        if request.source_hwnd == 0
            || request.source_hwnd != inner.panel_hwnd
            || request.input_epoch.window != epoch.window
            || request.input_epoch.list != epoch.list
            || request.folder_id != inner.ui.panel_folder_id
            || !inner.window_runtime_ready
            || !inner.panel_web_view_ready
            || !inner.ui.panel_visible
            || inner.ui.panel_spa_overlay_open
            || native_context_menu_for(&inner, "panel")
            || request.no_focus != inner.ui.panel_no_focus_session
            || request.fake_focus != inner.ui.panel_fake_focus
            || request.keyboard_focus_host != inner.ui.keyboard_focus_host
            || (inner.window_visible && inner.has_host_fake_focus && !inner.ui.panel_fake_focus)
            || inner.qp.overlay_visible
            || inner.super_menu.visible
        {
            return Err(PANEL_INPUT_RETIRED.into());
        }
        Ok(())
    }

    fn verify_native_panel_target(
        &self,
        request: &NativePanelPasteRequest,
        _foreground: isize,
    ) -> Result<(), String> {
        request.target.ok_or("找不到可接收输入的目标窗口。")?;
        Ok(())
    }

    pub fn favor_copy_panel_native(
        &self,
        params: &Value,
        request: &NativePanelPasteRequest,
    ) -> Result<Value, String> {
        self.verify_native_panel_lifecycle(request)?;
        let id = req_id(params)?;
        let (store, opts) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                paste_options_from_config(&inner.config),
            )
        };
        let item = store.lock().get_favor(&id).map_err(item_not_found)?;
        let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
        self.verify_native_panel_lifecycle(request)?;
        self.copy_panel_record_native(clip, opts, request)?;
        Ok(json!({ "ok": true }))
    }

    pub fn favor_paste_panel_native(
        &self,
        params: &Value,
        request: &NativePanelPasteRequest,
    ) -> Result<Value, String> {
        self.favor_paste_panel_native_for_foreground(params, request, native_paste_foreground)
    }

    fn favor_paste_panel_native_for_foreground(
        &self,
        params: &Value,
        request: &NativePanelPasteRequest,
        mut foreground: impl FnMut() -> isize,
    ) -> Result<Value, String> {
        self.verify_native_panel_request(request, foreground())?;
        let id = req_id(params)?;
        let (store, opts) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                paste_options_from_config(&inner.config),
            )
        };
        let item = store.lock().get_favor(&id).map_err(item_not_found)?;
        let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
        self.verify_native_panel_request(request, foreground())?;
        self.paste_panel_record_native(clip, opts, request)?;
        // favor.paste itself does not update usage; panel Open does below.
        Ok(json!({ "ok": true }))
    }

    fn panel_request_guard(
        &self,
        request: &NativePanelPasteRequest,
    ) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let host = self.clone();
        let request = request.clone();
        Arc::new(move || host.verify_native_panel_request(&request, native_paste_foreground()))
    }

    fn panel_action_guard(
        &self,
        request: &NativePanelPasteRequest,
    ) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let host = self.clone();
        let request = request.clone();
        Arc::new(move || host.verify_native_panel_lifecycle(&request))
    }

    fn copy_panel_record_native(
        &self,
        clip: ClipRecord,
        opts: PasteOptions,
        request: &NativePanelPasteRequest,
    ) -> Result<(), String> {
        let clip=self.store().lock().core_io_clip(&clip).map_err(|e|e.to_string())?;
        let guard = self.panel_action_guard(request);
        let prepare_guard = guard.clone();
        self.run_clipboard_write_then_guarded(
            move || {
                prepare_guard()?;
                crate::paste::prepare_copy_record(&clip, &opts)
            },
            |_| Ok(()),
            None,
            guard,
        )
    }

    fn paste_panel_record_native(
        &self,
        clip: ClipRecord,
        opts: PasteOptions,
        request: &NativePanelPasteRequest,
    ) -> Result<(), String> {
        self.paste_surface_record_native(
            clip,
            opts,
            request.target.ok_or("找不到可接收输入的目标窗口。")?,
            self.panel_request_guard(request),
        )
    }

    /// The original panel folder/file click rules, including usage only for
    /// successful Open. Default text Open uses the current paste receiver.
    pub fn panel_invoke_native(
        &self,
        params: &Value,
        request: &NativePanelPasteRequest,
    ) -> Result<Value, String> {
        self.panel_invoke_native_for_foreground(params, request, native_paste_foreground)
    }

    fn panel_invoke_native_for_foreground(
        &self,
        params: &Value,
        request: &NativePanelPasteRequest,
        mut foreground: impl FnMut() -> isize,
    ) -> Result<Value, String> {
        let _accepted_foreground = foreground();
        self.verify_native_panel_lifecycle(request)?;
        let _favor_id = req_str(params, "favorId")?;
        let id = req_str(params, "itemId")?;
        let (store, opts, folder_click, file_click) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                paste_options_from_config(&inner.config),
                inner.extras.panel.folder_default_click_action.clone(),
                inner.extras.panel.file_default_click_action.clone(),
            )
        };
        let item = store.lock().get_favor(&id).map_err(panel_item_not_found)?;
        let item=store.lock().core_io_favor(&item).map_err(|e|e.to_string())?;
        let current_foreground = foreground();
        self.verify_native_panel_lifecycle(request)?;
        let click = if item.is_folder {
            &folder_click
        } else {
            &file_click
        };
        if panel_click_is_copy(click) {
            let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
            self.copy_panel_record_native(clip, opts, request)?;
        } else if item.is_folder {
            return Ok(json!({ "action": "navigate", "navigateFolderId": item.id }));
        } else if panel_click_is_paste(click) {
            self.verify_native_panel_target(request, current_foreground)?;
            let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
            self.paste_panel_record_native(clip, opts, request)?;
        } else if item.kind == ClipKind::FileClipItem {
            // PanelContent first handles this through openShellPath(paths[0]);
            // it never visits the host's dialog/multi-file executor or usage.
            let first = item
                .file_paths
                .iter()
                .map(String::as_str)
                .map(str::trim)
                .find(|s| !s.is_empty())
                .or_else(|| {
                    item.body_text
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
                .ok_or("没有可打开的文件。")?;
            self.verify_native_panel_lifecycle(request)?;
            self.open_shell_target(first)?;
        } else if item.kind == ClipKind::LinkClipItem {
            self.verify_native_panel_lifecycle(request)?;
            self.open_shell_target(item.body_text.as_deref().unwrap_or("").trim())?;
        } else if item.kind == ClipKind::FavoriteClipItem {
            self.panel_show_folder(&id);
        } else if matches!(item.kind, ClipKind::ActionClipItem | ClipKind::StepClipItem) {
            // shouldRunPanelFavoriteViaPaste routes these payloads through
            // pasteFavorite even for default Open; no action RPC or usage.
            self.verify_native_panel_target(request, current_foreground)?;
            let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
            self.paste_panel_record_native(clip, opts, request)?;
        } else {
            self.verify_native_panel_target(request, current_foreground)?;
            let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
            self.paste_panel_record_native(clip, opts, request)?;
            self.touch_favor_usage(&id);
        }
        Ok(json!({ "action": "executed", "navigateFolderId": Value::Null }))
    }

    pub fn panel_native_snapshot(&self) -> Value {
        let inner = self.inner.lock();
        json!({
            "visible": inner.ui.panel_visible,
            "noFocus": inner.ui.panel_no_focus_session,
            "fakeFocus": inner.ui.panel_fake_focus,
            "hwnd": inner.panel_hwnd,
            "folderId": inner.ui.panel_folder_id,
            // Display ownership survives a deliberate keyboard-focus handoff;
            // accepted input separately retains its window/list epochs.
            "presentationGeneration": inner.chrome_presentation_epochs[1],
            "windowInputGeneration": inner.chrome_input_epochs[1].window,
            "inputGeneration": inner.chrome_input_epochs[1].list,
        })
    }

    pub fn apply_native_panel_page(
        &self,
        presentation_generation: u64,
        source_hwnd: isize,
        can_go_back: bool,
        current_folder_id: &str,
    ) -> bool {
        let mut inner = self.inner.lock();
        if !inner.ui.panel_visible
            || source_hwnd == 0
            || inner.panel_hwnd != source_hwnd
            || !inner.window_runtime_ready
            || !inner.panel_web_view_ready
            || inner.chrome_presentation_epochs[1] != presentation_generation
        {
            return false;
        }
        let folder = current_folder_id.trim();
        if !folder.is_empty() && folder != crate::panel::DETACHED_ORPHANS_FOLDER_ID {
            if inner.ui.panel_folder_id.as_deref() != Some(folder) {
                drop_queued_panel_navigation(&mut inner);
            }
            inner.ui.panel_folder_id = Some(folder.to_string());
        }
        inner.ui.panel_can_go_back = can_go_back;
        true
    }

    pub fn capture_super_menu_paste(
        &self,
        session_id: &str,
        generation: i32,
        source_hwnd: isize,
    ) -> Result<NativeSuperMenuPasteRequest, String> {
        self.capture_super_menu_request(session_id, generation, source_hwnd, true)
    }

    pub fn capture_native_super_menu_action(
        &self,
        session_id: &str,
        generation: i32,
        source_hwnd: isize,
    ) -> Result<NativeSuperMenuPasteRequest, String> {
        self.capture_super_menu_request(session_id, generation, source_hwnd, false)
    }

    /// Async UI replies belong to this captured menu, never whichever menu is
    /// currently global. Observation does not require a paste receiver or the
    /// foreground window, so a successful slider/copy reply can remain current.
    /// Existing menu hide closes the registry and always retires this ticket.
    pub fn native_super_menu_request_is_current(&self, request: &NativeSuperMenuRequest) -> bool {
        self.verify_super_menu_lifecycle(request).is_ok()
    }

    /// Successful activation may itself leave the keyboard foreground. Its
    /// close reply may retire only the original presentation, without granting
    /// any further input/action permission or borrowing a replacement session.
    pub fn native_super_menu_close_reply_is_current(
        &self,
        request: &NativeSuperMenuRequest,
    ) -> bool {
        let mut presentation = request.clone();
        presentation.keyboard = None;
        self.verify_super_menu_lifecycle(&presentation).is_ok()
    }

    fn capture_super_menu_request(
        &self,
        session_id: &str,
        generation: i32,
        source_hwnd: isize,
        require_target: bool,
    ) -> Result<NativeSuperMenuPasteRequest, String> {
        // with_session distinguishes a closed registry from an empty target.
        let target_hwnd = self.menus.with_session(session_id, |registry| {
            Ok(registry.session_target_handle(session_id))
        })? as isize;
        let target = if target_hwnd == 0 {
            Err("找不到可接收输入的目标窗口。".to_string())
        } else {
            InputTarget::capture(target_hwnd)
        };
        let target = if require_target {
            Some(target?)
        } else {
            target.ok()
        };
        let request = NativeSuperMenuPasteRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            source_hwnd,
            registry_target: target_hwnd,
            target,
            session_id: session_id.to_string(),
            generation,
            keyboard: None,
        };
        self.verify_super_menu_lifecycle(&request)?;
        if require_target {
            self.verify_super_menu_target(&request, native_paste_foreground())?;
        }
        Ok(request)
    }

    fn verify_super_menu_request(
        &self,
        request: &NativeSuperMenuPasteRequest,
        foreground: isize,
    ) -> Result<(), String> {
        self.verify_super_menu_lifecycle(request)?;
        self.verify_super_menu_target(request, foreground)
    }

    fn verify_super_menu_lifecycle(
        &self,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<(), String> {
        if request.owner != Arc::as_ptr(&self.inner) as usize {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        let target = self
            .menus
            .with_session(&request.session_id, |registry| {
                Ok(registry.session_target_handle(&request.session_id))
            })
            .map_err(|_| SUPER_MENU_INPUT_RETIRED.to_string())?;
        let foreground = self.native_super_menu_foreground();
        let mut inner = self.inner.lock();
        if request.keyboard.is_some() {
            inner
                .super_menu
                .native_keyboard
                .observe_foreground(foreground);
        }
        Self::verify_super_menu_presentation(&inner, request, target)
    }

    fn verify_super_menu_presentation(
        inner: &HostInner,
        request: &NativeSuperMenuRequest,
        registry_target: i64,
    ) -> Result<(), String> {
        if request.source_hwnd == 0
            || request.source_hwnd != inner.super_menu_hwnd
            || !inner.window_runtime_ready
            || !inner.super_menu.visible
            || inner.super_menu.session_id.as_deref() != Some(request.session_id.as_str())
            || inner.super_menu.generation != request.generation
            || registry_target != request.registry_target as i64
        {
            return Err(SUPER_MENU_INPUT_RETIRED.into());
        }
        if let Some(identity) = &request.keyboard {
            let keyboard = &inner.super_menu.native_keyboard;
            if inner.ole_drag_active
                || inner.native_incoming_drag.is_some()
                || inner.native_context_menu.is_some()
                || inner.qp.overlay_visible
                || inner.native_incoming_drag_next_id != identity.incoming_drag_generation
                || keyboard.epoch != identity.epoch
                || !keyboard.binding.as_ref().is_some_and(|binding| {
                    binding.foreground == identity.foreground && binding.scope == identity.scope
                })
            {
                return Err(SUPER_MENU_INPUT_RETIRED.into());
            }
        }
        Ok(())
    }

    fn verify_super_menu_target(
        &self,
        request: &NativeSuperMenuPasteRequest,
        _foreground: isize,
    ) -> Result<(), String> {
        request.target.ok_or("找不到可接收输入的目标窗口。")?;
        Ok(())
    }

    fn super_menu_action_guard(
        &self,
        request: &NativeSuperMenuPasteRequest,
    ) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let host = self.clone();
        let request = request.clone();
        Arc::new(move || host.verify_super_menu_lifecycle(&request))
    }

    fn super_menu_request_guard(
        &self,
        request: &NativeSuperMenuPasteRequest,
    ) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let host = self.clone();
        let request = request.clone();
        Arc::new(move || host.verify_super_menu_request(&request, native_paste_foreground()))
    }

    pub fn paste_super_menu_clip(
        &self,
        id: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        self.paste_super_menu_clip_for_foreground(id, request, native_paste_foreground)
    }

    fn paste_super_menu_clip_for_foreground(
        &self,
        id: &str,
        request: &NativeSuperMenuPasteRequest,
        mut foreground: impl FnMut() -> isize,
    ) -> Result<Value, String> {
        self.verify_super_menu_request(request, foreground())?;
        if id.is_empty() {
            return Err("id is required.".into());
        }
        let (store, opts) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                paste_options_from_config(&inner.config),
            )
        };
        let item = store.lock().get_clip(id).map_err(item_not_found)?;
        self.verify_super_menu_request(request, foreground())?;
        self.paste_surface_record_native(
            item,
            opts,
            request.target.ok_or("找不到可接收输入的目标窗口。")?,
            self.super_menu_request_guard(request),
        )?;
        self.touch_clip_usage(id);
        Ok(json!({ "ok": true }))
    }

    pub fn simulate_super_menu_clip(
        &self,
        id: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        self.verify_super_menu_request(request, native_paste_foreground())?;
        let item = self.store().lock().get_clip(id).map_err(item_not_found)?;
        let text = resolve_simulate_text(&item);
        if text.is_empty() {
            return Err("没有可模拟输入的文本。".into());
        }
        self.verify_super_menu_request(request, native_paste_foreground())?;
        let guard = self.super_menu_request_guard(request);
        let activating = guard.clone();
        let target = request.target.ok_or("找不到可接收输入的目标窗口。")?;
        self.run_input(move || {
            activating()?;
            target.activate_for_paste()
        })?;
        self.run_input(move || {
            crate::paste::simulate_text_to_with_guard(&text, 0, Some(target), || guard())
        })
        .map_err(|error| {
            if error == SUPER_MENU_INPUT_RETIRED {
                error
            } else {
                format!("模拟输入失败：{error}")
            }
        })?;
        self.touch_clip_usage(id);
        Ok(json!({ "ok": true }))
    }

    /// A menu picker belongs to the accepted menu HWND, even when the shared
    /// clipboard host is hidden or has another modal. Recheck unsuccessful and
    /// cancelled selections as well so an old menu cannot publish a result.
    fn with_super_menu_picker<T>(
        &self,
        request: &NativeSuperMenuPasteRequest,
        select: impl FnOnce(i64, &dyn Fn() -> Result<(), String>) -> Result<T, String>,
    ) -> Result<T, String> {
        let verify = || self.verify_super_menu_lifecycle(request);
        verify()?;
        let result = select(request.source_hwnd as i64, &verify);
        verify()?;
        result
    }

    /// Directory editor selections retain their original native menu ticket.
    /// Caller params cannot substitute another owner window.
    pub fn pick_folder_super_menu_native(
        &self,
        params: &Value,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        let initial = initial_path(params);
        let path = self.with_super_menu_picker(request, |owner, verify| {
            crate::file_dialog_pick::pick_folder_guarded(initial.as_deref(), owner, &verify)
        })?;
        Ok(json!({ "cancelled": path.is_none(), "path": path }))
    }

    pub(super) fn prompt_input_text_menu(
        &self,
        initial: &str,
        native: Option<&NativeSuperMenuPasteRequest>,
    ) -> Result<Option<String>, String> {
        match native {
            Some(request) => self.with_super_menu_picker(request, |owner, _| {
                Ok(crate::input_text::prompt_input_text(initial, owner))
            }),
            None => {
                let owner = self.inner.lock().host_hwnd as i64;
                Ok(crate::input_text::prompt_input_text(initial, owner))
            }
        }
    }

    pub(super) fn pick_folder_menu(
        &self,
        params: &Value,
        native: Option<&NativeSuperMenuPasteRequest>,
    ) -> Result<Value, String> {
        match native {
            Some(request) => self.pick_folder_super_menu_native(params, request),
            None => self.pick_folder(params),
        }
    }

    pub(super) fn pick_files_menu(
        &self,
        params: &Value,
        native: Option<&NativeSuperMenuPasteRequest>,
    ) -> Result<Value, String> {
        let Some(request) = native else {
            return self.pick_files(params);
        };
        let initial = initial_path(params);
        let paths = self.with_super_menu_picker(request, |owner, verify| {
            crate::file_dialog_pick::pick_files_guarded(initial.as_deref(), owner, &verify)
        })?;
        Ok(json!({ "cancelled": paths.is_empty(), "paths": paths }))
    }

    pub(super) fn pick_directories_menu(
        &self,
        params: &Value,
        native: Option<&NativeSuperMenuPasteRequest>,
    ) -> Result<Value, String> {
        let Some(request) = native else {
            return self.pick_directories(params);
        };
        let initial = initial_path(params);
        let paths = self.with_super_menu_picker(request, |owner, verify| {
            crate::file_dialog_pick::pick_directories_guarded(initial.as_deref(), owner, &verify)
        })?;
        Ok(json!({ "cancelled": paths.is_empty(), "paths": paths }))
    }

    /// params cannot replace the accepted registry source or destination.
    pub fn menu_invoke_native(
        &self,
        node_id: &str,
        _params: &Value,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        self.menu_invoke_native_after_node(node_id, request, || {})
    }

    /// Native value requests are scoped to the accepted original session and
    /// source HWND. Caller params cannot retarget a new menu or its receiver.
    /// Like the existing WebView endpoint, a successful value patch keeps the
    /// menu and its generation alive rather than rebuilding/dismissing it.
    pub fn menu_set_value_native(
        &self,
        params: &Value,
        request: &NativeSuperMenuRequest,
    ) -> Result<Value, String> {
        self.menu_set_value_native_after_node(params, request, || {})
    }

    fn menu_set_value_native_after_node(
        &self,
        params: &Value,
        request: &NativeSuperMenuRequest,
        after_node: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_super_menu_lifecycle(request)?;
        let node_id = params
            .get("nodeId")
            .or_else(|| params.get("NodeId"))
            .or_else(|| params.get("id"))
            .or_else(|| params.get("Id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let value = params
            .get("value")
            .or_else(|| params.get("Value"))
            .and_then(Value::as_f64);
        let scoped = self.native_menu_scoped_host(request);
        scoped.menu_set_value_scoped(
            &request.session_id,
            node_id,
            value,
            Some(request),
            after_node,
        )
    }

    /// Opacity changes need their actual owner-thread guard, even though they
    /// neither activate an external target nor send keyboard/clipboard input.
    pub(super) fn set_native_menu_opacity(
        &self,
        hwnd: i64,
        alpha: u8,
        request: &NativeSuperMenuRequest,
    ) -> Result<(), String> {
        self.verify_super_menu_lifecycle(request)?;
        let guard = self.super_menu_action_guard(request);
        self.run_input(move || {
            guard()?;
            crate::window_menu::set_opacity(hwnd, alpha)
        })
    }

    pub(super) fn patch_native_menu_value(
        &self,
        node_id: &str,
        current: f64,
        request: &NativeSuperMenuRequest,
    ) -> Result<Value, String> {
        self.verify_super_menu_lifecycle(request)?;
        self.menus
            .with_session(&request.session_id, |registry| {
                let target = registry.session_target_handle(&request.session_id);
                let inner = self.inner.lock();
                // Hold the short UI-state lock only across this in-memory patch;
                // no materialization, OS calls, queue waits or disk IO run here.
                Self::verify_super_menu_presentation(&inner, request, target)?;
                if request.owner != Arc::as_ptr(&self.inner) as usize {
                    return Err(SUPER_MENU_INPUT_RETIRED.into());
                }
                registry.patch_value(&request.session_id, node_id, current)
            })
            .map_err(|error| {
                if error == "Session not found" {
                    SUPER_MENU_INPUT_RETIRED.into()
                } else {
                    error
                }
            })
    }

    fn menu_invoke_native_after_node(
        &self,
        node_id: &str,
        request: &NativeSuperMenuPasteRequest,
        after_node: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_super_menu_lifecycle(request)?;
        let node = self.menus.node(&request.session_id, node_id)?;
        after_node();
        self.verify_super_menu_lifecycle(request)?;
        let scoped = self.native_menu_scoped_host(request);
        let path = node
            .payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("");
        match node.role.as_deref() {
            Some("window-copy")
            | Some("file-copy")
            | Some("file-copy-path")
            | Some("file-copy-quoted")
            | Some("file-copy-name")
            | Some("file-copy-stem")
            | Some("file-copy-parent")
            | Some("bookmark-copy")
            | Some("bookmark-copy-title")
            | Some("favor-copy-link")
            | Some("favor-copy-link-title") => scoped.copy_native_menu_node(&node, request),
            Some("favor") | Some("favor-folder") => {
                if let Some(id) = node.payload.get("favorId").and_then(Value::as_str) {
                    scoped.execute_native_menu_favor(id, request)
                } else {
                    Ok(invoke_ok(
                        node.role.as_deref() == Some("favor"),
                        "None",
                        Value::Null,
                    ))
                }
            }
            Some("text") if !node.has_children => {
                scoped.execute_native_menu_text(&node.payload, request)
            }
            Some("file-copy-paste") | Some("file-paste-on-window") => {
                if !path.is_empty() {
                    scoped.verify_super_menu_request(request, native_paste_foreground())?;
                    let files=native_menu_output_files(&scoped.store().lock(),&[path.to_string()])?;
                    let target = request.target.ok_or("找不到可接收输入的目标窗口。")?;
                    scoped.paste_surface_payload_native(
                        PasteOptions::default(),
                        target,
                        scoped.super_menu_request_guard(request),
                        || {
                            crate::clipboard_payload::prepare_text_and_files(
                                "",
                                &files,
                            )
                        },
                        None,
                    )?;
                }
                Ok(invoke_ok(true, "None", Value::Null))
            }
            Some("file-move-selected") => {
                let target = scoped.activate_native_menu_target(request)?;
                let selected =
                    scoped.capture_native_menu_selection(target, request, peek_snapshot)?;
                scoped.verify_super_menu_lifecycle(request)?;
                if let Some(selected) = selected {
                    for file in selected.value.files {
                        scoped.verify_super_menu_lifecycle(request)?;
                        crate::shell_file::move_into_dir(&file, path)?;
                    }
                } else {
                    scoped.popup_error("获取选中文件失败");
                }
                Ok(invoke_ok(true, "None", Value::Null))
            }
            Some("favor-add") | Some("favor-add-clipboard") => {
                scoped.collect_native_menu_favor(&node.payload, request)
            }
            Some("file-cut") => {
                if !path.is_empty() {
                    let _operation = scoped.clipboard_operation.lock();
                    scoped.verify_super_menu_lifecycle(request)?;
                    crate::shell_file::cut(path)?;
                }
                Ok(invoke_ok(true, "None", Value::Null))
            }
            Some("file") | Some("reopen") | Some("opened-folder") => {
                scoped.open_native_menu_path(&node, request)
            }
            _ => scoped.menu_invoke_node(
                &request.session_id,
                node_id,
                node,
                request.source_hwnd as i64,
                request.registry_target as i64,
                Some(request),
            ),
        }
    }

    pub(super) fn menu_native_check(
        &self,
        request: Option<&NativeSuperMenuPasteRequest>,
    ) -> Result<(), String> {
        if let Some(request) = request {
            self.verify_super_menu_lifecycle(request)?;
        }
        Ok(())
    }

    /// Runners are local to this dispatch clone. Updating them never changes
    /// other windows' dispatchers or installs a process-wide ambient ticket.
    fn native_menu_scoped_host(&self, request: &NativeSuperMenuPasteRequest) -> Self {
        let mut scoped = self.clone();
        let guard = self.super_menu_action_guard(request);
        let clipboard = self.clipboard_ui.lock().clone();
        let clipboard_guard = guard.clone();
        scoped.clipboard_ui = Arc::new(Mutex::new(Some(Arc::new(
            move |job: Box<dyn FnOnce() -> Result<(), String> + Send>| {
                clipboard_guard()?;
                let queued_guard = clipboard_guard.clone();
                let guarded: Box<dyn FnOnce() -> Result<(), String> + Send> = Box::new(move || {
                    queued_guard()?;
                    job()
                });
                match &clipboard {
                    Some(runner) => runner(guarded),
                    None => guarded(),
                }
            },
        ) as OperationRunner)));
        let input = self.input_runner.lock().clone();
        let input_guard = guard.clone();
        scoped.input_runner = Arc::new(Mutex::new(Some(Arc::new(
            move |job: Box<dyn FnOnce() -> Result<(), String> + Send>| {
                input_guard()?;
                let queued_guard = input_guard.clone();
                let guarded: Box<dyn FnOnce() -> Result<(), String> + Send> = Box::new(move || {
                    queued_guard()?;
                    job()
                });
                match &input {
                    Some(runner) => runner(guarded),
                    None => guarded(),
                }
            },
        ) as OperationRunner)));
        let shell = self.shell_open_runner.lock().clone();
        scoped.shell_open_runner = Arc::new(Mutex::new(Some(Arc::new(move |path: &str| {
            guard()?;
            match &shell {
                Some(runner) => runner(path),
                None => crate::shell_open::open(path),
            }
        }) as ShellOpenRunner)));
        scoped
    }

    fn activate_native_menu_target(
        &self,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<InputTarget, String> {
        self.verify_super_menu_request(request, native_paste_foreground())?;
        let target = request.target.ok_or("找不到可接收输入的目标窗口。")?;
        // Selection copy and Explorer/dialog operations still belong to the
        // original receiver. They must not inspect a newly foreground app.
        verify_surface_foreground(request.source_hwnd, target, native_paste_foreground())?;
        let guard = self.super_menu_request_guard(request);
        let source = request.source_hwnd;
        self.run_input(move || {
            guard()?;
            verify_surface_foreground(source, target, native_paste_foreground())?;
            target.activate()
        })?;
        Ok(target)
    }

    fn copy_native_menu_node(
        &self,
        node: &crate::menu_api::MenuItemNode,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        let role = node.role.as_deref().unwrap_or("");
        let path = node
            .payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = match role {
            "window-copy" => node
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string),
            "file-copy" | "file-copy-path" if path.is_empty() => None,
            "file-copy" | "file-copy-path" => Some(path.to_string()),
            "file-copy-quoted" => Some(crate::shell_file::json_quoted_path(path)),
            "file-copy-name" => Some(
                Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            "file-copy-stem" => Some(
                Path::new(path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            "file-copy-parent" => Some(
                Path::new(path)
                    .parent()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            "bookmark-copy" | "favor-copy-link" => node
                .payload
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string),
            "bookmark-copy-title" | "favor-copy-link-title" => {
                let url = node
                    .payload
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let title = node
                    .payload
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&node.title);
                Some(format!("{title}\r\n{url}"))
            }
            _ => None,
        };
        if let Some(text) = text {
            let guard = self.super_menu_action_guard(request);
            let preparing = guard.clone();
            let files = (role == "file-copy")
                .then(||native_menu_output_files(&self.store().lock(),&[path.to_string()]))
                .transpose()?;
            let toast_text = text.clone();
            self.run_clipboard_write_then_guarded(
                move || {
                    preparing()?;
                    if let Some(files) = files {
                        crate::clipboard_payload::prepare_text_and_files("", &files)
                    } else {
                        crate::clipboard_payload::prepare_text(&text)
                    }
                },
                |_| Ok(()),
                None,
                guard,
            )?;
            if role.starts_with("bookmark-copy") {
                self.popup_success(&toast_text);
            }
        }
        Ok(invoke_ok(true, "None", Value::Null))
    }

    fn capture_native_menu_selection<T>(
        &self,
        target: InputTarget,
        request: &NativeSuperMenuPasteRequest,
        mut read: impl FnMut() -> Option<T>,
    ) -> Result<Option<crate::selection_capture::Captured<T>>, String> {
        let _operation = self.clipboard_operation.lock();
        let guard = self.super_menu_request_guard(request);
        guard()?;
        self.suppress_clipboard_capture();
        let previous = crate::clipboard::sequence_number();
        if !crate::clipboard::is_headless() && previous == 0 {
            return Err("无法读取剪贴板状态，已停止获取选中内容。".into());
        }
        let copying_guard = guard.clone();
        self.run_input(move || {
            copying_guard()?;
            target.verify()?;
            crate::paste::send_ctrl_c()
        })?;
        let result = if crate::clipboard::is_headless() {
            let value = read();
            guard()?;
            Ok(value.map(|value| crate::selection_capture::Captured {
                value,
                sequence: crate::selection_capture::current_sequence(),
            }))
        } else {
            crate::selection_capture::wait_for_copy(
                previous,
                || {
                    guard()?;
                    target.verify()
                },
                read,
            )
        };
        self.suppress_clipboard_capture();
        result
    }

    fn execute_native_menu_text(
        &self,
        payload: &Value,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        let id = payload
            .get("actionId")
            .and_then(Value::as_str)
            .ok_or_else(|| "Text action not found.".to_string())?;
        let action = crate::text_actions::get_by_id(&self.inner.lock().text_actions, id)
            .cloned()
            .ok_or_else(|| "Text action not found.".to_string())?;
        self.execute_native_menu_text_snapshot(action, request)
    }

    /// Execute the exact rule displayed by the searchable native launcher.
    /// A deleted/reordered/edited index must never select a different rule.
    pub fn text_action_execute_native_checked(
        &self,
        id: &str,
        rule_token: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        self.verify_super_menu_request(request, native_paste_foreground())?;
        let action = self.text_action_snapshot_checked(id, rule_token)?;
        self.execute_native_menu_text_snapshot(action, request)
    }

    fn execute_native_menu_text_snapshot(
        &self,
        action: Value,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        let needs_input = crate::text_actions::needs_input(&action);
        let action_type = action.get("Type").or_else(|| action.get("type"));
        let launch_only = action_type.is_some_and(|kind| {
            kind.as_str().is_some_and(|kind| kind.eq_ignore_ascii_case("QuickerAction"))
                || kind.as_i64() == Some(5)
        });
        let target = if needs_input {
            self.activate_native_menu_target(request)?
        } else {
            self.verify_super_menu_request(request, native_paste_foreground())?;
            let target = request.target.ok_or("找不到可接收输入的目标窗口。")?;
            let activating = self.super_menu_request_guard(request);
            self.run_input(move || {
                activating()?;
                target.activate_for_paste()
            })?;
            target
        };
        let selection = if needs_input {
            match self
                .capture_native_menu_selection(
                    target,
                    request,
                    crate::clipboard::read_selection_text,
                )
                .and_then(|value| value.ok_or_else(|| "获取选中文本失败".to_string()))
            {
                Ok(selection) => Some(selection),
                Err(message) => {
                    self.verify_super_menu_request(request, native_paste_foreground())?;
                    self.popup_warning(&message);
                    return Ok(invoke_ok(true, "None", Value::Null));
                }
            }
        } else {
            None
        };
        self.verify_super_menu_request(request, native_paste_foreground())?;
        let input = selection
            .as_ref()
            .map(|value| value.value.as_str())
            .unwrap_or_default();
        let expected_sequence = selection.as_ref().and_then(|value| value.sequence);
        let output = crate::text_actions::apply_with_runners(
            &action,
            input,
            |id, input| {
                self.verify_super_menu_request(request, native_paste_foreground())?;
                self.run_quicker_action(id, input)
            },
            |expression, input| {
                self.verify_super_menu_request(request, native_paste_foreground())?;
                crate::quicker_expression::text_result(crate::quicker_expression::evaluate(
                    &self.quicker_rpc,
                    expression,
                    &crate::quicker_expression::text_params(input),
                )?)
            },
        )?;
        self.verify_super_menu_request(request, native_paste_foreground())?;
        // Quicker receives the captured input and owns all subsequent effects.
        // Its protocol has no result channel; do not re-paste the old selection
        // (or overwrite a clipboard value changed by the action itself).
        if launch_only {
            return Ok(invoke_ok(true, "None", Value::Null));
        }
        if output.is_empty() {
            return Ok(invoke_ok(true, "None", Value::Null));
        }
        crate::selection_capture::verify_unchanged(expected_sequence)?;
        let simulate = action
            .get("Simulate")
            .or_else(|| action.get("simulate"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let delay = action
            .get("SimulateDelay")
            .or_else(|| action.get("simulateDelay"))
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32;
        let guard = self.super_menu_request_guard(request);
        if simulate {
            self.run_input(move || {
                crate::paste::simulate_text_to_with_guard(&output, delay, Some(target), || {
                    guard()?;
                    crate::selection_capture::verify_unchanged(expected_sequence)
                })
            })?;
        } else {
            let opts = PasteOptions {
                delay_ms: 50,
                extra_shift_insert: self
                    .inner
                    .lock()
                    .config
                    .use_shift_insert_to_paste_proc
                    .clone(),
                ..Default::default()
            };
            self.paste_surface_payload_native(
                opts,
                target,
                guard,
                || crate::clipboard_payload::prepare_text(&output),
                expected_sequence,
            )?;
        }
        Ok(invoke_ok(true, "None", Value::Null))
    }

    fn execute_native_menu_favor(
        &self,
        id: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        let item = self
            .store()
            .lock()
            .get_favor(id)
            .map_err(|error| error.to_string())?;
        let item=self.store().lock().core_io_favor(&item).map_err(|e|e.to_string())?;
        self.verify_super_menu_lifecycle(request)?;
        if item.is_folder || item.kind == ClipKind::FavoriteClipItem {
            self.panel_show_folder(id);
            return Ok(invoke_ok(true, "None", Value::Null));
        }
        match item.kind {
            ClipKind::FileClipItem => {
                let paths = if item.file_paths.is_empty() {
                    item.body_text
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| vec![s.to_string()])
                        .unwrap_or_default()
                } else {
                    item.file_paths.clone()
                };
                let Some(first) = paths.iter().map(|s| s.trim()).find(|s| !s.is_empty()) else {
                    return Err("没有可打开的文件。".into());
                };
                if !self.locate_native_menu_dialog(first, request)? {
                    for (opened, path) in paths
                        .iter()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .enumerate()
                    {
                        self.open_shell_target(path).map_err(|error| {
                            if opened == 0 {
                                error
                            } else {
                                format!("已打开 {opened} 项，后续打开失败：{error}")
                            }
                        })?;
                    }
                }
            }
            ClipKind::LinkClipItem => {
                self.open_shell_target(item.body_text.as_deref().unwrap_or("").trim())?
            }
            ClipKind::ActionClipItem => {
                let action_id = crate::quicker_action::action_id_from_payload(
                    item.body_text.as_deref().unwrap_or(""),
                )?;
                self.verify_super_menu_lifecycle(request)?;
                if request.target.is_some() {
                    self.activate_native_menu_target(request)?;
                }
                self.run_quicker_action(&action_id, &item.execute_param)?;
            }
            _ => {
                self.verify_super_menu_request(request, native_paste_foreground())?;
                let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
                let opts = paste_options_from_config(&self.inner.lock().config);
                self.paste_surface_record_native(
                    clip,
                    opts,
                    request.target.ok_or("找不到可接收输入的目标窗口。")?,
                    self.super_menu_request_guard(request),
                )?;
            }
        }
        self.touch_favor_usage(id);
        Ok(invoke_ok(true, "None", Value::Null))
    }

    fn collect_native_menu_favor(
        &self,
        payload: &Value,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        let mut params = json!({});
        if let Some(parent) = favor_parent_from_payload(payload) {
            self.resolve_optional_favor_parent(Some(&parent))?;
            params["parentId"] = json!(parent);
        }
        let target = self.activate_native_menu_target(request)?;
        let Some(snapshot) = self.capture_native_menu_selection(target, request, peek_snapshot)?
        else {
            return Err("获取选中内容失败，剪贴板未更新或暂不可用。".into());
        };
        self.verify_super_menu_request(request, native_paste_foreground())?;
        params = self.with_menu_favor_insert_index(params);
        let added = self.favor_from_snapshot(&params, snapshot.value)?;
        if added.get("ok") != Some(&json!(true))
            || added.get("count").and_then(Value::as_i64).unwrap_or(0) <= 0
        {
            self.popup_warning("无剪贴板内容");
        }
        Ok(invoke_ok(true, "None", Value::Null))
    }

    fn locate_native_menu_dialog(
        &self,
        path: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<bool, String> {
        self.verify_super_menu_lifecycle(request)?;
        if request.registry_target == 0
            || !crate::file_dialog_locate::is_file_dialog_hwnd(request.registry_target as i64)
        {
            return Ok(false);
        }
        let target = self.activate_native_menu_target(request)?;
        let path = path.to_string();
        let guard = self.super_menu_request_guard(request);
        let located = Arc::new(AtomicBool::new(false));
        let result = located.clone();
        self.run_input(move || {
            let hit = crate::file_dialog_locate::try_locate_hwnd_guarded(
                target.hwnd() as i64,
                &path,
                || {
                    guard()?;
                    target.verify()
                },
            )?;
            result.store(hit, Ordering::SeqCst);
            Ok(())
        })?;
        Ok(located.load(Ordering::SeqCst))
    }

    fn open_native_menu_path(
        &self,
        node: &crate::menu_api::MenuItemNode,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        self.verify_super_menu_lifecycle(request)?;
        let path = node
            .payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let folder_hwnd = node
            .payload
            .get("windowHandle")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let prefer_internal = node.role.as_deref() == Some("file")
            && node.payload.get("isDirectory").and_then(Value::as_bool) == Some(true)
            && self.inner.lock().extras.file_menu.prefer_internal_browser;
        if node.role.as_deref() == Some("opened-folder") && folder_hwnd == 0 {
            let (opened, reopen) = self.menus.session_opened_and_reopen(&request.session_id);
            self.verify_super_menu_lifecycle(request)?;
            let locate = opened
                .iter()
                .map(|f| f.path.as_str())
                .find(|p| !p.trim().is_empty())
                .or_else(|| {
                    reopen
                        .iter()
                        .map(String::as_str)
                        .find(|p| !p.trim().is_empty())
                })
                .unwrap_or("");
            self.require_external_effects("navigate external files or windows")?;
            if !locate.is_empty() && self.locate_native_menu_dialog(locate, request)? {
                return Ok(invoke_ok(true, "None", Value::Null));
            }
            self.verify_super_menu_lifecycle(request)?;
            if let Some(first) = opened.iter().find(|f| f.hwnd != 0) {
                crate::menu_api::activate_window(first.hwnd)?;
            } else {
                self.reopen_last_folder()?;
            }
        } else if !path.trim().is_empty() {
            // Existing Open / Save As targets retain the guarded same-dialog
            // workflow. Merely browsing internally needs no external effects.
            if request.registry_target != 0
                && crate::file_dialog_locate::is_file_dialog_hwnd(request.registry_target as i64)
            {
                self.require_external_effects("navigate an external file dialog")?;
                if self.locate_native_menu_dialog(path, request)? {
                    return Ok(invoke_ok(true, "None", Value::Null));
                }
            }
            if prefer_internal {
                let path = crate::file_browser::browser_normalize_path(path)?;
                self.verify_super_menu_lifecycle(request)?;
                let mut reply = invoke_ok(false, "None", Value::Null);
                reply["browsePath"] = json!(path);
                return Ok(reply);
            }
            self.require_external_effects("navigate external files or windows")?;
            self.verify_super_menu_lifecycle(request)?;
            let navigated = if let Some(target) = request.target {
                target.verify_identity()?;
                self.verify_super_menu_lifecycle(request)?;
                crate::explorer::try_navigate_path(target.hwnd() as i64, path)
            } else {
                false
            };
            if !navigated {
                self.verify_super_menu_lifecycle(request)?;
                if node.role.as_deref() == Some("reopen") {
                    self.reopen_named_or_last(path)?;
                } else if folder_hwnd != 0
                    && !crate::clipboard::is_headless()
                    && crate::window_menu::activate(folder_hwnd).is_ok()
                {
                } else {
                    self.open_shell_target(path)?;
                }
            }
        } else if node.role.as_deref() == Some("reopen") {
            self.require_external_effects("reopen an external folder")?;
            self.reopen_named_or_last(path)?;
        }
        Ok(invoke_ok(true, "None", Value::Null))
    }

    /// What a plain folder click reaches for this accepted menu: `dialog`,
    /// `explorer` or `other`, plus the target title for the menu's hint line.
    pub fn super_menu_folder_target(&self, request: &NativeSuperMenuPasteRequest) -> (&'static str, String) {
        let hwnd = request.registry_target as i64;
        if hwnd == 0 {
            return ("other", String::new());
        }
        let info = crate::window_menu::query_target(hwnd);
        let kind = if crate::file_dialog_locate::is_file_dialog_hwnd(hwnd) {
            "dialog"
        } else if crate::explorer::is_explorer_window_class(&info.class_name) {
            "explorer"
        } else {
            "other"
        };
        (kind, info.title)
    }

    /// The docked browser's footer sends a folder to the accepted target with
    /// a folder row's Open / Save As and Explorer rules. It never applies the
    /// built-in browser preference: the request comes from that browser.
    pub fn menu_go_to_folder_native(
        &self,
        path: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<Value, String> {
        self.verify_super_menu_lifecycle(request)?;
        let path = crate::file_browser::browser_normalize_path(path)?;
        if path.trim().is_empty() {
            return Err("请选择一个文件夹。".into());
        }
        if request.registry_target != 0
            && crate::file_dialog_locate::is_file_dialog_hwnd(request.registry_target as i64)
        {
            self.require_external_effects("navigate an external file dialog")?;
            if self.locate_native_menu_dialog(&path, request)? {
                return Ok(invoke_ok(true, "None", Value::Null));
            }
        }
        self.require_external_effects("navigate external files or windows")?;
        self.verify_super_menu_lifecycle(request)?;
        let navigated = if let Some(target) = request.target {
            target.verify_identity()?;
            self.verify_super_menu_lifecycle(request)?;
            crate::explorer::try_navigate_path(target.hwnd() as i64, &path)
        } else {
            false
        };
        if !navigated {
            self.verify_super_menu_lifecycle(request)?;
            self.open_shell_target(&path)?;
        }
        Ok(invoke_ok(true, "None", Value::Null))
    }

    /// Browser "copy path" uses the menu's existing text writer and session scope.
    pub fn menu_copy_path_native(
        &self,
        path: &str,
        request: &NativeSuperMenuPasteRequest,
    ) -> Result<(), String> {
        self.verify_super_menu_lifecycle(request)?;
        if path.trim().is_empty() {
            return Err("没有可复制的路径。".into());
        }
        self.copy_text(path)
    }

    /// Reuse the existing prepared-clipboard writer and input dispatcher. The
    /// surface guard also runs after preparation/queueing and throughout input
    /// delay, while InputTarget follows the current external paste receiver.
    fn paste_surface_record_native(
        &self,
        clip: ClipRecord,
        opts: PasteOptions,
        target: InputTarget,
        guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Result<(), String> {
        let clip=self.store().lock().core_io_clip(&clip).map_err(|e|e.to_string())?;
        let copy_opts = opts.clone();
        self.paste_surface_payload_native(
            opts,
            target,
            guard,
            move || crate::paste::prepare_copy_record(&clip, &copy_opts),
            None,
        )
    }

    fn paste_surface_payload_native(
        &self,
        opts: PasteOptions,
        target: InputTarget,
        guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
        prepare: impl FnOnce() -> Result<crate::clipboard_payload::PreparedClipboard, String>,
        expected_sequence: Option<u32>,
    ) -> Result<(), String> {
        guard()?;
        let activating = guard.clone();
        self.run_input(move || {
            activating()?;
            target.activate_for_paste()
        })?;
        self.paste_native_payload_after_activation(opts, target, guard, prepare, expected_sequence)
    }

    pub(super) fn paste_native_payload_after_activation(
        &self,
        mut opts: PasteOptions,
        target: InputTarget,
        guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
        prepare: impl FnOnce() -> Result<crate::clipboard_payload::PreparedClipboard, String>,
        expected_sequence: Option<u32>,
    ) -> Result<(), String> {
        guard()?;
        opts.expected_target = Some(target);
        let prepare_guard = guard.clone();
        let write_guard = guard.clone();
        let writing_guard = Arc::new(move || {
            write_guard()?;
            target.verify_for_paste()
        });
        self.run_clipboard_write_then_guarded(
            move || {
                prepare_guard()?;
                prepare()
            },
            |sequence| {
                opts.expected_clipboard_sequence = sequence;
                self.run_input(move || crate::paste::send_paste_with_guard(&opts, || guard()))
            },
            expected_sequence,
            writing_guard,
        )
    }
}

fn verify_surface_foreground(
    source: isize,
    target: InputTarget,
    foreground: isize,
) -> Result<(), String> {
    if foreground != 0 && foreground != source && foreground != target.hwnd() {
        return Err("目标窗口已切换，无法继续原窗口操作。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    #[cfg(windows)]
    fn core_native_menu_raw_files_map_only_to_explicit_candidate_roots(){
        let parent=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let parent=parent.canonicalize().unwrap();
        let directory=tempfile::Builder::new().prefix("native-menu-core-files-").tempdir_in(parent).unwrap();
        let source=directory.path().join("source");std::fs::create_dir_all(source.join("Image/Temp")).unwrap();
        std::fs::write(source.join("Image/Temp/menu-file.txt"),b"clone-only bytes").unwrap();
        let native=AppStore::open(&source.join("userdata.db")).unwrap();
        native.insert_clip(&ClipRecord::new_text("synthetic fixture","fixture"),true).unwrap();drop(native);
        let candidate=directory.path().join("candidate");
        let old_root=PathBuf::from(r"Z:\cliphost-native-menu-offline-original");
        crate::storage_core::migrate::build_candidate_with_settings(
            &crate::storage_core::migrate::MigrationOptions{source_root:source.clone(),candidate_root:candidate.clone()},
            &crate::storage_core::migrate::MigrationSettings{source_aliases:vec![old_root.clone()],source_local_offset_seconds:None},
            &mut |_|Ok(())
        ).unwrap();
        let store=AppStore::open_isolated_core(&candidate.join("userdata.db")).unwrap();
        let relative=Path::new("Image/Temp/menu-file.txt");
        let expected=candidate.join(relative).to_string_lossy().into_owned();
        for original in [old_root.join(relative),source.join(relative),candidate.join(relative)]{
            let projected=native_menu_output_files(&store,&[original.to_string_lossy().into_owned()]).unwrap();
            assert_eq!(projected.len(),1);
            assert_eq!(std::fs::canonicalize(&projected[0]).unwrap(),std::fs::canonicalize(&expected).unwrap());
            assert_eq!(std::fs::read(&projected[0]).unwrap(),b"clone-only bytes");
        }
        let outside=directory.path().join("outside.txt");std::fs::write(&outside,b"unmapped synthetic external").unwrap();
        for refused in [outside,PathBuf::from(r"Z:\cliphost-native-menu-offline-original-extra\Image\Temp\menu-file.txt"),old_root.join("Image/Temp/missing.txt"),candidate.join("Image/Temp")]{
            assert!(native_menu_output_files(&store,&[refused.to_string_lossy().into_owned()]).is_err());
        }
        assert_eq!(std::fs::read(source.join(relative)).unwrap(),b"clone-only bytes");
    }

    struct Fixture {
        _root: tempfile::TempDir,
        host: HostState,
        clip: String,
        folder: String,
        favor: String,
        writes: Arc<AtomicUsize>,
        inputs: Arc<AtomicUsize>,
    }

    fn fixture() -> Fixture {
        crate::clipboard::force_headless(true);
        let temp_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let root = tempfile::Builder::new()
            .prefix("native-surfaces-")
            .tempdir_in(temp_root)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
        let clip = host
            .add_text(&json!({"text":"native surface text"}))
            .unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        let folder = host
            .favor_folder_create(&json!({"title":"surface panel"}))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let favor = host
            .favor_add_text_items(&json!({"parentId":folder,"items":[{"text":"surface favorite"}]}))
            .unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        host.set_window_runtime_ready(true);
        host.set_panel_web_view_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.panel_hwnd = 100;
            inner.super_menu_hwnd = 200;
            inner.last_non_host_fg = 42;
            inner.ui.panel_visible = true;
            inner.ui.panel_folder_id = Some(folder.clone());
            inner.ui.panel_no_focus_session = true;
            inner.ui.panel_fake_focus = true;
        }
        while host.take_chrome_task().is_some() {}
        let writes = Arc::new(AtomicUsize::new(0));
        let written = writes.clone();
        host.set_clipboard_ui_runner(move |job| {
            job()?;
            written.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let inputs = Arc::new(AtomicUsize::new(0));
        let sent = inputs.clone();
        host.set_input_runner(move |job| {
            sent.fetch_add(1, Ordering::SeqCst);
            job()
        });
        Fixture {
            _root: root,
            host,
            clip,
            folder,
            favor,
            writes,
            inputs,
        }
    }

    fn super_request(f: &Fixture) -> NativeSuperMenuPasteRequest {
        let menu = f.host.menu_create_session_ex("text", "", 200, 42);
        let generation = f.host.super_menu_state_detail()["generation"]
            .as_i64()
            .unwrap() as i32;
        f.host
            .capture_super_menu_paste(menu["sessionId"].as_str().unwrap(), generation, 200)
            .unwrap()
    }

    #[test]
    fn native_super_menu_picker_keeps_menu_owner_and_selection_results() {
        let f = fixture();
        f.host.set_host_hwnd(100);
        let request = super_request(&f);
        for selection in [
            Ok(Some("selected directory".to_string())),
            Ok(None),
            Err("picker failed".to_string()),
        ] {
            let result = f.host.with_super_menu_picker(&request, |owner, verify| {
                assert_eq!(owner, 200, "picker belongs to the accepted menu HWND");
                assert_ne!(owner, f.host.host_hwnd() as i64);
                verify()?;
                selection.clone()
            });
            assert_eq!(result, selection);
        }
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_super_menu_picker_rejects_retired_request_before_selection() {
        for retirement in ["hide-reopen", "registry", "target", "root", "runtime"] {
            let f = fixture();
            let request = super_request(&f);
            match retirement {
                "hide-reopen" => {
                    f.host.super_menu_hide_session();
                    f.host.menu_create_session_ex("text", "", 200, 42);
                }
                "registry" => {
                    f.host.menus.close(&request.session_id);
                }
                "target" => f.host.menus.set_session_handles(&request.session_id, 200, 43),
                "root" => f.host.set_super_menu_hwnd(201),
                "runtime" => f.host.set_window_runtime_ready(false),
                _ => unreachable!(),
            }
            let mut called = false;
            let result = f.host.with_super_menu_picker(&request, |_, _| {
                called = true;
                Ok(None::<String>)
            });
            assert_eq!(result.unwrap_err(), SUPER_MENU_INPUT_RETIRED, "{retirement}");
            assert!(!called, "retired request must not prepare a picker: {retirement}");
        }
    }

    #[test]
    fn native_super_menu_picker_rechecks_retirement_during_and_after_selection() {
        for selection in [
            Ok(Some("selected directory".to_string())),
            Ok(None),
            Err("picker failed".to_string()),
        ] {
            let f = fixture();
            let request = super_request(&f);
            let result = f.host.with_super_menu_picker(&request, |owner, verify| {
                assert_eq!(owner, 200);
                verify()?;
                f.host.super_menu_hide_session();
                assert_eq!(verify().unwrap_err(), SUPER_MENU_INPUT_RETIRED);
                selection.clone()
            });
            assert_eq!(result.unwrap_err(), SUPER_MENU_INPUT_RETIRED);
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        }
    }

    fn menu_key(vk: u32, down: bool) -> KeyEvent {
        KeyEvent {
            vk,
            down,
            alt: false,
            ctrl: false,
            shift: false,
            win: false,
            now_ms: 1,
        }
    }

    fn keyboard_fixture(scope: NativeSuperMenuKeyScope) -> (Fixture, NativeSuperMenuRequest) {
        let f = fixture();
        {
            let mut inner = f.host.inner.lock();
            inner.ui.panel_visible = false;
            inner.window_visible = false;
            inner.has_host_fake_focus = false;
            inner.last_fg = 42;
        }
        let request = super_request(&f);
        f.host
            .bind_native_super_menu_keyboard(&request, scope)
            .unwrap();
        while f.host.take_chrome_task().is_some() {}
        (f, request)
    }

    #[test]
    fn native_super_menu_focused_input_retires_queued_keys_but_keeps_release_drains() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let taken = f.host.take_native_super_menu_key().unwrap();
        assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
        f.host.use_focused_native_super_menu_input(&request).unwrap();
        {
            let inner = f.host.inner.lock();
            let keyboard = &inner.super_menu.native_keyboard;
            assert!(keyboard.owns_presentation(&inner));
            assert!(keyboard.binding.is_none());
            assert!(keyboard.inbox.is_empty());
        }
        assert!(!f.host.native_super_menu_key_is_current(&taken));
        assert!(!f.host.native_super_menu_request_is_current(taken.request()));
        assert!(f.host.take_native_super_menu_key().is_none());
        // Previously swallowed keydowns still own their matching releases;
        // neither release may reach the now-focused text field.
        for vk in [0x27, 0x1B] {
            assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(vk, false), 42));
            assert!(!f.host.dispatch_low_level_key_for_foreground(menu_key(vk, false), 42));
        }
        assert!(f.host.take_native_super_menu_key().is_none());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_super_menu_focused_input_leaves_escape_text_and_ime_to_local_input() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::Dismiss);
        f.host.use_focused_native_super_menu_input(&request).unwrap();
        // Repeated UI polling must retain this opt-in without reclaiming keys.
        f.host.use_focused_native_super_menu_input(&request).unwrap();
        for vk in [0x1B, 0x41, 0x31, 0x25, 0x0D, 0xE5] {
            let mut inner = f.host.inner.lock();
            assert_eq!(f.host.input_route_native_super_menu_key(&mut inner, menu_key(vk, true), 42, false), Some(false),
                "native opt-in must suppress WebView/clipboard fallback for key {vk}");
            drop(inner);
            assert!(!f.host.dispatch_low_level_key_for_foreground(menu_key(vk, true), 42));
            assert!(!f.host.dispatch_low_level_key_for_foreground(menu_key(vk, false), 42));
        }
        assert!(f.host.inner.lock().super_menu.visible, "global Escape must not close a focused IME search");
        assert!(f.host.take_native_super_menu_key().is_none());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_super_menu_focused_input_rejects_old_presentation_after_reopen() {
        let (f, old) = keyboard_fixture(NativeSuperMenuKeyScope::Dismiss);
        f.host.use_focused_native_super_menu_input(&old).unwrap();
        f.host.super_menu_hide_session();
        let current = super_request(&f);
        assert_eq!(f.host.use_focused_native_super_menu_input(&old).unwrap_err(), SUPER_MENU_INPUT_RETIRED);
        f.host.use_focused_native_super_menu_input(&current).unwrap();
        let inner = f.host.inner.lock();
        assert!(inner.super_menu.native_keyboard.owns_presentation(&inner));
        assert!(inner.super_menu.native_keyboard.binding.is_none());
        assert_eq!(inner.super_menu.native_keyboard.presentation.as_ref().unwrap().session_id, current.session_id);
    }

    #[test]
    fn native_super_menu_focused_restore_resumes_a_new_epoch_once_without_clipboard_input() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let old = f.host.take_native_super_menu_key().unwrap();
        f.host.use_focused_native_super_menu_input(&request).unwrap();
        assert!(f.host.resume_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss).is_err(),
            "plain focus mode does not authorize a global rebind");
        f.host.restore_native_super_menu_target(&request).unwrap();
        assert_eq!(f.inputs.load(Ordering::SeqCst), 1, "activation only, no selection copy/paste");
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        f.host.resume_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss).unwrap();
        assert!(!f.host.native_super_menu_key_is_current(&old));
        assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42), "old held key drains");
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
        let fresh = f.host.take_native_super_menu_key().unwrap();
        assert!(f.host.native_super_menu_key_is_current(&fresh));
        assert_eq!(fresh.scope(), &NativeSuperMenuKeyScope::Dismiss);
        assert!(f.host.resume_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::WindowHub).is_err(),
            "the handoff's resume authorization is single use");
        assert!(f.host.restore_native_super_menu_target(old.request()).is_err(), "taken keyboard identity cannot be upgraded");
    }

    #[test]
    fn native_super_menu_focused_restore_rejects_retired_session_and_foreground_round_trip() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::Dismiss);
        f.host.use_focused_native_super_menu_input(&request).unwrap();
        f.host.restore_native_super_menu_target(&request).unwrap();
        f.host.inner.lock().last_fg = 43;
        assert!(f.host.resume_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss).is_err());
        f.host.inner.lock().last_fg = 42;
        assert!(f.host.resume_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss).is_err(),
            "returning foreground cannot reuse a rejected handoff");
        f.host.super_menu_hide_session();
        let current = super_request(&f);
        let inputs = f.inputs.load(Ordering::SeqCst);
        assert!(f.host.restore_native_super_menu_target(&request).is_err());
        assert_eq!(f.inputs.load(Ordering::SeqCst), inputs, "old session cannot activate any window");
        f.host.restore_native_super_menu_target(&current).unwrap();
        f.host.resume_native_super_menu_keyboard(&current, NativeSuperMenuKeyScope::Dismiss).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn native_super_menu_focused_restore_raw_foreground_aba_cannot_resume_old_ticket() {
        use crate::active_window::{deliver_test_win_event, EVENT_SYSTEM_FOREGROUND};
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::Dismiss);
        f.host.use_focused_native_super_menu_input(&request).unwrap();
        crate::hooks::with_win_event_test_host(&f.host, || {
            f.host.restore_native_super_menu_target(&request).unwrap();
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 43);
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 42);
            f.host.inner.lock().last_fg = 42;
            assert!(f.host.resume_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss).is_err());
            assert!(f.host.inner.lock().super_menu.native_keyboard.binding.is_none());
            assert!(f.host.take_native_super_menu_key().is_none());
        });
    }

    #[test]
    fn native_super_menu_dismissed_focused_restore_is_one_use_and_requires_the_exact_hidden_surface() {
        for invalidation in ["none", "ordinary", "foreground", "new-session", "root", "target", "owner", "hide-again"] {
            let (f, mut request) = keyboard_fixture(NativeSuperMenuKeyScope::Dismiss);
            if invalidation != "ordinary" {
                f.host.use_focused_native_super_menu_input(&request).unwrap();
            }
            f.host.inner.lock().last_fg = 200;
            f.host.super_menu_hide_session();
            assert_eq!(f.host.inner.lock().super_menu.generation, request.generation.wrapping_add(1));
            match invalidation {
                "foreground" => f.host.inner.lock().last_fg = 43,
                "new-session" => { super_request(&f); }
                "root" => f.host.set_super_menu_hwnd(201),
                "target" => request.target = Some(InputTarget::capture(43).unwrap()),
                "owner" => request.owner = request.owner.wrapping_add(1),
                "hide-again" => f.host.super_menu_hide_session(),
                _ => {},
            }
            let result = f.host.restore_dismissed_native_super_menu_target(&request);
            assert_eq!(result.is_ok(), invalidation == "none", "{invalidation}");
            assert_eq!(f.inputs.load(Ordering::SeqCst), usize::from(invalidation == "none"), "{invalidation}");
            assert_eq!(f.writes.load(Ordering::SeqCst), 0, "{invalidation}");
            if invalidation == "none" {
                assert!(!f.host.inner.lock().super_menu.visible);
                assert!(f.host.restore_dismissed_native_super_menu_target(&request).is_err());
                assert_eq!(f.inputs.load(Ordering::SeqCst), 1, "a hide callback cannot activate twice");
            }
        }
    }

    #[test]
    fn native_super_menu_dismissed_focused_restore_rechecks_before_queued_activation() {
        for invalidation in ["foreground", "new-session"] {
            let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::Dismiss);
            f.host.use_focused_native_super_menu_input(&request).unwrap();
            f.host.inner.lock().last_fg = 200;
            f.host.super_menu_hide_session();
            let host = f.host.clone();
            f.host.set_input_runner(move |job| {
                if invalidation == "foreground" {
                    host.inner.lock().last_fg = 43;
                } else {
                    host.menu_create_session_ex("text", "", 200, 42);
                }
                job()
            });
            assert!(f.host.restore_dismissed_native_super_menu_target(&request).is_err(), "{invalidation}");
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_super_menu_text_quicker_launch_has_no_output_or_post_launch_clipboard_check() {
        for kind in [json!("QuickerAction"), json!(5)] {
            for simulate in [false, true] {
                let f = fixture();
                let _selection = crate::clipboard::install_test_snapshot(crate::clipboard::ClipboardSnapshot {
                    text: Some("captured action input".into()), ..Default::default()
                });
                let sequence = Arc::new(std::sync::atomic::AtomicU32::new(41));
                let _sequence = crate::selection_capture::install_test_sequence(sequence.clone());
                f.host.inner.lock().text_actions = vec![json!({"Type":kind, "Simulate":simulate,
                    "Data":{"Id":"synthetic-text-action"}})];
                let token = f.host.text_action_list()["actions"][0]["ruleToken"].as_str().unwrap().to_string();
                let request = super_request(&f);
                let launches = Arc::new(AtomicUsize::new(0));
                let called = launches.clone();
                f.host.set_shell_open_runner(move |_| {
                    called.fetch_add(1, Ordering::SeqCst);
                    sequence.store(42, Ordering::SeqCst);
                    Ok(())
                });
                assert_eq!(f.host.text_action_execute_native_checked("0", &token, &request).unwrap()["closeMenu"], true);
                assert_eq!(launches.load(Ordering::SeqCst), 1);
                assert_eq!(f.inputs.load(Ordering::SeqCst), 2, "activate and copy only, no old-selection re-paste");
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            }
        }
        let f = fixture();
        let _selection = crate::clipboard::install_test_snapshot(crate::clipboard::ClipboardSnapshot {
            text: Some("captured action input".into()), ..Default::default()
        });
        f.host.inner.lock().text_actions = vec![json!({"Type":"QuickerAction","Data":{"Id":"reject"}})];
        let token = f.host.text_action_list()["actions"][0]["ruleToken"].as_str().unwrap().to_string();
        let request = super_request(&f);
        f.host.set_shell_open_runner(|_| Err("synthetic launch rejected".into()));
        assert_eq!(f.host.text_action_execute_native_checked("0", &token, &request).unwrap_err(), "synthetic launch rejected");
        assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_super_menu_text_checked_snapshot_rejects_changed_rule_before_any_input() {
        for change in ["edited", "shifted", "deleted"] {
            let f = fixture();
            let first = json!({"Type":"Send","Title":"first","Data":{"Text":"one"}});
            let second = json!({"Type":"Send","Title":"second","Data":{"Text":"two"}});
            f.host.inner.lock().text_actions = vec![first, second];
            let token = f.host.text_action_list()["actions"][0]["ruleToken"].as_str().unwrap().to_string();
            let request = super_request(&f);
            {
                let mut inner = f.host.inner.lock();
                match change {
                    "edited" => inner.text_actions[0]["Data"]["Text"] = json!("changed"),
                    "shifted" => { inner.text_actions.remove(0); }
                    "deleted" => inner.text_actions.clear(),
                    _ => unreachable!(),
                }
            }
            assert!(f.host.text_action_execute_native_checked("0", &token, &request).is_err(), "{change}");
            assert_eq!(f.writes.load(Ordering::SeqCst), 0, "{change}");
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0, "{change}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_super_menu_keyboard_win_event_callback_keeps_background_restore_but_retires_raw_aba()
    {
        use crate::active_window::{
            deliver_test_win_event, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
        };
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let accepted = f.host.take_native_super_menu_key().unwrap();
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        crate::hooks::with_win_event_test_host(&f.host, || {
            // Actual production win_event_proc -> both registered hooks -> HostState.
            // The live foreground fixture remains A even when raw events contain B.
            deliver_test_win_event(EVENT_SYSTEM_MINIMIZEEND, 43);
            assert!(f.host.native_super_menu_key_is_current(&accepted));
            assert!(
                f.host.take_native_super_menu_key().unwrap().repeat(),
                "restore cannot clear inbox"
            );
            assert_eq!(crate::active_window::last_foreground_event_hwnd(), 43);
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 43);
            assert_eq!(
                f.host.inner.lock().last_fg,
                42,
                "live query alone already says A"
            );
            assert!(
                !f.host.native_super_menu_key_is_current(&accepted),
                "raw B retires before legacy refresh"
            );
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 42);
            assert!(f.host.take_native_super_menu_key().is_none());
            assert!(!f
                .host
                .native_super_menu_request_is_current(accepted.request()));
            assert!(f
                .host
                .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::WindowHub)
                .is_err());
            assert_eq!(crate::active_window::foreground_event_history(), [42, 43]);
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42));
            assert!(!f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        });
    }

    #[cfg(windows)]
    #[test]
    fn native_super_menu_keyboard_input_busy_raw_foreground_round_trip_drops_old_input() {
        use crate::active_window::{deliver_test_win_event, EVENT_SYSTEM_FOREGROUND};
        let (f, _) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let accepted = f.host.take_native_super_menu_key().unwrap();
        crate::hooks::with_win_event_test_host(&f.host, || {
            let owner = f.host.inner.lock();
            let started = Instant::now();
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 43);
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 42);
            assert!(started.elapsed() < Duration::from_millis(200));
            assert!(f.host.input_gap_pending());
            drop(owner);
            assert!(!f.host.native_super_menu_key_is_current(&accepted));
            f.host.poll_keyboard_work();
            assert!(f.host.take_native_super_menu_key().is_none());
            assert!(!f.host.dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42));
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        });
    }

    #[cfg(windows)]
    #[test]
    fn native_super_menu_keyboard_win_event_callback_history_root_is_not_keyboard_foreground() {
        let (f, _) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        f.host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42);
        let accepted = f.host.take_native_super_menu_key().unwrap();
        crate::hooks::with_win_event_test_host(&f.host, || {
            crate::active_window::deliver_test_win_event(
                crate::active_window::EVENT_SYSTEM_MINIMIZEEND,
                43,
            );
            // The legacy Explorer resolver can choose B from the mixed history.
            // Its resolved-root notification must retain keyboard ownership of A.
            f.host.on_foreground_hwnd(43);
            assert!(f
                .host
                .inner
                .lock()
                .super_menu
                .native_keyboard
                .binding
                .is_some());
            // Restore the independently injected live keyboard foreground A.
            f.host.inner.lock().last_fg = 42;
            f.host.notify_accessibility_foreground();
            assert!(f.host.native_super_menu_key_is_current(&accepted));
            // Poll/tray notification still notices a real live foreground loss.
            f.host.inner.lock().last_fg = 44;
            f.host.notify_accessibility_foreground();
            f.host.inner.lock().last_fg = 42;
            f.host.notify_accessibility_foreground();
            assert!(!f.host.native_super_menu_key_is_current(&accepted));
        });
    }

    #[test]
    fn native_super_menu_keyboard_scopes_match_web_product_keys() {
        for (scope, keys) in [
            (NativeSuperMenuKeyScope::Dismiss, vec![0x1B]),
            (
                NativeSuperMenuKeyScope::WindowHub,
                vec![0x1B, 0x25, 0x27, 0x0D],
            ),
            (
                NativeSuperMenuKeyScope::ClipboardHub,
                vec![0x1B, 0x26, 0x28, 0x0D],
            ),
            (
                NativeSuperMenuKeyScope::Value("opacity".into()),
                vec![0x1B, 0x25, 0x27, 0x24, 0x23],
            ),
        ] {
            for vk in [
                0x1B, 0x25, 0x27, 0x26, 0x28, 0x0D, 0x24, 0x23, 0x20, 0x31, 0x39, 0x41,
            ] {
                assert_eq!(
                    scope.key(vk).is_some(),
                    keys.contains(&vk),
                    "{scope:?} {vk}"
                );
            }
            let (f, _) = keyboard_fixture(scope);
            for vk in [0x20, 0x31, 0x41] {
                assert!(!f
                    .host
                    .dispatch_low_level_key_for_foreground(menu_key(vk, true), 42));
                assert!(!f
                    .host
                    .dispatch_low_level_key_for_foreground(menu_key(vk, false), 42));
            }
            let mut modified = menu_key(0x1B, true);
            modified.shift = true;
            assert!(!f.host.dispatch_low_level_key_for_foreground(modified, 42));
            f.host
                .dispatch_low_level_key_for_foreground(menu_key(0x1B, false), 42);
            assert!(f.host.take_native_super_menu_key().is_none());
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
            assert!(
                f.host.inner.lock().super_menu.visible,
                "hook only queues Escape"
            );
            assert_eq!(f.host.take_native_super_menu_key().unwrap().key(), "Escape");
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_super_menu_keyboard_preserves_paired_down_repeat_and_up() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::Value("opacity".into()));
        for down in [true, true, true, false] {
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, down), 42));
        }
        let first = f.host.take_native_super_menu_key().unwrap();
        let repeat = f.host.take_native_super_menu_key().unwrap();
        let second_repeat = f.host.take_native_super_menu_key().unwrap();
        let up = f.host.take_native_super_menu_key().unwrap();
        assert!(first.down() && !first.repeat());
        assert!(repeat.down() && repeat.repeat());
        assert!(second_repeat.down() && second_repeat.repeat());
        assert!(!up.down() && !up.repeat());
        assert!(f.host.native_super_menu_request_is_current(up.request()));
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42));
        f.host
            .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::WindowHub)
            .unwrap();
        for vk in [0x0D, 0x1B] {
            for down in [true, true, true, false] {
                assert!(f
                    .host
                    .dispatch_low_level_key_for_foreground(menu_key(vk, down), 42));
            }
            let down = f.host.take_native_super_menu_key().unwrap();
            let up = f.host.take_native_super_menu_key().unwrap();
            assert!(down.down() && !down.repeat());
            assert!(!up.down());
            assert!(
                f.host.take_native_super_menu_key().is_none(),
                "execution is once per press"
            );
        }
    }

    #[test]
    fn native_super_menu_keyboard_receiver_free_binding_and_qp_takeover_are_separate() {
        let f = fixture();
        {
            let mut inner = f.host.inner.lock();
            inner.ui.panel_visible = false;
            inner.window_visible = false;
            inner.last_fg = 42;
        }
        let (request, _) = menu_fixture(&f, "text", Default::default(), 0);
        assert!(request.target.is_none());
        f.host
            .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss)
            .unwrap();
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
        let accepted = f.host.take_native_super_menu_key().unwrap();
        f.host.inner.lock().qp.overlay_visible = true;
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
        assert!(!f.host.native_super_menu_key_is_current(&accepted));
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x1B, false), 42));
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(f.host.inner.lock().super_menu.visible);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_super_menu_keyboard_qp_escape_and_completed_input_layer_do_not_revive_menu_keys() {
        let (f, _) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        {
            let mut inner = f.host.inner.lock();
            inner.qp.machine.begin_mouse_session();
            inner.qp.machine.on_session_ui_ready();
            inner.qp.overlay_visible = true;
        }
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
        assert!(
            f.host.take_native_super_menu_key().is_none(),
            "Escape belongs to QP"
        );
        assert!(
            f.host.inner.lock().super_menu.visible,
            "must not close the background menu"
        );
        {
            let inner = f.host.inner.lock();
            assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
            assert!(!inner.qp.overlay_visible, "Escape cancels before the worker runs");
            assert!(inner.qp.pending_commands.is_empty());
            assert!(inner.qp.escape_down);
        }
        for down in [true, false] {
            assert!(f.host.dispatch_low_level_key_for_foreground(menu_key(0x1B, down), 42),
                "QP owns the accepted Escape repeat and release");
        }
        {
            let inner = f.host.inner.lock();
            assert!(!inner.qp.escape_down);
            assert!(inner.super_menu.visible, "held Escape must not close the background menu");
        }
        assert!(f.host.take_native_super_menu_key().is_none());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);

        for layer in ["context", "incoming"] {
            let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            let accepted = f.host.take_native_super_menu_key().unwrap();
            if layer == "context" {
                f.host.inner.lock().ui.main_visible = true;
                assert!(f.host.acquire_native_context_menu(42, "main"));
                assert!(f.host.release_native_context_menu(42));
            } else {
                f.host.inner.lock().ui.panel_visible = true;
                let drag = f.host.acquire_native_incoming_drag("panel", 100).unwrap();
                assert!(f.host.release_native_incoming_drag(drag));
            }
            assert!(
                !f.host.native_super_menu_key_is_current(&accepted),
                "{layer}"
            );
            assert!(
                f.host
                    .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::WindowHub)
                    .is_err(),
                "{layer}"
            );
        }
    }

    #[test]
    fn native_super_menu_keyboard_retirement_drains_held_press_only() {
        for cause in [
            "hide",
            "root",
            "generation",
            "runtime",
            "drag",
            "foreground",
            "registry",
            "target",
        ] {
            let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            let event = f.host.take_native_super_menu_key().unwrap();
            match cause {
                "hide" => {
                    f.host.super_menu_hide_session();
                }
                "root" => f.host.set_super_menu_hwnd(201),
                "generation" => {
                    f.host
                        .menu_create_session_loading("text", "", 200, 42, |_, _| {
                            Default::default()
                        });
                }
                "runtime" => f.host.set_window_runtime_ready(false),
                "drag" => f.host.inner.lock().ole_drag_active = true,
                "foreground" => {
                    f.host.notify_native_super_menu_foreground(43);
                    f.host.notify_native_super_menu_foreground(42);
                }
                "registry" => {
                    f.host.menus.close(&request.session_id);
                }
                "target" => f
                    .host
                    .menus
                    .set_session_handles(&request.session_id, 200, 43),
                _ => unreachable!(),
            }
            assert!(!f.host.native_super_menu_key_is_current(&event), "{cause}");
            assert!(
                f.host
                    .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42),
                "repeat {cause}"
            );
            assert!(
                f.host
                    .dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42),
                "up {cause}"
            );
            assert!(
                f.host.take_native_super_menu_key().is_none(),
                "queue {cause}"
            );
            assert!(
                !f.host
                    .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42),
                "fresh {cause}"
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_super_menu_keyboard_foreground_return_cannot_rebind_original_presentation() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x0D, true), 42));
        let accepted = f.host.take_native_super_menu_key().unwrap();
        f.host.notify_native_super_menu_foreground(43);
        f.host.notify_native_super_menu_foreground(42);
        assert!(f
            .host
            .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::WindowHub)
            .is_err());
        assert!(!f
            .host
            .native_super_menu_request_is_current(accepted.request()));
        assert!(f
            .host
            .native_super_menu_close_reply_is_current(accepted.request()));
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x0D, false), 42));
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x0D, true), 42));
        assert!(f.host.take_native_super_menu_key().is_none());
        // A new session is the only way to claim this foreground again.
        let next = super_request(&f);
        f.host
            .bind_native_super_menu_keyboard(&next, NativeSuperMenuKeyScope::WindowHub)
            .unwrap();
        f.host
            .dispatch_low_level_key_for_foreground(menu_key(0x0D, false), 42);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x0D, true), 42));
        assert!(!f
            .host
            .native_super_menu_close_reply_is_current(accepted.request()));
    }

    #[test]
    fn native_super_menu_keyboard_scope_change_retires_taken_keys_and_value_commits() {
        let (f, request) = keyboard_fixture(NativeSuperMenuKeyScope::Value("A".into()));
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x25, true), 42));
        let old = f.host.take_native_super_menu_key().unwrap();
        f.host
            .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Value("B".into()))
            .unwrap();
        assert!(!f.host.native_super_menu_key_is_current(&old));
        assert!(!f.host.native_super_menu_request_is_current(old.request()));
        assert!(f
            .host
            .simulate_super_menu_clip(&f.clip, old.request())
            .is_err());
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x25, false), 42));
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x25, true), 42));
        assert_eq!(
            f.host.take_native_super_menu_key().unwrap().scope(),
            &NativeSuperMenuKeyScope::Value("B".into())
        );
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_super_menu_keyboard_does_not_acquire_preexisting_held_key_or_background_panel() {
        let f = fixture();
        {
            let mut inner = f.host.inner.lock();
            inner.ui.panel_visible = false;
            inner.window_visible = false;
            inner.has_host_fake_focus = false;
            inner.last_fg = 42;
        }
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x25, true), 42));
        let request = super_request(&f);
        f.host
            .bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::Dismiss)
            .unwrap();
        f.host.inner.lock().ui.panel_visible = true;
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x25, true), 42));
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x25, false), 42));
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(f.host.take_chrome_task().is_some()); // only the menu's existing Show
        while let Some(task) = f.host.take_chrome_task() {
            assert!(!matches!(
                task.command,
                ChromeCommand::PanelNavKey(_) | ChromeCommand::ClipboardNavKey { .. }
            ));
        }
    }

    #[test]
    fn native_super_menu_keyboard_overflow_and_fresh_down_drop_stale_drain() {
        let (f, _) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        for _ in 0..33 {
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42));
        }
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let (f, _) = keyboard_fixture(NativeSuperMenuKeyScope::WindowHub);
        f.host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42);
        f.host
            .inner
            .lock()
            .super_menu
            .native_keyboard
            .retire_presentation();
        // Simulate a release tracked outside this owned press's route.
        f.host.inner.lock().keyboard_keys_down[0x27] = false;
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        assert!(f
            .host
            .inner
            .lock()
            .super_menu
            .native_keyboard
            .held
            .is_empty());
    }

    #[test]
    fn native_super_menu_keyboard_untouched_tauri_route_uses_hwnd_without_opt_in() {
        let f = fixture();
        f.host.inner.lock().ui.panel_visible = false;
        f.host.inner.lock().window_visible = false;
        let _ = super_request(&f);
        f.host.set_super_menu_hwnd(200);
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        assert!(f.host.take_native_super_menu_key().is_none());
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x1B, true), 42));
        assert!(!f.host.inner.lock().super_menu.visible);
        assert!(!f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x1B, false), 42));
    }

    fn menu_fixture(
        f: &Fixture,
        action: &str,
        mut context: crate::menu_api::MenuBuildContext,
        target: i64,
    ) -> (NativeSuperMenuPasteRequest, Value) {
        context.source_window_handle = 200;
        context.target_window_handle = target;
        let created = f
            .host
            .menus
            .create_from_context(action, json!({}), &context);
        let session = created["sessionId"].as_str().unwrap();
        let generation = {
            let mut inner = f.host.inner.lock();
            inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
            inner.super_menu.visible = true;
            inner.super_menu.session_id = Some(session.to_string());
            inner.super_menu.snapshot = Some(created.clone());
            inner.super_menu.generation
        };
        let request = f
            .host
            .capture_native_super_menu_action(session, generation, 200)
            .unwrap();
        (request, created)
    }

    fn root_role(created: &Value, role: &str) -> String {
        created["root"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["role"] == role)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn value_menu_fixture(host: &HostState, target: i64) -> (NativeSuperMenuRequest, String) {
        // The production registry/outer session are used, with a synthetic HWND
        // under headless. Building a real window root would enumerate desktop
        // windows, which this isolated regression deliberately never does.
        let context = crate::menu_api::MenuBuildContext {
            source_window_handle: 200,
            target_window_handle: target,
            ..Default::default()
        };
        let mut node =
            crate::menu_api::command("不透明度", "window-opacity", json!({"windowHandle":42}));
        node.kind = "Value".into();
        node.value = Some((26.0, 255.0, 1.0, 255.0));
        node.keep_open_on_invoke = true;
        let node_id = node.id.clone();
        let created = host.menus.create_fixture_session(&context, vec![node]);
        let session = created["sessionId"].as_str().unwrap();
        let generation = {
            let mut inner = host.inner.lock();
            inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
            inner.super_menu.visible = true;
            inner.super_menu.session_id = Some(session.into());
            inner.super_menu.snapshot = Some(created.clone());
            inner.super_menu.generation
        };
        (
            host.capture_native_super_menu_action(session, generation, 200)
                .unwrap(),
            node_id,
        )
    }

    #[test]
    fn native_menu_value_commits_without_receiver_and_does_not_retire_its_ticket() {
        let f = fixture();
        let (request, node) = value_menu_fixture(&f.host, 0);
        assert!(request.target.is_none());
        assert!(f.host.native_super_menu_request_is_current(&request));
        let before = f.host.super_menu_state_detail();
        let config_before = f.host.config_get();
        for value in [120.0, 180.0] {
            let result = f
                .host
                .menu_set_value_native(
                    &json!({"nodeId":node,"value":value,
                "sessionId":"unrelated-session","sourceWindowHandle":999,"targetWindowHandle":555}),
                    &request,
                )
                .unwrap();
            assert_eq!(result["closeMenu"], false);
            assert_eq!(result["refresh"], "Node");
            assert_eq!(result["patchedItem"]["id"], node);
            assert_eq!(result["patchedItem"]["value"]["current"], value);
            assert_eq!(
                f.host
                    .menus
                    .node(&request.session_id, &node)
                    .unwrap()
                    .value
                    .unwrap()
                    .3,
                value
            );
            f.host.verify_super_menu_lifecycle(&request).unwrap();
            assert!(f.host.native_super_menu_request_is_current(&request));
        }
        assert_eq!(
            f.host.super_menu_state_detail()["generation"],
            before["generation"]
        );
        assert_eq!(
            f.host.super_menu_state_detail()["sessionId"],
            before["sessionId"]
        );
        assert!(f.host.inner.lock().super_menu.visible);
        assert_eq!(f.host.menu_session_target_handle(&request.session_id), 0);
        assert_eq!(f.host.config_get(), config_before);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 2); // opacity jobs, no target activation or keys
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_menu_value_rejects_original_ticket_after_node_read_or_queue_retirement() {
        for cancellation in ["read", "queue", "hide-reopen", "target", "hwnd"] {
            let f = fixture();
            let (request, node) = value_menu_fixture(&f.host, 0);
            let executed = Arc::new(AtomicUsize::new(0));
            let sent = executed.clone();
            let host = f.host.clone();
            if cancellation == "queue" {
                f.host.set_input_runner(move |job| {
                    host.super_menu_hide_session();
                    job()?;
                    sent.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            } else {
                f.host.set_input_runner(move |job| {
                    job()?;
                    sent.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            }
            let result = f.host.menu_set_value_native_after_node(
                &json!({"nodeId":node,"value":120.0}),
                &request,
                || match cancellation {
                    "read" => {
                        f.host.super_menu_hide_session();
                    }
                    "hide-reopen" => {
                        f.host.super_menu_hide_session();
                        value_menu_fixture(&f.host, 0);
                    }
                    "target" => {
                        f.host
                            .menus
                            .set_session_handles(&request.session_id, 200, 77);
                    }
                    "hwnd" => {
                        f.host.set_super_menu_hwnd(201);
                    }
                    _ => {}
                },
            );
            assert_eq!(
                result.unwrap_err(),
                SUPER_MENU_INPUT_RETIRED,
                "{cancellation}"
            );
            assert!(
                !f.host.native_super_menu_request_is_current(&request),
                "{cancellation}"
            );
            assert_eq!(executed.load(Ordering::SeqCst), 0, "{cancellation}");
            if let Ok(original) = f.host.menus.node(&request.session_id, &node) {
                assert_eq!(original.value.unwrap().3, 255.0);
            }
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_menu_value_rechecks_before_patch_without_touching_replacement_menu() {
        for cancellation in ["hide-reopen", "target", "hwnd"] {
            let f = fixture();
            let (request, node) = value_menu_fixture(&f.host, 0);
            let host = f.host.clone();
            let session = request.session_id.clone();
            let replacement = Arc::new(Mutex::new(None::<(NativeSuperMenuRequest, String)>));
            let published = replacement.clone();
            let executed = Arc::new(AtomicUsize::new(0));
            let sent = executed.clone();
            f.host.set_input_runner(move |job| {
                job()?;
                sent.fetch_add(1, Ordering::SeqCst);
                match cancellation {
                    "hide-reopen" => {
                        host.super_menu_hide_session();
                        *published.lock() = Some(value_menu_fixture(&host, 0));
                    }
                    "target" => host.menus.set_session_handles(&session, 200, 77),
                    _ => host.set_super_menu_hwnd(201),
                }
                Ok(())
            });
            assert_eq!(
                f.host
                    .menu_set_value_native(&json!({"nodeId":node,"value":120.0}), &request)
                    .unwrap_err(),
                SUPER_MENU_INPUT_RETIRED,
                "{cancellation}"
            );
            assert_eq!(executed.load(Ordering::SeqCst), 1);
            if let Some((new_request, new_node)) = replacement.lock().as_ref() {
                assert_eq!(
                    f.host
                        .menus
                        .node(&new_request.session_id, new_node)
                        .unwrap()
                        .value
                        .unwrap()
                        .3,
                    255.0
                );
                assert_eq!(
                    f.host.super_menu_state_detail()["sessionId"],
                    new_request.session_id
                );
            } else {
                assert_eq!(
                    f.host
                        .menus
                        .node(&request.session_id, &node)
                        .unwrap()
                        .value
                        .unwrap()
                        .3,
                    255.0
                );
            }
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_menu_value_scope_does_not_replace_the_hosts_input_dispatcher() {
        let f = fixture();
        let (request, node) = value_menu_fixture(&f.host, 0);
        let host = f.host.clone();
        let passed = Arc::new(AtomicUsize::new(0));
        let completed = passed.clone();
        let first = Arc::new(AtomicBool::new(true));
        f.host.set_input_runner(move |job| {
            if first.swap(false, Ordering::SeqCst) {
                host.super_menu_hide_session();
            }
            job()?;
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(f
            .host
            .menu_set_value_native(&json!({"nodeId":node,"value":120.0}), &request)
            .is_err());
        assert!(!f.host.native_super_menu_request_is_current(&request));
        assert_eq!(passed.load(Ordering::SeqCst), 0);
        f.host.run_input(|| Ok(())).unwrap();
        assert_eq!(passed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn native_menu_value_root_keeps_web_noop_without_receiver_or_retiring_menu() {
        let f = fixture();
        let (request, _) = menu_fixture(&f, "clipboard", Default::default(), 0);
        assert!(request.target.is_none());
        let before = f.host.super_menu_state_detail();
        let result = f
            .host
            .menu_set_value_native(
                &json!({"nodeId":"root","value":180.0,
            "sessionId":"must-not-retarget","sourceWindowHandle":999,"targetWindowHandle":555}),
                &request,
            )
            .unwrap();
        assert_eq!(result["closeMenu"], false);
        assert_eq!(result["refresh"], "None");
        assert!(result["patchedItem"].is_null());
        assert_eq!(
            f.host.super_menu_state_detail()["generation"],
            before["generation"]
        );
        assert!(f.host.inner.lock().super_menu.visible);
        assert_eq!(f.host.menu_session_target_handle(&request.session_id), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        f.host.verify_super_menu_lifecycle(&request).unwrap();
    }

    #[test]
    fn native_menu_value_original_ticket_cannot_survive_read_retirement_or_wrong_owner() {
        let f = fixture();
        let (request, _) = menu_fixture(&f, "clipboard", Default::default(), 0);
        let result = f.host.menu_set_value_native_after_node(
            &json!({"nodeId":"root","value":120.0}),
            &request,
            || {
                f.host.super_menu_hide_session();
                menu_fixture(&f, "clipboard", Default::default(), 0);
            },
        );
        assert_eq!(result.unwrap_err(), SUPER_MENU_INPUT_RETIRED);
        let other = fixture();
        assert_eq!(
            other
                .host
                .menu_set_value_native(&json!({"nodeId":"root","value":120.0}), &request)
                .unwrap_err(),
            SUPER_MENU_INPUT_RETIRED
        );
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_panel_old_ticket_cannot_survive_hide_reopen_or_new_document() {
        for retirement in [
            "hide",
            "document",
            "root",
            "modal",
            "directory",
            "clipboard",
        ] {
            let f = fixture();
            let request = f.host.capture_native_panel_paste(None).unwrap();
            match retirement {
                "hide" => {
                    f.host.apply_panel_hide();
                    f.host.panel_show(&json!({"favorId":f.folder})).unwrap();
                }
                "document" => {
                    f.host.set_panel_web_view_ready(false);
                    f.host.set_panel_web_view_ready(true);
                }
                "root" => f.host.set_panel_hwnd(101),
                "modal" => {
                    let mut inner = f.host.inner.lock();
                    drop_queued_panel_navigation(&mut inner);
                }
                "directory" => f.host.inner.lock().ui.panel_folder_id = Some("new-folder".into()),
                _ => {
                    let mut inner = f.host.inner.lock();
                    inner.ui.keyboard_focus_host = Some("clipboard");
                }
            }
            assert!(
                f.host
                    .favor_paste_panel_native(&json!({"id":f.favor}), &request)
                    .is_err(),
                "{retirement}"
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_panel_rechecks_lifecycle_after_read_and_allows_new_foreground() {
        for cancel in [false, true] {
            let f = fixture();
            let request = f.host.capture_native_panel_paste(None).unwrap();
            assert_eq!(request.target.unwrap().hwnd(), 42);
            f.host.inner.lock().last_non_host_fg = 43;
            let mut checks = 0;
            let result = f.host.favor_paste_panel_native_for_foreground(
                &json!({"id":f.favor}),
                &request,
                || {
                    checks += 1;
                    if cancel && checks == 2 {
                        f.host.apply_panel_hide();
                    }
                    if checks == 1 {
                        42
                    } else {
                        43
                    }
                },
            );
            assert_eq!(result.is_err(), cancel);
            assert_eq!(f.writes.load(Ordering::SeqCst), usize::from(!cancel));
            assert_eq!(f.inputs.load(Ordering::SeqCst), if cancel { 0 } else { 2 });
            assert_eq!(
                f.host
                    .store()
                    .lock()
                    .get_usage(&crate::store::favor_usage_key(&f.favor))
                    .unwrap()
                    .usage_count,
                0
            );
        }
    }

    #[test]
    fn native_panel_open_text_tracks_usage_only_after_success() {
        for action in ["Copy", "Paste", "Open"] {
            let f = fixture();
            f.host.inner.lock().extras.panel.file_default_click_action = action.into();
            let request = f.host.capture_native_panel_paste(None).unwrap();
            f.host.inner.lock().last_non_host_fg = 43;
            let result = f
                .host
                .panel_invoke_native(
                    &json!({"favorId":f.folder,"itemId":f.favor,"sourceHandle":999}),
                    &request,
                )
                .unwrap();
            assert_eq!(result["action"], "executed");
            assert_eq!(f.writes.load(Ordering::SeqCst), 1);
            assert_eq!(
                f.inputs.load(Ordering::SeqCst),
                if action == "Copy" { 0 } else { 2 }
            );
            assert_eq!(
                f.host
                    .store()
                    .lock()
                    .get_usage(&crate::store::favor_usage_key(&f.favor))
                    .unwrap()
                    .usage_count,
                i32::from(action == "Open")
            );
        }
    }

    #[test]
    fn native_panel_action_and_steps_default_open_paste_without_action_launch_or_usage() {
        for kind in [ClipKind::ActionClipItem, ClipKind::StepClipItem] {
            let f = fixture();
            f.host.inner.lock().extras.panel.file_default_click_action = "Open".into();
            let store = f.host.store();
            let mut item = store.lock().get_favor(&f.favor).unwrap();
            item.kind = kind;
            item.body_text = Some(if kind == ClipKind::ActionClipItem {
                r#"{"Id":"native-panel-action","Title":"action"}"#.into()
            } else {
                "<steps>native panel steps</steps>".into()
            });
            store.lock().save_favor(&item).unwrap();
            let launches = Arc::new(AtomicUsize::new(0));
            let launched = launches.clone();
            f.host.set_shell_open_runner(move |_| {
                launched.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            let request = f.host.capture_native_panel_action(None).unwrap();
            assert_eq!(
                f.host
                    .panel_invoke_native(&json!({"favorId":f.folder,"itemId":f.favor}), &request)
                    .unwrap()["action"],
                "executed"
            );
            assert_eq!(launches.load(Ordering::SeqCst), 0);
            assert_eq!(f.writes.load(Ordering::SeqCst), 1);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
            assert_eq!(
                store
                    .lock()
                    .get_usage(&crate::store::favor_usage_key(&f.favor))
                    .unwrap()
                    .usage_count,
                0
            );
        }
    }

    #[test]
    fn native_panel_file_default_open_only_shell_opens_first_file_without_input_or_usage() {
        for target in [0, 42] {
            let f = fixture();
            {
                let mut inner = f.host.inner.lock();
                inner.last_non_host_fg = target;
                inner.extras.panel.file_default_click_action = "Open".into();
            }
            let first = f
                ._root
                .path()
                .join("first.txt")
                .to_string_lossy()
                .into_owned();
            let second = f
                ._root
                .path()
                .join("second.txt")
                .to_string_lossy()
                .into_owned();
            std::fs::write(&first, b"first").unwrap();
            std::fs::write(&second, b"second").unwrap();
            let store = f.host.store();
            let mut item = store.lock().get_favor(&f.favor).unwrap();
            item.kind = ClipKind::FileClipItem;
            item.file_paths = vec![first.clone(), second];
            store.lock().save_favor(&item).unwrap();
            let launches = Arc::new(Mutex::new(Vec::new()));
            let launched = launches.clone();
            f.host.set_shell_open_runner(move |path| {
                launched.lock().push(path.to_string());
                Ok(())
            });
            let request = f.host.capture_native_panel_action(None).unwrap();
            f.host
                .panel_invoke_native(&json!({"favorId":f.folder,"itemId":f.favor}), &request)
                .unwrap();
            assert_eq!(&*launches.lock(), &[first]);
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
            assert_eq!(
                store
                    .lock()
                    .get_usage(&crate::store::favor_usage_key(&f.favor))
                    .unwrap()
                    .usage_count,
                0
            );
        }
    }

    #[test]
    fn native_panel_queued_clipboard_commit_checks_lifetime() {
        let f = fixture();
        let request = f.host.capture_native_panel_paste(None).unwrap();
        let host = f.host.clone();
        f.host.set_clipboard_ui_runner(move |job| {
            host.apply_panel_hide();
            job()
        });
        assert!(f
            .host
            .favor_paste_panel_native(&json!({"id":f.favor}), &request)
            .is_err());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn native_panel_non_input_actions_work_without_an_external_target() {
        let f = fixture();
        f.host.inner.lock().last_non_host_fg = 0;
        assert!(f.host.capture_native_panel_paste(None).is_err());
        let request = f.host.capture_native_panel_action(None).unwrap();
        assert!(request.target.is_none());
        f.host.inner.lock().extras.panel.file_default_click_action = "Copy".into();
        let params = json!({"favorId":f.folder,"itemId":f.favor});
        assert_eq!(
            f.host.panel_invoke_native(&params, &request).unwrap()["action"],
            "executed"
        );
        assert_eq!(
            f.host
                .favor_copy_panel_native(&json!({"id":f.favor}), &request)
                .unwrap()["ok"],
            true
        );
        f.host.inner.lock().extras.panel.folder_default_click_action = "Open".into();
        assert_eq!(
            f.host
                .panel_invoke_native(&json!({"favorId":f.folder,"itemId":f.folder}), &request)
                .unwrap()["navigateFolderId"],
            f.folder
        );
        for action in ["Paste", "Open"] {
            f.host.inner.lock().extras.panel.file_default_click_action = action.into();
            assert_eq!(
                f.host.panel_invoke_native(&params, &request).unwrap_err(),
                "找不到可接收输入的目标窗口。"
            );
        }
        let store = f.host.store();
        let mut link = store.lock().get_favor(&f.favor).unwrap();
        link.kind = ClipKind::LinkClipItem;
        link.body_text = Some("https://example.invalid/native-panel".into());
        store.lock().save_favor(&link).unwrap();
        let opens = Arc::new(AtomicUsize::new(0));
        let opened = opens.clone();
        f.host.set_shell_open_runner(move |_| {
            opened.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert_eq!(
            f.host.panel_invoke_native(&params, &request).unwrap()["action"],
            "executed"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        assert_eq!(f.writes.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .lock()
                .get_usage(&crate::store::favor_usage_key(&f.favor))
                .unwrap()
                .usage_count,
            0
        );
    }

    #[test]
    fn native_panel_copy_without_target_rechecks_owner_queue_lifetime() {
        let f = fixture();
        f.host.inner.lock().last_non_host_fg = 0;
        let request = f.host.capture_native_panel_action(None).unwrap();
        let host = f.host.clone();
        f.host.set_clipboard_ui_runner(move |job| {
            host.apply_panel_hide();
            job()
        });
        assert!(f
            .host
            .favor_copy_panel_native(&json!({"id":f.favor}), &request)
            .is_err());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_panel_page_commit_is_atomic_and_retires_old_navigation() {
        let f = fixture();
        let generation = f.host.panel_native_snapshot()["presentationGeneration"]
            .as_u64()
            .unwrap();
        f.host.enqueue_chrome(ChromeCommand::PanelNavKey("Enter"));
        let old = f.host.take_chrome_task().unwrap();
        let request = f.host.capture_native_panel_paste(Some(old)).unwrap();
        assert!(f
            .host
            .apply_native_panel_page(generation, 100, true, "next-folder"));
        assert!(!f.host.chrome_task_input_allowed(old));
        assert!(f
            .host
            .favor_paste_panel_native(&json!({"id":f.favor}), &request)
            .is_err());
        f.host.request_host_keyboard_focus("panel");
        assert_eq!(
            f.host.panel_native_snapshot()["presentationGeneration"],
            generation,
            "focus must not retire the visible page"
        );
        assert!(f
            .host
            .apply_native_panel_page(generation, 100, true, "next-folder"));
        f.host.apply_panel_hide();
        f.host.panel_show(&json!({"favorId":f.folder})).unwrap();
        assert!(!f
            .host
            .apply_native_panel_page(generation, 100, false, "stale-folder"));
        assert_ne!(f.host.panel_native_snapshot()["folderId"], "stale-folder");
        let current = f.host.panel_native_snapshot()["presentationGeneration"]
            .as_u64()
            .unwrap();
        f.host.set_panel_hwnd(101);
        assert!(!f
            .host
            .apply_native_panel_page(current, 100, false, "old-root"));
    }

    #[test]
    fn native_super_menu_allows_new_foreground_without_changing_registry_target() {
        let f = fixture();
        let request = super_request(&f);
        assert_eq!(request.target.unwrap().hwnd(), 42);
        f.host.inner.lock().last_non_host_fg = 43;
        assert_eq!(
            f.host
                .paste_super_menu_clip_for_foreground(&f.clip, &request, || 43)
                .unwrap()["ok"],
            true
        );
        assert_eq!(f.writes.load(Ordering::SeqCst), 1);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 2);
        assert_eq!(
            f.host
                .menus
                .with_session(&request.session_id, |registry| {
                    Ok(registry.session_target_handle(&request.session_id))
                })
                .unwrap(),
            42
        );
        assert_eq!(
            f.host.store().lock().get_clip(&f.clip).unwrap().usage_count,
            1
        );
    }

    #[test]
    fn native_super_menu_retirement_or_target_patch_rejects_old_ticket() {
        for retirement in [
            "hide",
            "registry",
            "target",
            "root",
            "new-session",
        ] {
            let f = fixture();
            let request = super_request(&f);
            match retirement {
                "hide" => {
                    f.host.super_menu_hide_session();
                    f.host.menu_create_session_ex("text", "", 200, 42);
                }
                "registry" => {
                    f.host.menus.close(&request.session_id);
                }
                "target" => f
                    .host
                    .menus
                    .set_session_handles(&request.session_id, 200, 43),
                "root" => f.host.set_super_menu_hwnd(201),
                "new-session" => {
                    f.host.menu_create_session_ex("text", "", 200, 43);
                }
                _ => {}
            }
            assert!(
                f.host
                    .paste_super_menu_clip_for_foreground(&f.clip, &request, || 43)
                    .is_err(),
                "{retirement}"
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
            assert_eq!(
                f.host.store().lock().get_clip(&f.clip).unwrap().usage_count,
                0
            );
        }
    }

    #[test]
    fn native_selection_copy_still_requires_original_foreground() {
        crate::clipboard::force_headless(true);
        let target = InputTarget::capture(42).unwrap();
        assert!(verify_surface_foreground(200, target, 42).is_ok());
        assert!(verify_surface_foreground(200, target, 200).is_ok());
        assert!(verify_surface_foreground(200, target, 43).is_err());
    }

    #[test]
    fn native_super_menu_rechecks_after_history_and_owner_queue() {
        for cancellation in ["read", "clipboard", "input"] {
            let f = fixture();
            let request = super_request(&f);
            let host = f.host.clone();
            if cancellation == "clipboard" {
                f.host.set_clipboard_ui_runner(move |job| {
                    host.super_menu_hide_session();
                    job()
                });
            } else if cancellation == "input" {
                let calls = Arc::new(AtomicUsize::new(0));
                f.host.set_input_runner(move |job| {
                    if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                        host.super_menu_hide_session();
                    }
                    job()
                });
            }
            let mut checks = 0;
            let result = f
                .host
                .paste_super_menu_clip_for_foreground(&f.clip, &request, || {
                    checks += 1;
                    if cancellation == "read" && checks == 2 {
                        f.host.super_menu_hide_session();
                    }
                    42
                });
            assert!(result.is_err(), "{cancellation}");
            if cancellation != "input" {
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            }
            assert_eq!(
                f.host.store().lock().get_clip(&f.clip).unwrap().usage_count,
                0
            );
        }
    }

    #[test]
    fn native_super_menu_simulate_is_guarded_without_quick_paste_state() {
        for retire in [false, true] {
            let f = fixture();
            let request = super_request(&f);
            if retire {
                f.host.super_menu_hide_session();
            }
            assert_eq!(
                f.host.simulate_super_menu_clip(&f.clip, &request).is_err(),
                retire
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(
                f.host.store().lock().get_clip(&f.clip).unwrap().usage_count,
                i32::from(!retire)
            );
            assert!(f.host.inner.lock().qp.last_simulated_text.is_none());
        }
    }

    #[test]
    fn native_menu_text_node_retains_registry_target_and_ignores_handle_overrides() {
        for simulate in [false, true] {
            let f = fixture();
            let action =
                json!({"Type":"Send","Data":{"Text":"native menu output"},"Simulate":simulate});
            f.host.inner.lock().text_actions = vec![action.clone()];
            let context = crate::menu_api::MenuBuildContext {
                text_actions: vec![action],
                ..Default::default()
            };
            let (request, created) = menu_fixture(&f, "text", context, 42);
            f.host.inner.lock().last_non_host_fg = 43;
            let node = root_role(&created, "text");
            assert_eq!(
                f.host
                    .menu_invoke_native(
                        &node,
                        &json!({"sourceWindowHandle":999,"targetWindowHandle":43}),
                        &request
                    )
                    .unwrap()["closeMenu"],
                true
            );
            assert_eq!(f.host.menu_session_target_handle(&request.session_id), 42);
            assert_eq!(f.writes.load(Ordering::SeqCst), usize::from(!simulate));
            assert!(f.host.inner.lock().qp.last_simulated_text.is_none());
        }
    }

    #[test]
    fn native_menu_node_rechecks_after_registry_read_and_queued_input() {
        for cancellation in ["read", "input", "clipboard"] {
            let f = fixture();
            let action = json!({"Type":"Send","Data":{"Text":"cancelled output"}});
            f.host.inner.lock().text_actions = vec![action.clone()];
            let context = crate::menu_api::MenuBuildContext {
                text_actions: vec![action],
                ..Default::default()
            };
            let (request, created) = menu_fixture(&f, "text", context, 42);
            let node = root_role(&created, "text");
            let host = f.host.clone();
            if cancellation == "input" {
                f.host.set_input_runner(move |job| {
                    host.super_menu_hide_session();
                    job()
                });
            } else if cancellation == "clipboard" {
                f.host.set_clipboard_ui_runner(move |job| {
                    host.super_menu_hide_session();
                    job()
                });
            }
            let result = f.host.menu_invoke_native_after_node(&node, &request, || {
                if cancellation == "read" {
                    f.host.super_menu_hide_session();
                    f.host.menu_create_session_ex("text", "", 200, 42);
                }
            });
            assert!(result.is_err(), "{cancellation}");
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_menu_copy_node_works_without_target_and_does_not_replace_global_runners() {
        let f = fixture();
        let leaf = crate::bookmarks::BookmarkNode {
            id: "bookmark".into(),
            title: "native bookmark".into(),
            url: "https://example.invalid/native-menu".into(),
            children: Vec::new(),
        };
        let context = crate::menu_api::MenuBuildContext {
            bookmarks: Some(crate::bookmarks::BookmarkNode {
                id: "root".into(),
                title: "root".into(),
                url: String::new(),
                children: vec![leaf],
            }),
            ..Default::default()
        };
        let (request, created) = menu_fixture(&f, "bookmarks", context, 0);
        assert!(request.target.is_none());
        let bookmark = root_role(&created, "bookmark");
        let context = f
            .host
            .menus
            .get_nodes(
                &request.session_id,
                &bookmark,
                crate::menu_api::CollectionKind::ContextMenu,
                None,
            )
            .unwrap();
        let copy = context["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["role"] == "bookmark-copy")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        let host = f.host.clone();
        f.host.set_clipboard_ui_runner(move |job| {
            host.super_menu_hide_session();
            job()
        });
        assert!(f
            .host
            .menu_invoke_native(copy, &json!({}), &request)
            .is_err());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        // The original host still owns its original unguarded dispatcher.
        assert!(f.host.favor_copy(&json!({"id":f.favor})).is_ok());
    }

    #[test]
    fn native_menu_selection_rechecks_after_read_without_followup_input() {
        let f = fixture();
        let request = super_request(&f);
        let result =
            f.host
                .capture_native_menu_selection(request.target.unwrap(), &request, || {
                    f.host.super_menu_hide_session();
                    Some("selected content")
                });
        assert!(result.is_err());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.inputs.load(Ordering::SeqCst), 1);
    }

    fn window_batch_menu_fixture(
        host: &HostState,
        close: bool,
    ) -> (NativeSuperMenuRequest, String) {
        let context = crate::menu_api::MenuBuildContext {
            source_window_handle: 200,
            target_window_handle: 0,
            ..Default::default()
        };
        let role = if close { "window-close-class" } else { "window-kill-all" };
        let node = crate::menu_api::command("owned batch", role,
            json!({"className":"OwnedClass", "processName":"owned", "windowHandle":501}));
        let node_id = node.id.clone();
        // Never build a live window root: that would enumerate desktop HWNDs.
        let created = host.menus.create_fixture_session(&context, vec![node]);
        let session = created["sessionId"].as_str().unwrap();
        let generation = {
            let mut inner = host.inner.lock();
            inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
            inner.super_menu.visible = true;
            inner.super_menu.session_id = Some(session.into());
            inner.super_menu.snapshot = Some(created.clone());
            inner.super_menu.generation
        };
        (host.capture_native_super_menu_action(session, generation, 200).unwrap(), node_id)
    }

    fn retire_window_batch_menu(host: &HostState, request: &NativeSuperMenuRequest, retirement: &str) {
        match retirement {
            "root" => host.set_super_menu_hwnd(201),
            "registry-target" => host.menus.set_session_handles(&request.session_id, 200, 99),
            "registry-close" => { host.menu_close_session(&request.session_id); },
            _ => {
                host.super_menu_hide_session();
                // Reopen on the same root without capturing any replacement
                // request. The old request must remain cancelled forever.
                let context = crate::menu_api::MenuBuildContext {
                    source_window_handle: 200,
                    target_window_handle: 0,
                    ..Default::default()
                };
                let created = host.menus.create_fixture_session(&context, Vec::new());
                let mut inner = host.inner.lock();
                inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
                inner.super_menu.visible = true;
                inner.super_menu.session_id = created["sessionId"].as_str().map(str::to_string);
                inner.super_menu.snapshot = Some(created);
            },
        }
    }

    struct WindowBatchStubRun {
        result: Result<Value, String>,
        enumerated: usize,
        prepared: usize,
        executed: usize,
        released: usize,
    }

    fn invoke_stubbed_window_batch(
        f: &Fixture,
        close: bool,
        native: bool,
        phase: &'static str,
        retirement: &'static str,
    ) -> WindowBatchStubRun {
        use crate::window_menu::{GuardedSystemActionError, WindowBatchTestAction};
        struct OwnedResource(Arc<AtomicUsize>);
        impl Drop for OwnedResource { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }
        let (request, node) = window_batch_menu_fixture(&f.host, close);
        assert!(request.target.is_none(), "system batch must not require an input receiver");
        let enumerated = Arc::new(AtomicUsize::new(0));
        let prepared = Arc::new(AtomicUsize::new(0));
        let executed = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let read_count = enumerated.clone();
        let prepare_count = prepared.clone();
        let execute_count = executed.clone();
        let release_count = released.clone();
        let host = f.host.clone();
        let original = request.clone();
        let action = if close { WindowBatchTestAction::CloseClass } else { WindowBatchTestAction::KillName };
        let result = crate::window_menu::with_window_batch_test_runner(action, move |name, verify| {
            assert_eq!(name, if close { "OwnedClass" } else { "owned" });
            let enumerate = || {
                read_count.fetch_add(1, Ordering::SeqCst);
                if phase.starts_with("enumeration") { retire_window_batch_menu(&host, &original, retirement); }
            };
            let execute = |_: u32| {
                crate::window_menu::execute_prepared_system_action_guarded(&|| verify(), || {
                    prepare_count.fetch_add(1, Ordering::SeqCst);
                    if phase == "prepare" { retire_window_batch_menu(&host, &original, retirement); }
                    Ok(OwnedResource(release_count.clone()))
                }, |_| {
                    let completed = execute_count.fetch_add(1, Ordering::SeqCst);
                    if phase == "first" && completed == 0 { retire_window_batch_menu(&host, &original, retirement); }
                    Ok(())
                })
            };
            if close {
                crate::window_menu::close_all_by_class_name_with(name, &|| verify(), || {
                    enumerate();
                    if phase == "enumeration-empty" { Vec::new() } else {
                        [501, 502].into_iter().map(|hwnd| crate::window_menu::WindowInfo {
                            hwnd, class_name:"OwnedClass".into(), ..Default::default()
                        }).collect()
                    }
                }, |hwnd| execute(hwnd as u32).map_err(|error| match error {
                    GuardedSystemActionError::Cancelled(error) | GuardedSystemActionError::Operation(error) => error,
                }))
            } else {
                crate::window_menu::kill_all_by_process_name_with(name, &|| verify(), || {
                    enumerate();
                    Ok(if phase == "enumeration-empty" { Vec::new() } else { vec![(501,"owned.exe".into()),(502,"OWNED.EXE".into())] })
                }, execute)
            }
        }, || {
            if native {
                f.host.menu_invoke_native(&node, &json!({"sourceWindowHandle":999,"targetWindowHandle":999}), &request)
            } else {
                f.host.menu_invoke(&request.session_id, &node, &json!({}))
            }
        });
        WindowBatchStubRun {
            result,
            enumerated: enumerated.load(Ordering::SeqCst),
            prepared: prepared.load(Ordering::SeqCst),
            executed: executed.load(Ordering::SeqCst),
            released: released.load(Ordering::SeqCst),
        }
    }

    #[test]
    fn native_menu_window_batches_recheck_retirement_after_enumeration_even_empty() {
        for close in [false, true] {
            for phase in ["enumeration", "enumeration-empty"] {
                let f = fixture();
                let run = invoke_stubbed_window_batch(&f, close, true, phase, "hide-reopen");
                assert_eq!(run.result.unwrap_err(), SUPER_MENU_INPUT_RETIRED);
                assert_eq!((run.enumerated, run.prepared, run.executed, run.released), (1,0,0,0));
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
                assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn native_menu_window_batches_stop_after_first_action_on_original_ticket_retirement() {
        for close in [false, true] {
            for retirement in ["hide-reopen", "root", "registry-target", "registry-close"] {
                let f = fixture();
                let run = invoke_stubbed_window_batch(&f, close, true, "first", retirement);
                assert_eq!(run.result.unwrap_err(), SUPER_MENU_INPUT_RETIRED, "{retirement}");
                assert_eq!((run.enumerated, run.prepared, run.executed, run.released), (1,1,1,1), "{retirement}");
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
                assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn native_menu_window_batches_recheck_after_prepare_before_system_action_and_release() {
        for close in [false, true] {
            let f = fixture();
            let run = invoke_stubbed_window_batch(&f, close, true, "prepare", "hide-reopen");
            assert_eq!(run.result.unwrap_err(), SUPER_MENU_INPUT_RETIRED);
            assert_eq!((run.enumerated, run.prepared, run.executed, run.released), (1,1,0,1));
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn native_menu_window_batches_preserve_current_native_and_web_success_rules() {
        for close in [false, true] {
            for native in [false, true] {
                let f = fixture();
                let run = invoke_stubbed_window_batch(&f, close, native, "success", "hide-reopen");
                let result = run.result.unwrap();
                assert_eq!(result["closeMenu"], true);
                assert_eq!(result["refresh"], "None");
                assert_eq!((run.enumerated, run.prepared, run.executed, run.released), (1,2,2,2));
                assert!(f.host.inner.lock().super_menu.visible);
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
                assert_eq!(f.inputs.load(Ordering::SeqCst), 0);
            }
        }
    }
}

// Shared pure ledger routing with an already-owned input guard.
impl HostState {
    pub(super) fn input_dispatch_native_super_menu_held_key(
        &self, inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        suspended: bool,
        repeat: bool,
    ) -> bool {
        let incoming_drag_generation = inner.native_incoming_drag_next_id;
        let allowed = !suspended
            && inner.window_runtime_ready
            && inner.native_context_menu.is_none()
            && !inner.qp.overlay_visible
            && inner.super_menu.native_keyboard.owns_presentation(&inner);
        let keyboard = &mut inner.super_menu.native_keyboard;
        keyboard.observe_incoming_drag(incoming_drag_generation);
        keyboard.observe_foreground(foreground);
        if !allowed && keyboard.binding.is_some() {
            keyboard.retire_binding();
        }
        keyboard.route_held(ev, foreground, allowed, repeat)
        }

    pub(super) fn input_route_native_super_menu_key(
        &self, inner: &mut HostInner,
        ev: KeyEvent,
        foreground: isize,
        repeat: bool,
    ) -> Option<bool> {
        if !inner.super_menu.native_keyboard.owns_presentation(&inner) {
            return None;
        }
        let incoming_drag_generation = inner.native_incoming_drag_next_id;
        let keyboard = &mut inner.super_menu.native_keyboard;
        keyboard.observe_incoming_drag(incoming_drag_generation);
        keyboard.observe_foreground(foreground);
        Some(keyboard.accept_down(ev, repeat))
        }
}
