//! C# `ActiveWindowHook` + `HookBase` foreground WinEvents.
//!
//! Install **one hook per event id**. `SetWinEventHook(min, max)` is inclusive, so
//! FOREGROUND=3 plus MINIMIZEEND=23 as a range would also deliver CAPTURESTART=8
//! (drag-selecting text then looks like a window switch).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// C# `EVENT_SYSTEM_FOREGROUND`.
pub const EVENT_SYSTEM_FOREGROUND: u32 = 3;
/// C# `EVENT_SYSTEM_MINIMIZEEND`.
pub const EVENT_SYSTEM_MINIMIZEEND: u32 = 23;
/// C# `EVENT_SYSTEM_CAPTURESTART` — must **not** be tracked.
pub const EVENT_SYSTEM_CAPTURESTART: u32 = 8;

/// C# `ActiveWindowHook.TrackedWinEvents`.
pub const TRACKED_WIN_EVENTS: [u32; 2] = [EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND];

const HANDLE_LIST_CAP: usize = 100;
/// C# `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`.
const OUT_OF_CONTEXT_SKIP_OWN_PROCESS: u32 = 0x0002;

struct WinEventState {
    last_hwnd: isize,
    history: VecDeque<isize>,
    /// C# `ActiveWindowService.activeWindows` resolved roots (Ignore host / QP / Super Menu).
    resolved_roots: VecDeque<isize>,
}

impl Default for WinEventState {
    fn default() -> Self {
        Self {
            last_hwnd: 0,
            history: VecDeque::with_capacity(HANDLE_LIST_CAP),
            resolved_roots: VecDeque::with_capacity(HANDLE_LIST_CAP),
        }
    }
}

static STATE: Mutex<WinEventState> = Mutex::new(WinEventState {
    last_hwnd: 0,
    history: VecDeque::new(),
    resolved_roots: VecDeque::new(),
});
static AFTER_FOREGROUND: Mutex<Option<fn()>> = Mutex::new(None);
static AFTER_RAW_FOREGROUND: Mutex<Option<fn(isize)>> = Mutex::new(None);
static RAW_FOREGROUND_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn raw_foreground_epoch() -> u64 {
    RAW_FOREGROUND_EPOCH.load(Ordering::Acquire)
}

#[cfg(test)]
// All tests that reset or populate the process-wide history share this fixture lock.
pub(crate) static HISTORY_TEST: Mutex<()> = Mutex::new(());

/// Legacy history/root refresh after both FOREGROUND and MINIMIZEEND.
pub fn set_after_foreground_event(cb: Option<fn()>) {
    *AFTER_FOREGROUND.lock() = cb;
}

/// Actual foreground changes carry their original HWND, before legacy root
/// resolution or a live foreground query can erase a short A→B→A transition.
/// MINIMIZEEND remains a history event and must not release keyboard ownership.
pub(crate) fn set_after_raw_foreground_event(cb: Option<fn(isize)>) {
    *AFTER_RAW_FOREGROUND.lock() = cb;
}

/// C# `HookBase.WouldInclusiveRangeIncludeUnlistedEvents`.
pub fn would_inclusive_range_include_unlisted_events(event_list: &[u32]) -> bool {
    if event_list.is_empty() {
        return false;
    }
    let min = *event_list.iter().min().unwrap();
    let max = *event_list.iter().max().unwrap();
    (min..=max).any(|ev| !event_list.contains(&ev))
}

fn enqueue_distinct(buf: &mut VecDeque<isize>, hwnd: isize) {
    if hwnd == 0 || buf.front() == Some(&hwnd) {
        return;
    }
    if buf.len() == HANDLE_LIST_CAP {
        buf.pop_back();
    }
    buf.push_front(hwnd);
}

/// C# `DataBuffer.EnqueueDistinct` then `LastForegroundEventHwnd = hwnd`.
pub fn record_foreground_event_hwnd(hwnd: isize) {
    let mut state = STATE.lock();
    state.last_hwnd = hwnd;
    enqueue_distinct(&mut state.history, hwnd);
}

/// C# `ActiveWindowService.Update` of the resolved root (host windows are Ignore()'d).
pub fn record_resolved_root_hwnd(hwnd: isize) {
    let mut state = STATE.lock();
    enqueue_distinct(&mut state.resolved_roots, hwnd);
}

pub fn last_foreground_event_hwnd() -> isize {
    STATE.lock().last_hwnd
}

pub fn foreground_event_history() -> Vec<isize> {
    STATE.lock().history.iter().copied().collect()
}

/// C# `IActiveWindowService.GetLastForegroundWindow(except)`.
pub fn last_visible_foreground_except_with(
    except: isize,
    is_visible: impl Fn(isize) -> bool,
) -> isize {
    let roots = STATE.lock().resolved_roots.clone();
    for hwnd in roots {
        if hwnd != 0 && hwnd != except && is_visible(hwnd) {
            return hwnd;
        }
    }
    0
}

#[cfg(windows)]
pub fn last_visible_foreground_except(except: isize) -> isize {
    last_visible_foreground_except_with(except, |hwnd| unsafe {
        windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
        )
        .as_bool()
            && !crate::input_target::belongs_to_host(hwnd)
    })
}

/// C# `ResolveForegroundRootWindow` after Explorer shell chrome: event hwnd, then
/// `HandleList`, then `activeWindows` / EnumWindows fallback.
pub fn resolve_shell_chrome_fallback(
    event_hwnd: isize,
    history: impl IntoIterator<Item = isize>,
    mut try_root: impl FnMut(isize) -> Option<isize>,
    enum_fallback: Option<isize>,
) -> Option<isize> {
    if let Some(root) = try_root(event_hwnd) {
        return Some(root);
    }
    for hwnd in history {
        if let Some(root) = try_root(hwnd) {
            return Some(root);
        }
    }
    enum_fallback
}

#[cfg(windows)]
unsafe extern "system" fn win_event_proc(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    hwnd: windows::Win32::Foundation::HWND,
    _id_object: i32,
    _id_child: i32,
    _id_thread: u32,
    _ms: u32,
) {
    if !TRACKED_WIN_EVENTS.contains(&event) {
        return;
    }
    // This callback shares the input pump. History/logging/host refresh may not
    // wait here. The epoch preserves A -> B -> A retirement even while busy.
    if event == EVENT_SYSTEM_FOREGROUND {
        RAW_FOREGROUND_EPOCH.fetch_add(1, Ordering::AcqRel);
    }
    if let Some(mut state) = STATE.try_lock() {
        state.last_hwnd = hwnd.0 as isize;
        enqueue_distinct(&mut state.history, hwnd.0 as isize);
    }
    if event == EVENT_SYSTEM_FOREGROUND {
        let cb = AFTER_RAW_FOREGROUND.try_lock().and_then(|callback| *callback);
        if let Some(cb) = cb {
            cb(hwnd.0 as isize);
        }
    }
    let cb = AFTER_FOREGROUND.try_lock().and_then(|callback| *callback);
    if let Some(cb) = cb {
        cb();
    }
}

#[cfg(all(test, windows))]
pub(crate) fn deliver_test_win_event(event: u32, hwnd: isize) {
    // Exercise the actual registered extern callback, without installing an OS
    // hook, changing foreground, or injecting any input into another process.
    unsafe {
        win_event_proc(
            Default::default(),
            event,
            windows::Win32::Foundation::HWND(hwnd as *mut _),
            0,
            0,
            0,
            0,
        );
    }
}

#[cfg(all(test, windows))]
pub(crate) fn with_test_win_event_callbacks<T>(
    tracked: Option<fn()>,
    raw: Option<fn(isize)>,
    test: impl FnOnce() -> T,
) -> T {
    // Take the input gate before the history lock. Fixtures may already hold
    // its shared guard; exclusive() releases that guard before waiting, so a
    // second WinEvent fixture cannot keep it while blocked on HISTORY_TEST.
    let _input_globals = crate::hooks::input_globals_test::exclusive();
    let _serial = HISTORY_TEST.lock();
    struct Restore {
        state: Option<WinEventState>,
        tracked: Option<fn()>,
        raw: Option<fn(isize)>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            *STATE.lock() = self.state.take().unwrap();
            *AFTER_FOREGROUND.lock() = self.tracked;
            *AFTER_RAW_FOREGROUND.lock() = self.raw;
        }
    }
    let _restore = Restore {
        state: Some(std::mem::take(&mut *STATE.lock())),
        tracked: std::mem::replace(&mut *AFTER_FOREGROUND.lock(), tracked),
        raw: std::mem::replace(&mut *AFTER_RAW_FOREGROUND.lock(), raw),
    };
    test()
}

#[cfg(windows)]
pub fn install_on_current_thread() -> Vec<windows::Win32::UI::Accessibility::HWINEVENTHOOK> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::UI::Accessibility::{SetWinEventHook, WINEVENTPROC};

    let proc: WINEVENTPROC = Some(win_event_proc);
    let mut hooks = Vec::new();
    for event_id in TRACKED_WIN_EVENTS {
        let hook = unsafe {
            SetWinEventHook(
                event_id,
                event_id,
                HMODULE::default(),
                proc,
                0,
                0,
                OUT_OF_CONTEXT_SKIP_OWN_PROCESS,
            )
        };
        if !hook.is_invalid() {
            hooks.push(hook);
        }
    }
    hooks
}

#[cfg(windows)]
pub fn uninstall(hooks: Vec<windows::Win32::UI::Accessibility::HWINEVENTHOOK>) {
    use windows::Win32::UI::Accessibility::UnhookWinEvent;
    for hook in hooks {
        if !hook.is_invalid() {
            let _ = unsafe { UnhookWinEvent(hook) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn win_event_callback_separates_minimize_history_and_raw_foreground_in_order() {
        static CALLS: Mutex<Vec<(bool, isize)>> = Mutex::new(Vec::new());
        fn tracked() {
            CALLS.lock().push((false, last_foreground_event_hwnd()));
        }
        fn raw(hwnd: isize) {
            CALLS.lock().push((true, hwnd));
        }
        with_test_win_event_callbacks(Some(tracked), Some(raw), || {
            CALLS.lock().clear();
            record_resolved_root_hwnd(42);
            deliver_test_win_event(EVENT_SYSTEM_MINIMIZEEND, 43);
            assert_eq!(*CALLS.lock(), [(false, 43)]);
            assert_eq!(last_foreground_event_hwnd(), 43);
            assert_eq!(foreground_event_history(), [43]);
            assert_eq!(last_visible_foreground_except_with(0, |_| true), 42);
            // Keep the old Explorer-shell recovery semantics for event 23.
            assert_eq!(
                resolve_shell_chrome_fallback(
                    last_foreground_event_hwnd(),
                    foreground_event_history(),
                    |hwnd| (hwnd == 43).then_some(430),
                    None
                ),
                Some(430)
            );
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 44);
            assert_eq!(*CALLS.lock(), [(false, 43), (true, 44), (false, 44)]);
            deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 42);
            assert_eq!(
                *CALLS.lock(),
                [
                    (false, 43),
                    (true, 44),
                    (false, 44),
                    (true, 42),
                    (false, 42)
                ]
            );
            deliver_test_win_event(EVENT_SYSTEM_CAPTURESTART, 99);
            assert_eq!(
                CALLS.lock().len(),
                5,
                "untracked event calls neither callback"
            );
            assert_eq!(last_foreground_event_hwnd(), 42);
            assert_eq!(foreground_event_history(), [42, 44, 43]);
        });
    }

    #[test]
    fn sparse_foreground_and_minimize_range_would_also_deliver_capture() {
        assert!(would_inclusive_range_include_unlisted_events(&[3, 8, 23]));
        assert!(!would_inclusive_range_include_unlisted_events(&[10, 11]));
    }

    #[test]
    fn active_window_hook_does_not_listen_for_mouse_capture() {
        assert!(!TRACKED_WIN_EVENTS.contains(&EVENT_SYSTEM_CAPTURESTART));
        assert!(would_inclusive_range_include_unlisted_events(&TRACKED_WIN_EVENTS));
    }

    #[test]
    fn enqueue_distinct_skips_repeat_head_and_drops_oldest() {
        let _serial = HISTORY_TEST.lock();
        let mut saved = STATE.lock();
        *saved = WinEventState::default();
        drop(saved);
        record_foreground_event_hwnd(10);
        record_foreground_event_hwnd(10);
        record_foreground_event_hwnd(11);
        record_foreground_event_hwnd(0);
        assert_eq!(last_foreground_event_hwnd(), 0);
        assert_eq!(foreground_event_history(), vec![11, 10]);
        for i in 12..112 {
            record_foreground_event_hwnd(i);
        }
        let hist = foreground_event_history();
        assert_eq!(hist.len(), 100);
        assert_eq!(hist[0], 111);
        assert!(!hist.contains(&10));
    }

    #[test]
    fn last_visible_foreground_skips_except_and_invisible() {
        let _serial = HISTORY_TEST.lock();
        let mut saved = STATE.lock();
        *saved = WinEventState::default();
        drop(saved);
        record_resolved_root_hwnd(1);
        record_resolved_root_hwnd(2);
        record_resolved_root_hwnd(3);
        assert_eq!(
            last_visible_foreground_except_with(3, |h| h != 2),
            1
        );
        assert_eq!(last_visible_foreground_except_with(0, |_| true), 3);
    }

    #[test]
    fn shell_chrome_fallback_prefers_event_hwnd_then_history() {
        let try_root = |hwnd: isize| match hwnd {
            0 | 1 => None,
            2 => Some(20),
            3 => Some(30),
            _ => None,
        };
        assert_eq!(
            resolve_shell_chrome_fallback(2, [3, 1], try_root, Some(99)),
            Some(20)
        );
        assert_eq!(
            resolve_shell_chrome_fallback(1, [3, 1], try_root, Some(99)),
            Some(30)
        );
        assert_eq!(
            resolve_shell_chrome_fallback(0, [1], try_root, Some(99)),
            Some(99)
        );
        assert_eq!(
            resolve_shell_chrome_fallback(0, [1], try_root, None),
            None
        );
    }
}
