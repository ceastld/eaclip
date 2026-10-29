//! Low-level keyboard / mouse hooks aligned with C# `WH_KEYBOARD_LL` + `WH_MOUSE_LL`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

use parking_lot::Mutex;

use crate::clipboard::is_headless;
use crate::host::{HostState, NativeHistoryShortcutKeys};
use crate::quick_paste::{KeyEvent, QuickPasteMouseButton};

const MOUSE_PRIORITY_MESSAGE: u32 = 0x8000 + 0x34;
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

static HOOK_HOST: Mutex<Option<HostState>> = Mutex::new(None);
// Installed once with the process-lifetime host. Its three atomic key slots
// stay reachable when HOOK_HOST is busy, without retaining another host lock.
static HOOK_HISTORY_SHORTCUT_KEYS: OnceLock<Arc<NativeHistoryShortcutKeys>> = OnceLock::new();
static HOOK_KEYBOARD_WAKEUP: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();
static HOOK_CHROME_WAKEUP: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();
static HOOK_ORIGIN: OnceLock<Instant> = OnceLock::new();
static FOREGROUND_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static INPUT_REGISTRY_GAP: AtomicBool = AtomicBool::new(false);
static INPUT_REGISTRY_GAPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn has_registry_input_gap() -> bool { INPUT_REGISTRY_GAP.load(Ordering::Acquire) }
pub(crate) fn take_registry_input_gap() -> bool { INPUT_REGISTRY_GAP.swap(false, Ordering::AcqRel) }
/// Events passed through because the hook registry was busy (diagnostics).
pub(crate) fn registry_gap_count() -> u64 { INPUT_REGISTRY_GAPS.load(Ordering::Relaxed) }

/// Parallel-test isolation for process-wide input state. The registry gap and
/// native chrome registry are global, so a fixture that raises the gap or holds
/// the registry would retire or reject unrelated hosts' input. Every test host
/// shares this gate for its test thread; such fixtures take it exclusively.
#[cfg(test)]
pub(crate) mod input_globals_test {
    use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
    use std::cell::{Cell, RefCell};

    static GATE: RwLock<()> = RwLock::new(());
    thread_local! {
        static SHARED: RefCell<Option<RwLockReadGuard<'static, ()>>> = const { RefCell::new(None) };
        static EXCLUSIVE: Cell<bool> = const { Cell::new(false) };
    }

    /// Held until the test thread exits. Recursive reads never queue behind a
    /// waiting writer, so a host opened on a helper thread cannot deadlock.
    pub(crate) fn enter_shared() {
        if EXCLUSIVE.with(Cell::get) { return; }
        SHARED.with(|shared| {
            let mut shared = shared.borrow_mut();
            if shared.is_none() { *shared = Some(GATE.read_recursive()); }
        });
    }

    pub(crate) struct Exclusive(#[allow(dead_code)] RwLockWriteGuard<'static, ()>);
    impl Drop for Exclusive {
        fn drop(&mut self) { EXCLUSIVE.with(|exclusive| exclusive.set(false)); }
    }

    /// Take before opening the fixture host; hosts opened on this thread then
    /// skip the shared gate.
    pub(crate) fn exclusive() -> Exclusive {
        SHARED.with(|shared| shared.borrow_mut().take());
        let guard = GATE.write();
        super::INPUT_REGISTRY_GAP.store(false, std::sync::atomic::Ordering::Release);
        EXCLUSIVE.with(|exclusive| exclusive.set(true));
        Exclusive(guard)
    }
}

thread_local! {
    static INPUT_CALLBACK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub(crate) struct InputCallbackScope;
pub(crate) fn input_callback_scope() -> InputCallbackScope {
    INPUT_CALLBACK_DEPTH.with(|depth| depth.set(depth.get() + 1));
    InputCallbackScope
}
impl Drop for InputCallbackScope {
    fn drop(&mut self) { INPUT_CALLBACK_DEPTH.with(|depth| depth.set(depth.get() - 1)); }
}
pub(crate) fn is_input_callback() -> bool {
    INPUT_CALLBACK_DEPTH.with(|depth| depth.get() != 0)
}
pub(crate) fn take_foreground_refresh() -> bool {
    FOREGROUND_REFRESH_PENDING.swap(false, Ordering::AcqRel)
}

#[cfg(windows)]
thread_local! {
    static MOUSE_HOOK: std::cell::Cell<Option<windows::Win32::UI::WindowsAndMessaging::HHOOK>> =
        const { std::cell::Cell::new(None) };
}

pub struct InputHooks {
    _alive: Arc<AtomicBool>,
}

impl InputHooks {
    pub fn install(host: HostState) -> Self {
        let alive = Arc::new(AtomicBool::new(true));
        if is_headless() {
            return Self { _alive: alive };
        }
        {
            let mut registered = HOOK_HOST.lock();
            // Retain one process-lifetime owner. Input clones can never become
            // the final HostState owner and flush a database on the hook thread.
            if registered.is_some() { return Self { _alive: alive }; }
            let _ = HOOK_HISTORY_SHORTCUT_KEYS.set(host.native_history_shortcut_keys().clone());
            let _ = HOOK_KEYBOARD_WAKEUP.set(host.keyboard_work_notifier());
            let _ = HOOK_CHROME_WAKEUP.set(host.chrome_command_notifier());
            *registered = Some(host);
        }
        HOOK_ORIGIN.get_or_init(Instant::now);
        let flag = alive.clone();
        let _ = thread::Builder::new()
            .name("cliphost-ll-hooks".into())
            .spawn(move || {
                #[cfg(windows)]
                unsafe {
                    pump(flag);
                }
                #[cfg(not(windows))]
                {
                    let _ = flag;
                }
            });
        Self { _alive: alive }
    }
}

/// Called on the owning hook thread at keyboard BeginSession or side-button
/// arming. Move our mouse hook ahead of hooks installed since boot.
pub(crate) fn prioritize_quick_paste_mouse_hook() {
    #[cfg(windows)]
    MOUSE_HOOK.with(|current| {
        let Some(previous) = current.get() else { return; };
        unsafe {
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL,
            };
            let module = GetModuleHandleW(None).unwrap_or_default();
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) {
                Ok(hook) => {
                    // Keep the old hook intact until its replacement exists.
                    current.set(Some(hook));
                    if let Err(error) = UnhookWindowsHookEx(previous) {
                        tracing::warn!(%error, "Could not remove the previous mouse hook");
                    }
                }
                Err(error) => tracing::warn!(%error, "Could not prioritize the quick-paste mouse hook; keeping the existing hook"),
            }
        }
    });
}

/// Settings may run on a worker: rehook once on the owning thread, rather
/// than periodically racing other applications for the hook chain.
pub(crate) fn request_quick_paste_mouse_priority() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
        let thread = HOOK_THREAD_ID.load(Ordering::Acquire);
        if thread == 0 { return; }
        // Reinstall only after returning from the input callback.
        let _ = PostThreadMessageW(thread, MOUSE_PRIORITY_MESSAGE, WPARAM(0), LPARAM(0));
    }
}

fn on_win_event_foreground() {
    FOREGROUND_REFRESH_PENDING.store(true, Ordering::Release);
    if let Some(wakeup) = HOOK_CHROME_WAKEUP.get() { wakeup.notify_one(); }
}

fn on_raw_foreground_event(hwnd: isize) {
    with_hook_host(&HOOK_HOST, |host| {
        host.try_notify_input_foreground(hwnd);
    });
}

#[cfg(all(test, windows))]
pub(crate) fn with_win_event_test_host<T>(host: &HostState, test: impl FnOnce() -> T) -> T {
    crate::active_window::with_test_win_event_callbacks(
        Some(on_win_event_foreground), Some(on_raw_foreground_event), || {
            struct RestoreHost(Option<HostState>);
            impl Drop for RestoreHost {
                fn drop(&mut self) { *HOOK_HOST.lock() = self.0.take(); }
            }
            let _restore = RestoreHost(HOOK_HOST.lock().replace(host.clone()));
            test()
        },
    )
}

fn with_hook_host<T>(registry: &Mutex<Option<HostState>>, callback: impl FnOnce(HostState) -> T) -> Option<T> {
    // An if-let lock temporary otherwise lives through the entire callback.
    // Native queries/input may reenter hooks: retain only the cloned state.
    let Some(registered) = registry.try_lock() else {
        INPUT_REGISTRY_GAP.store(true, Ordering::Release);
        INPUT_REGISTRY_GAPS.fetch_add(1, Ordering::Relaxed);
        if let Some(wakeup) = HOOK_KEYBOARD_WAKEUP.get() { wakeup.notify_one(); }
        return None;
    };
    let host = registered.clone();
    drop(registered);
    if let Some(host) = host.as_ref() {
        if has_registry_input_gap() {
            host.mark_input_gap();
            take_registry_input_gap();
        }
    }
    host.map(callback)
}

#[cfg(windows)]
fn dispatch_keyboard_event(
    registry: &Mutex<Option<HostState>>, history_keys: &NativeHistoryShortcutKeys,
    injected: bool, extra: usize,
    make_event: impl FnOnce() -> KeyEvent,
) -> bool {
    // Our own paste/text/menu-pulse input belongs to its verified target. Skip
    // before modifier reads or state routing so a pinned host cannot eat it.
    if injected && extra == crate::paste::HOST_KEYBOARD_INPUT_TAG {
        return false;
    }
    let event = make_event();
    let (fresh, owned) = history_keys.observe(event);
    if owned {
        // A swallowed Down owns repeat/Up through registry and host contention,
        // hiding, and focus handoff. Its Up always clears the physical key slot.
        return true;
    }
    // A Down passed by this registry gate remains observed: a later repeat
    // cannot be adopted as a new history shortcut after the gate becomes free.
    with_hook_host(registry, |host| host.try_dispatch_observed_input_key(event, fresh)).unwrap_or(false)
}

#[cfg(all(test, windows))]
pub(crate) fn deliver_registry_busy_key_for_test(host: &HostState, event: KeyEvent) -> bool {
    let registry = Mutex::new(Some(host.clone()));
    let _held = registry.lock();
    dispatch_keyboard_event(&registry, host.native_history_shortcut_keys(), false, 0, || event)
}

#[cfg(all(test, windows))]
pub(crate) fn deliver_keyboard_event_for_test(host: &HostState, event: KeyEvent, injected: bool, extra: usize) -> bool {
    let registry = Mutex::new(Some(host.clone()));
    dispatch_keyboard_event(&registry, host.native_history_shortcut_keys(), injected, extra, || event)
}

fn now_ms() -> u64 {
    HOOK_ORIGIN
        .get()
        .map(|origin| origin.elapsed().as_millis() as u64)
        .unwrap_or(0)
}

fn modifiers() -> (bool, bool, bool, bool) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
        };
        let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| {
            GetAsyncKeyState(vk.0 as i32) < 0
        };
        (
            down(VK_MENU),
            down(VK_CONTROL),
            down(VK_SHIFT),
            down(VK_LWIN) || down(VK_RWIN),
        )
    }
    #[cfg(not(windows))]
    {
        (false, false, false, false)
    }
}

#[cfg(windows)]
unsafe fn pump(alive: Arc<AtomicBool>) {
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        PeekMessageW, HHOOK, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    let mut msg = MSG::default();
    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);
    let module = GetModuleHandleW(None).unwrap_or_default();
    let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
    match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) {
        Ok(hook) => MOUSE_HOOK.with(|current| current.set(Some(hook))),
        Err(error) => tracing::warn!(%error, "Could not install the low-level mouse hook"),
    }
    let winevents = crate::active_window::install_on_current_thread();
    crate::active_window::set_after_raw_foreground_event(Some(on_raw_foreground_event));
    crate::active_window::set_after_foreground_event(Some(on_win_event_foreground));
    while alive.load(Ordering::Relaxed) && GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.message == MOUSE_PRIORITY_MESSAGE {
            prioritize_quick_paste_mouse_hook();
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    HOOK_THREAD_ID.store(0, Ordering::Release);
    crate::active_window::set_after_foreground_event(None);
    crate::active_window::set_after_raw_foreground_event(None);
    crate::active_window::uninstall(winevents);
    if let Ok(hook) = kbd {
        let _ = UnhookWindowsHookEx(hook);
    }
    MOUSE_HOOK.with(|current| {
        if let Some(hook) = current.take() { let _ = UnhookWindowsHookEx(hook); }
    });
    let _ = (HHOOK::default(), LPARAM::default(), LRESULT::default(), WPARAM::default());
}

#[cfg(windows)]
unsafe extern "system" fn keyboard_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };
    if code >= 0 && lparam.0 != 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if let Some(history_keys) = HOOK_HISTORY_SHORTCUT_KEYS.get().filter(|_| down || up) {
            if dispatch_keyboard_event(&HOOK_HOST, history_keys, info.flags.0 & LLKHF_INJECTED.0 != 0, info.dwExtraInfo, || {
                let (alt, ctrl, shift, win) = modifiers();
                KeyEvent { vk: info.vkCode, down, alt, ctrl, shift, win, now_ms: now_ms() }
            }) {
                return windows::Win32::Foundation::LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[cfg(windows)]
unsafe extern "system" fn mouse_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, MSLLHOOKSTRUCT,
    };
    // C# `cea_ll_hook.c` never enters managed code for WM_MOUSEMOVE. Drag-select
    // would otherwise serialize GetWindowRect + hide evaluation on every pixel.
    let msg = wparam.0 as u32;
    if !is_interesting_mouse(msg) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    if code >= 0 && lparam.0 != 0 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let swallowed = with_hook_host(&HOOK_HOST, |host| {
            host.try_dispatch_input_mouse(msg, info.pt.x, info.pt.y, info.mouseData)
        }).unwrap_or(false);
        if swallowed {
            return windows::Win32::Foundation::LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

pub(crate) fn dispatch_mouse_down(host: &crate::HostState, message: u32, x: i32, y: i32) {
    match message {
        0x0201 => host.notify_outside_click_at(x, y), // WM_LBUTTONDOWN
        0x0204 => host.notify_panel_outside_click_at(x, y), // WM_RBUTTONDOWN
        _ => {}
    }
}

/// Buttons and wheel only. `WM_MOUSEMOVE` must stay out of the hook path.
pub fn is_interesting_mouse(msg: u32) -> bool {
    matches!(
        msg,
        0x0201 | // WM_LBUTTONDOWN
        0x0202 | // WM_LBUTTONUP
        0x0204 | // WM_RBUTTONDOWN
        0x0205 | // WM_RBUTTONUP
        0x020B | // WM_XBUTTONDOWN
        0x020C | // WM_XBUTTONUP
        0x020A | // WM_MOUSEWHEEL
        0x020E // WM_MOUSEHWHEEL
    )
}

fn side_button_from_mouse_data(data: u32) -> Option<QuickPasteMouseButton> {
    match (data >> 16) as u16 {
        1 => Some(QuickPasteMouseButton::X1),
        2 => Some(QuickPasteMouseButton::X2),
        _ => None,
    }
}

pub fn parse_chord(chord: &str) -> Option<(bool, bool, bool, bool, u32)> {
    crate::global_hotkey::try_parse_parts(chord)
}

pub fn digit_0_to_9(vk: u32) -> Option<u8> {
    match vk {
        0x31..=0x39 => Some((vk - 0x31) as u8),
        0x61..=0x69 => Some((vk - 0x61) as u8),
        0x30 | 0x60 => Some(9),
        _ => None,
    }
}

/// C# `TryMapDigitVirtualKey`: `vk - VK_0` / `vk - VK_NUMPAD0` (key `1` → `1`).
pub fn csharp_digit_key_value(vk: u32) -> Option<u8> {
    match vk {
        0x30..=0x39 => Some((vk - 0x30) as u8),
        0x60..=0x69 => Some((vk - 0x60) as u8),
        _ => None,
    }
}

pub fn is_navigation_key(vk: u32) -> bool {
    // C# HostHidePolicy key-nav + clipboard `MapVirtualKeyToWebKeyName` / Tab switch.
    matches!(vk, 0x09 | 0x0D | 0x20 | 0x21..=0x28)
}

/// C# clipboard/panel `MapVirtualKeyToWebKeyName` (Tab is switch-tab, not this map).
pub fn web_navigation_key_name(vk: u32) -> Option<&'static str> {
    match vk {
        0x25 => Some("ArrowLeft"),
        0x26 => Some("ArrowUp"),
        0x27 => Some("ArrowRight"),
        0x28 => Some("ArrowDown"),
        0x0D => Some("Enter"),
        0x20 => Some(" "),
        0x21 => Some("PageUp"),
        0x22 => Some("PageDown"),
        0x23 => Some("End"),
        0x24 => Some("Home"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn own_input_fixture(panel: bool) -> (tempfile::TempDir, HostState, Mutex<Option<HostState>>) {
        crate::clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(crate::AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        host.request_show();
        host.apply_window_action_for("setPinned", true, "clipboard");
        if panel {
            host.favor_folder_create(&serde_json::json!({"title": "Owned input fixture"})).unwrap();
            host.panel_show(&serde_json::json!({})).unwrap();
            host.set_panel_web_view_ready(true);
            host.apply_window_action_for("setPinned", true, "panel");
        }
        while host.take_chrome_command().is_some() {}
        let registry = Mutex::new(Some(host.clone()));
        (root, host, registry)
    }

    #[cfg(windows)]
    fn assert_own_input_passes_pinned_host(panel: bool) {
        use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP;
        let (_root, host, registry) = own_input_fixture(panel);
        let before = host.hide_snapshot();
        let decoded = std::cell::Cell::new(0);
        for vk in [0x0D, 0x09, 0x31, 0x41, 0x08, 0x28] {
            for input in crate::paste::chord_inputs(false, false, vk) {
                let keyboard = unsafe { input.Anonymous.ki };
                assert!(!dispatch_keyboard_event(&registry, host.native_history_shortcut_keys(), true, keyboard.dwExtraInfo, || {
                    decoded.set(decoded.get() + 1);
                    KeyEvent { vk: keyboard.wVk.0 as u32,
                        down: keyboard.dwFlags.0 & KEYEVENTF_KEYUP.0 == 0,
                        alt: false, ctrl: false, shift: false, win: false, now_ms: 1 }
                }), "self-generated input intercepted: panel={panel}, vk={vk:x}");
                assert!(host.take_chrome_command().is_none(), "self-generated input operated the host");
            }
        }
        assert_eq!(decoded.get(), 0, "owned input must bypass modifier reads and host routing");
        let after = host.hide_snapshot();
        assert_eq!((after.visible, after.has_fake_focus, after.no_focus_session_active),
            (before.visible, before.has_fake_focus, before.no_focus_session_active));
    }

    #[cfg(windows)]
    #[test]
    fn own_input_does_not_operate_pinned_clipboard() {
        assert_own_input_passes_pinned_host(false);
    }

    #[cfg(windows)]
    #[test]
    fn own_input_does_not_operate_pinned_panel() {
        assert_own_input_passes_pinned_host(true);
    }

    #[cfg(windows)]
    #[test]
    fn physical_and_foreign_injected_keys_keep_their_existing_host_routing() {
        use crate::host::ChromeCommand;
        for panel in [false, true] {
            let (_root, host, registry) = own_input_fixture(panel);
            for (injected, extra) in [(false, crate::paste::HOST_KEYBOARD_INPUT_TAG),
                (true, 0), (true, crate::paste::HOST_KEYBOARD_INPUT_TAG ^ 1)] {
                let decoded = std::cell::Cell::new(0);
                assert!(dispatch_keyboard_event(&registry, host.native_history_shortcut_keys(), injected, extra, || {
                    decoded.set(decoded.get() + 1);
                    KeyEvent { vk: 0x0D, down: true, alt: false, ctrl: false,
                        shift: false, win: false, now_ms: 1 }
                }));
                assert_eq!(decoded.get(), 1);
                assert_eq!(host.take_chrome_command(), Some(if panel { ChromeCommand::PanelNavKey("Enter") }
                    else { ChromeCommand::ClipboardNavKey { key: "Enter", shift: false } }));
                assert!(host.take_chrome_command().is_none());
                assert!(!dispatch_keyboard_event(&registry, host.native_history_shortcut_keys(), injected, extra, || KeyEvent {
                    vk: 0x0D, down: false, alt: false, ctrl: false,
                    shift: false, win: false, now_ms: 2,
                }));
            }
        }
    }

    #[test]
    fn callback_reentry_can_read_hook_registry_without_deadlock() {
        crate::clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(crate::AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
        let registry = Mutex::new(Some(host));
        with_hook_host(&registry, |_| {
            assert!(registry.try_lock().is_some(), "hook registry stays locked during a reentrant callback");
        }).unwrap();
    }

    #[test]
    fn parses_default_chords() {
        assert_eq!(parse_chord("Alt+`"), Some((false, true, false, false, 0xC0)));
        assert_eq!(parse_chord("Alt+Oem3"), Some((false, true, false, false, 0xC0)));
        assert_eq!(parse_chord("Ctrl+Oem3"), Some((true, false, false, false, 0xC0)));
        assert_eq!(parse_chord("Ctrl+Shift+V"), Some((true, false, true, false, 0x56)));
    }

    #[test]
    fn maps_digit_keys() {
        assert_eq!(digit_0_to_9(0x31), Some(0));
        assert_eq!(digit_0_to_9(0x30), Some(9));
        assert_eq!(csharp_digit_key_value(0x31), Some(1));
        assert_eq!(csharp_digit_key_value(0x30), Some(0));
    }

    #[test]
    fn web_navigation_key_name_matches_csharp_map() {
        assert_eq!(web_navigation_key_name(0x28), Some("ArrowDown"));
        assert_eq!(web_navigation_key_name(0x0D), Some("Enter"));
        assert_eq!(web_navigation_key_name(0x20), Some(" "));
        assert_eq!(web_navigation_key_name(0x09), None);
        assert!(is_navigation_key(0x09));
        assert!(is_navigation_key(0x0D));
    }

    #[test]
    fn mouse_hook_ignores_move() {
        assert!(!is_interesting_mouse(0x0200)); // WM_MOUSEMOVE
        assert!(is_interesting_mouse(0x0201));
        assert!(is_interesting_mouse(0x0204));
        assert!(is_interesting_mouse(0x0205));
        assert!(is_interesting_mouse(0x020A));
        assert!(is_interesting_mouse(0x020B));
        assert!(is_interesting_mouse(0x020C));
        assert_eq!(side_button_from_mouse_data(1 << 16), Some(QuickPasteMouseButton::X1));
        assert_eq!(side_button_from_mouse_data(2 << 16), Some(QuickPasteMouseButton::X2));
        assert_eq!(side_button_from_mouse_data(3 << 16), None);
    }
}
