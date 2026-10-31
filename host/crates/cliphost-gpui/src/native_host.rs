//! GPUI window adapter for the existing ClipHost state machine. Capture,
//! keyboard ownership, hide decisions and paste targets remain in HostState.

use std::time::{Duration, Instant};

use cliphost_core::{
    host::{ChromeTask, HostState},
    i18n::tr,
    ChromeCommand,
};
use serde_json::json;

use crate::platform::{self, Geometry};

#[path = "native_activation.rs"]
mod activation;
use activation::ActivationObservation;

pub struct NativeWindow {
    hwnd: isize,
    host: HostState,
    geometry: Option<Geometry>,
    foreground: isize,
    activation: ActivationObservation<(isize, u64)>,
    motion: Option<EdgeMotion>,
    last_hide_poll: Option<Instant>,
    initialized: bool,
    posted_work_pending: bool,
}

struct EdgeMotion {
    generation: u64,
    tucked: bool,
    frames: Vec<(f64, f64)>,
    started: Instant,
}

impl NativeWindow {
    /// Register before explicit Show; attachment never captures a paste target,
    /// starts another host, or raises/activates a window by itself.
    pub fn attach(window: &mut gpui::Window, host: &HostState) -> Result<Self, String> {
        #[cfg(windows)]
        let hwnd = platform::hwnd(window)?.0 as isize;
        #[cfg(not(windows))]
        let hwnd = 0;
        cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd);
        let drag_host = host.clone();
        cliphost_core::window_chrome::set_caption_drag_observer(hwnd, move |moving| {
            if moving {
                drag_host.arm_caption_drag_interaction(false);
            } else {
                drag_host.end_window_move();
            }
        });
        platform::configure_host(window)?;
        platform::configure_clipboard_movement(window)?;
        let adapter = Self {
            hwnd,
            host: host.clone(),
            geometry: None,
            foreground: cliphost_core::window_menu::foreground_hwnd(),
            activation: ActivationObservation::new(
                host.native_clipboard_activation_stamp(),
                cliphost_core::window_chrome::is_active(hwnd),
            ),
            motion: None,
            last_hide_poll: None,
            initialized: false,
            posted_work_pending: true,
        };
        host.set_host_hwnd(hwnd);
        host.set_clipboard_web_view_ready(false);
        host.set_window_runtime_ready(false);
        platform::set_no_activate(window, host.clipboard_no_activate())?;
        {
            let (position, size) = host
                .restored_host_window_rect("clipboard")
                .unwrap_or((None, None));
            let scale = f64::from(window.scale_factor());
            let position = position.map(|position| match position {
                cliphost_core::window_state::WindowPosition::Physical(x, y) => (x, y),
                cliphost_core::window_state::WindowPosition::Logical(x, y) => (
                    (f64::from(x) * scale).round() as i32,
                    (f64::from(y) * scale).round() as i32,
                ),
            });
            platform::set_clipboard_geometry(
                window,
                position,
                size.map(|(w, h)| (f64::from(w), f64::from(h)))
                    .unwrap_or_else(|| host.clipboard_window_size(false)),
                false,
            )?;
        }
        // GPUI show=false defers its initial placement. Do not persist its
        // CW_USEDEFAULT HWND size before our posted placement has actually run.
        Ok(adapter)
    }

    /// Run on the owning UI thread. Return stamped tasks for immediate view or
    /// secondary-window handling; the consumer rechecks prepare_chrome_task.
    pub fn poll(
        &mut self,
        window: &mut gpui::Window,
        host: &HostState,
        content_drag_active: bool,
    ) -> Result<Vec<ChromeTask>, String> {
        #[cfg(windows)]
        if platform::hwnd(window)?.0 as isize != self.hwnd || host.host_hwnd() != self.hwnd {
            return Err(tr(
                "主窗口句柄已被替换，旧任务已取消。",
                "Native host HWND was replaced; old tasks have been retired.",
            )
            .into());
        }
        let error_stamp = host.native_clipboard_activation_stamp();
        if let Some(error) = platform::take_error(window)? {
            // A failed Show/focus cannot leave invisible list input engaged.
            // The same core Hide retires pending input and presentation epochs.
            host.hide_native_clipboard_if_current(error_stamp);
            return Err(error);
        }
        if !self.initialized {
            if platform::has_pending(window)? {
                return Ok(Vec::new());
            }
            self.initialized = true;
            host.set_clipboard_web_view_ready(true);
            host.set_window_runtime_ready(true);
            self.remember_geometry(window, host)?;
        }
        let foreground = cliphost_core::window_menu::foreground_hwnd();
        if foreground != self.foreground {
            self.foreground = foreground;
            host.notify_accessibility_foreground();
        }
        let active = cliphost_core::window_chrome::is_active(self.hwnd);
        let stamp = host.native_clipboard_activation_stamp();
        if let Some(active) = self.activation.observe(stamp, active) {
            host.notify_native_clipboard_activation(stamp, active);
        }
        let now = Instant::now();
        if self
            .last_hide_poll
            .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(50))
            || host.next_native_ui_deadline().is_some_and(|deadline| deadline <= now)
        {
            // Content thumb drags keep native mouse capture outside the HWND.
            // Preserve the existing drag-blocks-auto-dismiss policy without
            // changing focus/overlay ownership or delaying explicit Hide tasks.
            if !content_drag_active {
                host.poll_hide();
            }
            self.last_hide_poll = Some(now);
        }
        if let Some(no_activate) = host.take_clipboard_no_activate_update() {
            let style_host = host.clone();
            let _permission = platform::guard_condition_inherited(move || {
                style_host.clipboard_no_activate() == no_activate
            });
            platform::set_no_activate(window, no_activate)?;
        }
        let mut ui = Vec::new();
        for _ in 0..128 {
            let Some(task) = host.take_chrome_task() else {
                break;
            };
            let command = task.command();
            if !native_command(command) {
                ui.push(task);
                continue;
            }
            if !host.prepare_chrome_task(task) {
                continue;
            }
            let _permission = platform::guard_task(host, task);
            match command {
                ChromeCommand::Hide => {
                    self.motion = None;
                    platform::hide(window)?;
                }
                ChromeCommand::Show => {
                    self.motion = None;
                    let collapsed = host.hide_snapshot().is_collapsed;
                    platform::set_no_activate(window, host.clipboard_no_activate())?;
                    let size = host.clipboard_window_size(collapsed);
                    let position = host.place_main_window_logical(size.0, size.1).or_else(|| {
                        host.edge_show_restore_position()
                            .map(|(x, y)| (x as i32, y as i32))
                    });
                    platform::set_clipboard_geometry(window, position, size, collapsed)?;
                    if host.chrome_command_lifecycle_allowed(command) {
                        platform::show(
                            window,
                            host.chrome_task_explicit_presentation(task),
                            host.should_activate_on_show(),
                        )?;
                    }
                }
                ChromeCommand::Fold | ChromeCommand::Unfold => {
                    self.motion = None;
                    let collapsed = command == ChromeCommand::Fold;
                    platform::set_clipboard_geometry(
                        window,
                        None,
                        host.clipboard_window_size(collapsed),
                        collapsed,
                    )?;
                    platform::show(window, false, false)?;
                }
                ChromeCommand::EdgeHide | ChromeCommand::RestoreEdge => {
                    let tucked = command == ChromeCommand::EdgeHide;
                    let target = if tucked {
                        host.edge_tuck_position()
                    } else {
                        host.edge_show_restore_position()
                    };
                    if let Some(target) = target {
                        self.begin_edge(window, host, target, tucked)?;
                    } else if tucked {
                        host.cancel_edge_tuck();
                    }
                }
                ChromeCommand::BeginCaptionDrag("clipboard") => {
                    Self::begin_move(window, host)?;
                }
                _ => unreachable!("native command classification"),
            }
        }
        self.advance_edge(window, host)?;
        self.posted_work_pending = platform::has_pending(window)?;
        if !self.posted_work_pending {
            self.remember_geometry(window, host)?;
        }
        Ok(ui)
    }

    /// Hidden, settled HWNDs need no tick. Posted placement/show/style work
    /// must finish before sleeping, even when a Hide retires a pending Show.
    pub fn poll_interval(&self, active: bool) -> Option<Duration> {
        if self.motion.is_some() {
            Some(Duration::from_millis(10))
        } else if active || !self.initialized || self.posted_work_pending {
            Some(Duration::from_millis(50))
        } else {
            None
        }
    }

    pub fn ready_for_maintenance(&self) -> bool {
        self.initialized
    }

    /// Invoke from a deferred GPUI callback, after input-event borrows end.
    pub fn request_search_focus(window: &mut gpui::Window, host: &HostState) -> Result<(), String> {
        host.chrome_web_action(&json!({"kind":"clipboardHostChrome", "route":host.native_clipboard_route(),
            "sourceHostWindow":"clipboard", "isClipboardEditing":false, "requestHostKeyboardFocus":true}));
        if !host.clipboard_keyboard_focus_allowed() {
            return Err(tr(
                "剪贴板输入会话已结束，已取消焦点交接。",
                "Clipboard input ownership has ended; focus handoff was cancelled.",
            )
            .into());
        }
        let focus_host = host.clone();
        let stamp = host.native_clipboard_activation_stamp();
        // Search activation also belongs to the caller's current operation;
        // a retired parent request must not regain focus through this handoff.
        let _permission =
            platform::guard_condition_inherited(move || {
                focus_host.native_clipboard_activation_stamp() == stamp
                    && focus_host.clipboard_keyboard_focus_allowed()
            });
        platform::set_no_activate(window, false)?;
        platform::activate(window)?;
        Ok(())
    }

    pub fn restore_browsing(window: &gpui::Window, host: &HostState) -> Result<(), String> {
        host.chrome_web_action(
            &json!({"kind":"clipboardHostChrome", "route":host.native_clipboard_route(),
            "sourceHostWindow":"clipboard", "isClipboardEditing":false}),
        );
        platform::set_no_activate(window, host.clipboard_no_activate())
    }

    /// The shared core posts WM_NCLBUTTONDOWN, avoiding synchronous GPUI input
    /// reentry. It verifies a still-held mouse press hitting this owned root.
    pub fn begin_move(window: &gpui::Window, host: &HostState) -> Result<(), String> {
        #[cfg(windows)]
        let hwnd = platform::hwnd(window)?.0 as isize;
        #[cfg(not(windows))]
        let hwnd = {
            let _ = window;
            0
        };
        if hwnd != host.host_hwnd() {
            return Err(tr(
                "标题栏拖动属于已失效的原生窗口。",
                "Caption drag belongs to a retired native window.",
            )
            .into());
        }
        platform::begin_move(window, host)
    }

    fn remember_geometry(&mut self, window: &gpui::Window, host: &HostState) -> Result<(), String> {
        let geometry = platform::geometry(window)?;
        if self.geometry != Some(geometry) && geometry.width > 0 && geometry.height > 0 {
            host.remember_physical_host_window_rect(
                "clipboard",
                geometry.left,
                geometry.top,
                geometry.width,
                geometry.height,
                geometry.scale,
            );
            self.geometry = Some(geometry);
        }
        Ok(())
    }

    fn begin_edge(
        &mut self,
        window: &gpui::Window,
        host: &HostState,
        target: (f64, f64),
        tucked: bool,
    ) -> Result<(), String> {
        let Some(generation) = host.begin_clipboard_animation(tucked) else {
            return Ok(());
        };
        let geometry = platform::geometry(window)?;
        self.motion = Some(EdgeMotion {
            generation,
            tucked,
            frames: cliphost_core::edge_hide::animation_frames(
                (f64::from(geometry.left), f64::from(geometry.top)),
                target,
            ),
            started: Instant::now(),
        });
        Ok(())
    }

    fn advance_edge(&mut self, window: &gpui::Window, host: &HostState) -> Result<(), String> {
        let Some(motion) = &self.motion else {
            return Ok(());
        };
        if !host.clipboard_animation_is_current(motion.generation, motion.tucked) {
            self.motion = None;
            return Ok(());
        }
        let fraction = motion.started.elapsed().as_millis() as f64
            / cliphost_core::edge_hide::ANIMATION_MS as f64;
        let index = ((motion.frames.len() - 1) as f64 * fraction.min(1.0)) as usize;
        let (x, y) = motion.frames[index];
        let frame_host = host.clone();
        let generation = motion.generation;
        let tucked = motion.tucked;
        let _permission = platform::guard_condition(move || {
            frame_host.clipboard_animation_is_current(generation, tucked)
        });
        platform::set_geometry(window, Some((x as i32, y as i32)), None)?;
        if index + 1 == motion.frames.len() {
            self.motion = None;
        }
        Ok(())
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        if self.host.host_hwnd() == self.hwnd {
            self.host.retire_window_presentation("clipboard");
            self.host.set_clipboard_web_view_ready(false);
            self.host.set_window_runtime_ready(false);
            self.host.set_host_hwnd(0);
            self.host.end_window_move();
        }
    }
}

fn native_command(command: ChromeCommand) -> bool {
    matches!(
        command,
        ChromeCommand::Hide
            | ChromeCommand::Show
            | ChromeCommand::Fold
            | ChromeCommand::Unfold
            | ChromeCommand::EdgeHide
            | ChromeCommand::RestoreEdge
            | ChromeCommand::BeginCaptionDrag("clipboard")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_adapter_never_consumes_ui_input_or_secondary_surface_tasks() {
        assert!(native_command(ChromeCommand::Show));
        assert!(native_command(ChromeCommand::BeginCaptionDrag("clipboard")));
        for command in [
            ChromeCommand::RouteDigit {
                digit: 1,
                repeat: false,
            },
            ChromeCommand::ClipboardActivateSearch,
            ChromeCommand::ClipboardHandleEscape,
            ChromeCommand::ShowQuickPaste,
            ChromeCommand::ShowMain,
            ChromeCommand::BeginCaptionDrag("main"),
            ChromeCommand::Exit,
        ] {
            assert!(!native_command(command));
        }
    }
}
