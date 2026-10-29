//! Confirm the destination before injecting input. Paste may follow a different
//! foreground application; fixed-window operations retain their captured target.
//! Activation is asynchronous, so never infer success from the request alone.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputTarget {
    hwnd: isize,
    process_id: u32,
}

#[derive(Clone, Copy, Default)]
struct WindowSnapshot {
    exists: bool,
    visible: bool,
    minimized: bool,
    process_id: u32,
    foreground: isize,
}

trait TargetWindows {
    fn snapshot(&mut self, hwnd: isize) -> WindowSnapshot;
    fn request_foreground(&mut self, hwnd: isize) -> bool;
    fn await_activation(&mut self, hwnd: isize) -> bool;
}

impl InputTarget {
    /// The destination captured for a deferred native input request.
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn capture(hwnd: isize) -> Result<Self, String> {
        if crate::clipboard::is_headless() {
            return Ok(Self {
                hwnd,
                process_id: 0,
            });
        }
        let target = Self::from_snapshot(hwnd, NativeWindows.snapshot(hwnd), std::process::id())?;
        #[cfg(all(feature = "native-acceptance", not(test)))]
        crate::native_acceptance::verify_target_process(target.process_id)?;
        Ok(target)
    }

    fn from_snapshot(hwnd: isize, state: WindowSnapshot, own_pid: u32) -> Result<Self, String> {
        if hwnd == 0 || !state.exists || !state.visible || state.process_id == 0 {
            return Err("找不到可接收输入的目标窗口。".into());
        }
        if state.process_id == own_pid {
            return Err("尚未切回目标应用，已停止输入。".into());
        }
        Ok(Self {
            hwnd,
            process_id: state.process_id,
        })
    }

    pub fn activate(&self) -> Result<(), String> {
        if crate::clipboard::is_headless() {
            return Ok(());
        }
        self.activate_with(&mut NativeWindows)
    }

    /// Paste follows the application the user has brought to the foreground.
    /// If our own surface still owns foreground, return to the captured app.
    pub fn activate_for_paste(&self) -> Result<(), String> {
        self.resolve_for_paste().map(|_| ())
    }

    /// Recheck deferred paste input without treating a different application
    /// as cancellation. Fixed-window operations keep using `verify` instead.
    pub fn verify_for_paste(&self) -> Result<(), String> {
        self.resolve_for_paste().map(|_| ())
    }

    pub(crate) fn resolve_for_paste(&self) -> Result<Self, String> {
        if crate::clipboard::is_headless() {
            return Ok(*self);
        }
        let target = self.resolve_for_paste_with(&mut NativeWindows, std::process::id())?;
        #[cfg(all(feature = "native-acceptance", not(test)))]
        crate::native_acceptance::verify_target_process(target.process_id)?;
        Ok(target)
    }

    fn resolve_for_paste_with(
        &self,
        windows: &mut impl TargetWindows,
        own_pid: u32,
    ) -> Result<Self, String> {
        // Foreground can change between querying its HWND and inspecting that
        // window. Retake a bounded snapshot rather than reject a normal handoff.
        for _ in 0..3 {
            let captured = windows.snapshot(self.hwnd);
            let foreground = captured.foreground;
            if foreground == 0 {
                return Err("找不到可接收输入的目标窗口。".into());
            }
            let current = if foreground == self.hwnd {
                captured
            } else {
                windows.snapshot(foreground)
            };
            if current.foreground != foreground {
                continue;
            }
            let host_foreground = current.exists && current.process_id == own_pid;
            let target = if host_foreground {
                // Never restore a captured handle that now belongs to this host.
                if self.process_id == own_pid {
                    return Err("找不到可接收输入的目标窗口。".into());
                }
                *self
            } else {
                Self::from_snapshot(foreground, current, own_pid)?
            };
            if host_foreground || current.minimized {
                if let Err(error) = target.activate_with(windows) {
                    let after = windows.snapshot(target.hwnd).foreground;
                    if after != 0 && after != foreground && after != target.hwnd {
                        // The user selected another application while our own
                        // surface was handing foreground back to the target.
                        continue;
                    }
                    return Err(error);
                }
            } else {
                target.verify_snapshot(current)?;
            }
            return Ok(target);
        }
        Err("找不到可接收输入的目标窗口。".into())
    }

    fn activate_with(&self, windows: &mut impl TargetWindows) -> Result<(), String> {
        let before = windows.snapshot(self.hwnd);
        self.validate_window(before)?;
        if before.foreground == self.hwnd && !before.minimized {
            return Ok(());
        }
        // Activation can still be queued when the Win32 call returns. Confirm
        // the actual destination within the same bounded wait in either case.
        let requested = windows.request_foreground(self.hwnd);
        if !windows.await_activation(self.hwnd) {
            let after = windows.snapshot(self.hwnd);
            tracing::debug!(
                target_hwnd = self.hwnd,
                target_pid = self.process_id,
                before_foreground = before.foreground,
                after_foreground = after.foreground,
                requested,
                visible = after.visible,
                minimized = after.minimized,
                "Paste target activation did not complete"
            );
            return Err("目标窗口未及时完成激活，已停止输入。".into());
        }
        self.verify_snapshot(windows.snapshot(self.hwnd))
    }

    pub fn verify(&self) -> Result<(), String> {
        if crate::clipboard::is_headless() {
            return Ok(());
        }
        self.verify_snapshot(NativeWindows.snapshot(self.hwnd))
    }

    /// Check the accepted HWND/PID for non-input operations that must not
    /// activate a different foreground application merely to inspect it.
    pub fn verify_identity(&self) -> Result<(), String> {
        if crate::clipboard::is_headless() {
            return Ok(());
        }
        self.validate_window(NativeWindows.snapshot(self.hwnd))
    }

    fn validate_window(&self, state: WindowSnapshot) -> Result<(), String> {
        if !state.exists || !state.visible || state.process_id != self.process_id {
            return Err("目标窗口已关闭或发生变化，已停止输入。".into());
        }
        Ok(())
    }

    fn verify_snapshot(&self, state: WindowSnapshot) -> Result<(), String> {
        self.validate_window(state)?;
        if state.foreground != self.hwnd || state.minimized {
            return Err("目标窗口没有获得焦点或已切换，已停止输入。".into());
        }
        Ok(())
    }
}

pub fn belongs_to_host(hwnd: isize) -> bool {
    let state = NativeWindows.snapshot(hwnd);
    state.exists && state.process_id == std::process::id()
}

struct NativeWindows;

#[cfg(windows)]
impl TargetWindows for NativeWindows {
    fn snapshot(&mut self, hwnd: isize) -> WindowSnapshot {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
        unsafe {
            let handle = HWND(hwnd as *mut _);
            let mut process_id = 0;
            GetWindowThreadProcessId(handle, Some(&mut process_id));
            WindowSnapshot {
                exists: IsWindow(handle).as_bool(),
                visible: IsWindowVisible(handle).as_bool(),
                minimized: IsIconic(handle).as_bool(),
                process_id,
                foreground: GetForegroundWindow().0 as isize,
            }
        }
    }

    fn request_foreground(&mut self, hwnd: isize) -> bool {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
        unsafe {
            let handle = HWND(hwnd as *mut _);
            // Do not restore an already maximized target to its normal size.
            // Async restore also avoids waiting indefinitely for another app.
            if IsIconic(handle).as_bool() {
                let _ = ShowWindowAsync(handle, SW_RESTORE);
            }
            SetForegroundWindow(handle).as_bool()
        }
    }

    fn await_activation(&mut self, hwnd: isize) -> bool {
        use windows::Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            UI::WindowsAndMessaging::*,
        };
        // A WM_NULL response alone does not prove that activation has completed.
        // Wait for the actual foreground state, sharing one 200ms deadline with
        // message probes; never retry SetForegroundWindow or attach input queues.
        // https://devblogs.microsoft.com/oldnewthing/20161118-00/?p=94745
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let handle = HWND(hwnd as *mut _);
        loop {
            unsafe {
                if GetForegroundWindow() == handle && !IsIconic(handle).as_bool() {
                    return true;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                if SendMessageTimeoutW(
                    handle,
                    WM_NULL,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    remaining.as_millis().max(1) as u32,
                    None,
                )
                .0 == 0
                {
                    tracing::debug!(
                        target_hwnd = hwnd,
                        foreground = GetForegroundWindow().0 as isize,
                        error = windows::Win32::Foundation::GetLastError().0,
                        remaining_ms = deadline
                            .saturating_duration_since(std::time::Instant::now())
                            .as_millis(),
                        "Paste target message probe failed"
                    );
                    return false;
                }
            }
            std::thread::sleep(
                std::time::Duration::from_millis(2)
                    .min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
        }
    }
}

#[cfg(not(windows))]
impl TargetWindows for NativeWindows {
    fn snapshot(&mut self, _: isize) -> WindowSnapshot {
        WindowSnapshot::default()
    }
    fn request_foreground(&mut self, _: isize) -> bool {
        false
    }
    fn await_activation(&mut self, _: isize) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeWindows {
        state: WindowSnapshot,
        other_windows: std::collections::BTreeMap<isize, WindowSnapshot>,
        foreground_snapshots: std::collections::VecDeque<isize>,
        snapshots: usize,
        allow_activation: bool,
        responsive: bool,
        foreground_after_wait: isize,
        requests: usize,
        waits: usize,
    }

    impl TargetWindows for FakeWindows {
        fn snapshot(&mut self, hwnd: isize) -> WindowSnapshot {
            self.snapshots += 1;
            if let Some(foreground) = self.foreground_snapshots.pop_front() {
                self.state.foreground = foreground;
            }
            self.other_windows.get(&hwnd).map_or(self.state, |state| WindowSnapshot {
                foreground: self.state.foreground,
                ..*state
            })
        }
        fn request_foreground(&mut self, _: isize) -> bool {
            self.requests += 1;
            self.allow_activation
        }
        fn await_activation(&mut self, hwnd: isize) -> bool {
            self.waits += 1;
            self.state.foreground = self.foreground_after_wait;
            self.state.minimized = false;
            if let Some(state) = self.other_windows.get_mut(&hwnd) {
                state.minimized = false;
            }
            self.responsive
        }
    }

    fn fixture() -> (InputTarget, FakeWindows) {
        let state = WindowSnapshot {
            exists: true,
            visible: true,
            minimized: false,
            process_id: 7,
            foreground: 20,
        };
        (
            InputTarget::from_snapshot(10, state, 42).unwrap(),
            FakeWindows {
                state,
                other_windows: Default::default(),
                foreground_snapshots: Default::default(),
                snapshots: 0,
                allow_activation: true,
                responsive: true,
                foreground_after_wait: 10,
                requests: 0,
                waits: 0,
            },
        )
    }

    #[test]
    fn activation_waits_for_target_queue_and_preserves_an_already_active_window() {
        let (target, mut windows) = fixture();
        assert!(target.activate_with(&mut windows).is_ok());
        assert_eq!((windows.requests, windows.waits), (1, 1));
        assert!(target.activate_with(&mut windows).is_ok());
        assert_eq!((windows.requests, windows.waits), (1, 1));
    }

    #[test]
    fn denied_timed_out_or_stolen_activation_does_not_succeed() {
        for failure in ["denied", "timeout", "switched"] {
            let (target, mut windows) = fixture();
            match failure {
                "denied" => {
                    windows.allow_activation = false;
                    windows.foreground_after_wait = 20;
                }
                "timeout" => windows.responsive = false,
                _ => windows.foreground_after_wait = 30,
            }
            assert!(target.activate_with(&mut windows).is_err(), "{failure}");
            assert_eq!(windows.requests, 1);
            assert_eq!(windows.waits, 1);
        }
    }

    #[test]
    fn delayed_activation_is_confirmed_even_when_the_request_returned_false() {
        let (target, mut windows) = fixture();
        windows.allow_activation = false;
        assert!(target.activate_with(&mut windows).is_ok());
        assert_eq!((windows.requests, windows.waits), (1, 1));
    }

    #[test]
    fn foreground_window_still_minimized_must_be_restored_before_input() {
        let (target, mut windows) = fixture();
        windows.state.foreground = 10;
        windows.state.minimized = true;
        assert!(target.verify_snapshot(windows.state).is_err());
        assert!(target.activate_with(&mut windows).is_ok());
        assert_eq!((windows.requests, windows.waits), (1, 1));
    }

    #[test]
    fn closed_hidden_and_reused_handles_do_not_receive_activation() {
        for failure in ["closed", "hidden", "reused"] {
            let (target, mut windows) = fixture();
            match failure {
                "closed" => windows.state.exists = false,
                "hidden" => windows.state.visible = false,
                _ => windows.state.process_id = 8,
            }
            assert!(target.activate_with(&mut windows).is_err(), "{failure}");
            assert_eq!(windows.requests, 0);
            assert!(target.verify_snapshot(windows.state).is_err());
        }
    }

    #[test]
    fn capture_rejects_missing_and_host_windows() {
        let (_, windows) = fixture();
        assert!(InputTarget::from_snapshot(0, windows.state, 42).is_err());
        assert!(InputTarget::from_snapshot(10, WindowSnapshot::default(), 42).is_err());
        assert!(InputTarget::from_snapshot(10, windows.state, 7).is_err());
    }

    #[test]
    fn paste_follows_the_current_external_window_even_after_the_original_closes() {
        let (target, mut windows) = fixture();
        windows.other_windows.insert(20, WindowSnapshot {
            exists: true, visible: true, process_id: 8, ..Default::default()
        });
        windows.state.exists = false;
        let current = target.resolve_for_paste_with(&mut windows, 42).unwrap();
        assert_eq!((current.hwnd, current.process_id), (20, 8));
        assert_eq!((windows.requests, windows.waits), (0, 0));
        assert!(target.verify_snapshot(windows.state).is_err());
        windows.state.foreground = 30;
        windows.other_windows.insert(30, WindowSnapshot {
            exists: true, visible: true, process_id: 9, ..Default::default()
        });
        let next = target.resolve_for_paste_with(&mut windows, 42).unwrap();
        assert_eq!((next.hwnd, next.process_id), (30, 9));
        assert_eq!((windows.requests, windows.waits), (0, 0));
    }

    #[test]
    fn paste_restores_the_captured_application_when_the_host_is_foreground() {
        let (target, mut windows) = fixture();
        windows.other_windows.insert(20, WindowSnapshot {
            exists: true, visible: true, process_id: 42, ..Default::default()
        });
        let current = target.resolve_for_paste_with(&mut windows, 42).unwrap();
        assert_eq!(current.hwnd, target.hwnd);
        assert_eq!((windows.requests, windows.waits), (1, 1));
        target.verify_snapshot(windows.state).unwrap();
    }

    #[test]
    fn paste_does_not_accept_a_host_target_or_failed_restoration() {
        for failure in ["host-target", "reused-as-host", "activation"] {
            let (mut target, mut windows) = fixture();
            windows.other_windows.insert(20, WindowSnapshot {
                exists: true, visible: true, process_id: 42, ..Default::default()
            });
            if failure != "activation" {
                if failure == "host-target" {
                    target.process_id = 42;
                }
                windows.state.process_id = 42;
            } else {
                windows.responsive = false;
                windows.foreground_after_wait = 20;
            }
            assert!(target.resolve_for_paste_with(&mut windows, 42).is_err(), "{failure}");
            assert_eq!(windows.requests, usize::from(failure == "activation"));
        }
    }

    #[test]
    fn paste_requires_a_valid_external_foreground_or_restorable_original() {
        for failure in ["missing", "closed", "hidden", "unknown-process"] {
            let (target, mut windows) = fixture();
            let mut current = windows.state;
            match failure {
                "missing" => windows.state.foreground = 0,
                "closed" => current.exists = false,
                "hidden" => current.visible = false,
                _ => current.process_id = 0,
            }
            windows.other_windows.insert(20, current);
            assert!(target.resolve_for_paste_with(&mut windows, 42).is_err(), "{failure}");
            assert_eq!((windows.requests, windows.waits), (0, 0));
        }
    }

    #[test]
    fn paste_retries_foreground_handoffs_between_snapshots_with_a_fixed_bound() {
        for settles in [false, true] {
            let (target, mut windows) = fixture();
            for hwnd in [20, 30] {
                windows.other_windows.insert(hwnd, WindowSnapshot {
                    exists: true, visible: true, process_id: 8, ..Default::default()
                });
            }
            windows.foreground_snapshots = if settles {
                [20, 30].into_iter().collect()
            } else {
                [20, 30, 20, 30, 20, 30].into_iter().collect()
            };
            let result = target.resolve_for_paste_with(&mut windows, 42);
            if settles {
                assert_eq!(result.unwrap().hwnd(), 30);
                assert_eq!(windows.snapshots, 4);
            } else {
                assert!(result.is_err());
                assert_eq!(windows.snapshots, 6);
            }
            assert_eq!((windows.requests, windows.waits), (0, 0));
        }
    }

    #[test]
    fn paste_restores_a_minimized_current_external_target() {
        for foreground in [10, 20] {
            let (target, mut windows) = fixture();
            windows.state.foreground = foreground;
            windows.state.minimized = true;
            windows.foreground_after_wait = foreground;
            let current = target.resolve_for_paste_with(&mut windows, 42).unwrap();
            assert_eq!(current.hwnd(), foreground);
            assert_eq!((windows.requests, windows.waits), (1, 1));
            current.verify_snapshot(windows.snapshot(foreground)).unwrap();
        }
    }

    #[test]
    fn paste_follows_an_external_app_selected_during_host_restoration() {
        for responsive in [false, true] {
            let (target, mut windows) = fixture();
            windows.other_windows.insert(20, WindowSnapshot {
                exists: true, visible: true, process_id: 42, ..Default::default()
            });
            windows.other_windows.insert(30, WindowSnapshot {
                exists: true, visible: true, process_id: 8, ..Default::default()
            });
            windows.responsive = responsive;
            windows.foreground_after_wait = 30;
            let current = target.resolve_for_paste_with(&mut windows, 42).unwrap();
            assert_eq!((current.hwnd(), current.process_id), (30, 8));
            assert_eq!((windows.requests, windows.waits), (1, 1));
        }
    }
}
