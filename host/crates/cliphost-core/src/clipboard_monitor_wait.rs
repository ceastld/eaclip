//! Clipboard notifications wake the capture worker; they never consume a copy.
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

const RETRY_DELAY: Duration = Duration::from_millis(80);
// A safety net for a missed OS notification, not the normal capture schedule.
const SAFETY_CHECK: Duration = Duration::from_secs(30);

pub(super) struct StopSignal {
    stopped: Mutex<bool>,
    changed: Condvar,
    // An owned, manual-reset event shared with the waiting worker. Its Arc
    // outlives every wait, including when ClipboardMonitor is being dropped.
    #[cfg(windows)]
    event: isize,
}

impl StopSignal {
    pub(super) fn new() -> Self {
        Self {
            stopped: Mutex::new(false),
            changed: Condvar::new(),
            #[cfg(windows)]
            event: unsafe {
                windows::Win32::System::Threading::CreateEventW(None, true, false, None)
                    .map(|event| event.0 as isize).unwrap_or(0)
            },
        }
    }

    pub(super) fn signal(&self) {
        *self.stopped.lock() = true;
        self.changed.notify_all();
        #[cfg(windows)]
        if self.event != 0 {
            unsafe {
                let _ = windows::Win32::System::Threading::SetEvent(
                    windows::Win32::Foundation::HANDLE(self.event as *mut _));
            }
        }
    }

    pub(super) fn is_stopped(&self) -> bool { *self.stopped.lock() }

    pub(super) fn wait_timeout(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        let mut stopped = self.stopped.lock();
        // A persistent predicate handles stop-before-wait and spurious wakes.
        while !*stopped {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { break; }
            self.changed.wait_for(&mut stopped, remaining);
        }
    }
}

#[cfg(windows)]
impl Drop for StopSignal {
    fn drop(&mut self) {
        if self.event != 0 {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(
                    windows::Win32::Foundation::HANDLE(self.event as *mut _));
            }
        }
    }
}

pub(super) struct MonitorWait {
    #[cfg(windows)]
    listener: Option<native::Listener>,
}

impl MonitorWait {
    pub(super) fn new(stop: &StopSignal) -> Self {
        #[cfg(windows)]
        {
            let listener = if stop.event == 0 {
                tracing::warn!("Clipboard stop event unavailable; using 80 ms sequence polling");
                None
            } else {
                match native::Listener::new() {
                    Ok(listener) => Some(listener),
                    Err(error) => {
                        tracing::warn!(%error, "Clipboard notification registration failed; using 80 ms sequence polling");
                        None
                    }
                }
            };
            Self { listener }
        }
        #[cfg(not(windows))]
        { let _ = stop; Self {} }
    }

    /// False means stop; every true result requests only a sequence check.
    pub(super) fn wait(&mut self, stop: &StopSignal, retry_pending: bool) -> bool {
        #[cfg(windows)]
        if let Some(listener) = self.listener.as_ref() {
            let timeout = if retry_pending { RETRY_DELAY } else { SAFETY_CHECK };
            match listener.wait(stop, timeout) {
                Ok(native::Wake::Stopped) => return false,
                Ok(_) => return true,
                Err(error) => {
                    tracing::warn!(%error, "Clipboard notification wait failed; using 80 ms sequence polling");
                    self.listener = None;
                }
            }
        }
        // Registration failure preserves capture availability and still lets
        // shutdown interrupt the fallback wait, rather than losing monitoring.
        #[cfg(not(windows))]
        let _ = retry_pending;
        stop.wait_timeout(RETRY_DELAY);
        !stop.is_stopped()
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::{core::{HSTRING, PCWSTR, w}, Win32::{
        Foundation::{HANDLE, HMODULE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM},
        System::{DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener},
            LibraryLoader::GetModuleHandleW},
        UI::WindowsAndMessaging::*,
    }};

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Wake { Changed, TimedOut, Stopped }

    pub(super) struct Listener {
        hwnd: HWND,
        class: HSTRING,
        instance: HMODULE,
    }

    impl Listener {
        pub(super) fn new() -> windows::core::Result<Self> {
            unsafe {
                let class = HSTRING::from(format!("ClipHostClipboardMonitor_{}", uuid::Uuid::new_v4().simple()));
                let instance = GetModuleHandleW(None)?;
                let window_class = WNDCLASSW { lpfnWndProc: Some(window_proc),
                    hInstance: instance.into(), lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() };
                if RegisterClassW(&window_class) == 0 { return Err(windows::core::Error::from_win32()); }
                let hwnd = match CreateWindowExW(WINDOW_EX_STYLE::default(), &class,
                    w!("ClipHost clipboard monitor"), WINDOW_STYLE::default(), 0, 0, 0, 0,
                    HWND_MESSAGE, None, instance, None) {
                    Ok(hwnd) => hwnd,
                    Err(error) => { let _ = UnregisterClassW(&class, instance); return Err(error); }
                };
                let listener = Self { hwnd, class, instance };
                AddClipboardFormatListener(hwnd)?;
                Ok(listener)
            }
        }

        pub(super) fn wait(&self, stop: &StopSignal, timeout: Duration) -> windows::core::Result<Wake> {
            let deadline = Instant::now() + timeout;
            let handles = [HANDLE(stop.event as *mut _)];
            loop {
                if stop.is_stopped() { return Ok(Wake::Stopped); }
                let remaining = deadline.saturating_duration_since(Instant::now());
                // Round up to avoid spinning during the last fractional ms.
                let milliseconds = remaining.as_millis().saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
                    .min(u128::from(u32::MAX - 1)) as u32;
                let result = unsafe { MsgWaitForMultipleObjectsEx(Some(&handles), milliseconds,
                    QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
                if result == WAIT_OBJECT_0 { return Ok(Wake::Stopped); }
                if result == WAIT_TIMEOUT { return Ok(Wake::TimedOut); }
                if result.0 != WAIT_OBJECT_0.0 + handles.len() as u32 {
                    return Err(windows::core::Error::from_win32());
                }
                let mut changed = false;
                let mut message = MSG::default();
                unsafe {
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        if message.message == WM_QUIT { return Ok(Wake::Stopped); }
                        if message.hwnd == self.hwnd && message.message == WM_CLIPBOARDUPDATE {
                            changed = true;
                        }
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                if stop.is_stopped() { return Ok(Wake::Stopped); }
                if changed { return Ok(Wake::Changed); }
                // System events can wake MsgWait without an actual queued
                // message. Pump them above and retain the original deadline.
                if Instant::now() >= deadline { return Ok(Wake::TimedOut); }
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            unsafe {
                let _ = RemoveClipboardFormatListener(self.hwnd);
                let _ = DestroyWindow(self.hwnd);
                let _ = UnregisterClassW(&self.class, self.instance);
            }
        }
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(hwnd, message, w, l)
    }
}

#[cfg(test)]
#[path = "clipboard_monitor_wait_tests.rs"]
mod tests;
