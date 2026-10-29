//! C# `ChromelessWindow` HTML title-bar passthrough + `WindowCaptionDragInterop`.
//!
//! ClipHost paints the caption in WebView HTML. C# therefore returns **HTCLIENT** for the
//! title band (`PassthroughHitTestWndProc`, `captionHeight: 0`) so buttons receive clicks.
//! WebView2 owns CSS caption regions in its child window. The root keeps client
//! passthrough; explicit `WM_NCLBUTTONDOWN` remains an unsupported-runtime fallback.

use crate::clipboard::is_headless;

pub const HTCLIENT: i32 = 1;
pub const HTCAPTION: i32 = 2;
pub const HTLEFT: i32 = 10;
pub const HTRIGHT: i32 = 11;
pub const HTTOP: i32 = 12;
pub const HTTOPLEFT: i32 = 13;
pub const HTTOPRIGHT: i32 = 14;
pub const HTBOTTOM: i32 = 15;
pub const HTBOTTOMLEFT: i32 = 16;
pub const HTBOTTOMRIGHT: i32 = 17;

/// C# `NativeCaptionlessWindowChrome` resize border (DIPs).
pub const RESIZE_BORDER_DIP: i32 = 8;

#[derive(Clone, Copy, Default)]
struct InputChromeState {
    hwnd: isize,
    move_size: bool,
    active: Option<bool>,
}

thread_local! {
    static INPUT_CHROME_SNAPSHOT: std::cell::Cell<Option<[InputChromeState; 3]>> = const { std::cell::Cell::new(None) };
}

pub(crate) struct InputChromeScope(Option<[InputChromeState; 3]>);

impl Drop for InputChromeScope {
    fn drop(&mut self) { INPUT_CHROME_SNAPSHOT.with(|snapshot| snapshot.set(self.0)); }
}

/// Copy all input-visible chrome facts with one try-lock. Readers in this scope
/// never reacquire the registry, including helpers nested after state mutation.
pub(crate) fn try_input_scope(hwnds: [isize; 3]) -> Option<InputChromeScope> {
    #[cfg(windows)]
    let states = win::try_input_snapshot(hwnds)?;
    #[cfg(not(windows))]
    let states = hwnds.map(|hwnd| InputChromeState { hwnd, ..InputChromeState::default() });
    Some(InputChromeScope(INPUT_CHROME_SNAPSHOT.with(|snapshot| snapshot.replace(Some(states)))))
}

#[cfg(all(test, windows))]
pub(crate) fn with_registry_held_for_test<T>(work: impl FnOnce() -> T) -> T {
    win::with_registry_held_for_test(work)
}

/// C# `NativeCaptionlessWindowChrome.HitTestClientArea` with `captionHeight: 0`.
pub fn hit_test_client_area(x: i32, y: i32, width: i32, height: i32, border: i32) -> i32 {
    if width <= 0 || height <= 0 {
        return HTCLIENT;
    }
    let border = border.max(1).min(width / 2).min(height / 2);
    let left = x < border;
    let right = x >= width - border;
    let top = y < border;
    let bottom = y >= height - border;
    match (top, bottom, left, right) {
        (true, _, true, _) => HTTOPLEFT,
        (true, _, _, true) => HTTOPRIGHT,
        (_, true, true, _) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTTOP,
        (_, true, _, _) => HTBOTTOM,
        (_, _, true, _) => HTLEFT,
        (_, _, _, true) => HTRIGHT,
        _ => HTCLIENT,
    }
}

/// Install WM_NCHITTEST on the host HWND only, like C#'s HwndSource hook.
/// Tauri's TAURI_DRAG_RESIZE_WINDOW child overlays the WebView and returns
/// HTTRANSPARENT in the client area. Subclassing it to return HTCLIENT consumes
/// clicks before they can reach the page; Chromium children also own their hit tests.
pub fn install_html_titlebar_passthrough(hwnd: isize) {
    if hwnd == 0 || is_headless() {
        return;
    }
    #[cfg(windows)]
    unsafe {
        install_root(windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void));
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// Read the native move/size loop without calling a window proc or the UI thread.
/// Only HWNDs installed above are tracked, so unrelated windows never suspend hiding.
pub fn is_in_move_size(hwnd: isize) -> bool {
    if let Some(states) = INPUT_CHROME_SNAPSHOT.with(|snapshot| snapshot.get()) {
        return states.iter().find(|state| state.hwnd == hwnd).is_some_and(|state| state.move_size);
    }
    #[cfg(windows)]
    { win::is_in_move_size(hwnd) }
    #[cfg(not(windows))]
    { let _ = hwnd; false }
}

/// Observe native caption or move/size interaction without coupling chrome to
/// host state. Register after installing the root hook, on its owning UI thread.
/// The callback runs outside the chrome registry lock and receives transitions only.
pub fn set_caption_drag_observer(hwnd: isize, observer: impl Fn(bool) + Send + Sync + 'static) {
    if hwnd == 0 || is_headless() { return; }
    #[cfg(windows)]
    win::set_caption_drag_observer(hwnd, std::sync::Arc::new(observer));
    #[cfg(not(windows))]
    { let _ = (hwnd, observer); }
}

/// Actual root-window activation, independent of WebView GotFocus/LostFocus.
/// None means this HWND has no native chrome registration (headless/non-Windows).
pub fn is_active(hwnd: isize) -> Option<bool> {
    if let Some(states) = INPUT_CHROME_SNAPSHOT.with(|snapshot| snapshot.get()) {
        return states.iter().find(|state| state.hwnd == hwnd).and_then(|state| state.active);
    }
    #[cfg(windows)]
    { win::is_active(hwnd) }
    #[cfg(not(windows))]
    { let _ = hwnd; None }
}

/// C# `WindowCaptionDragInterop.BeginCaptionDrag`. Call on the owning UI thread
/// so `ReleaseCapture` releases the WebView's mouse capture before posting the drag.
pub fn begin_caption_drag(hwnd: isize) {
    if let Err(error) = try_begin_caption_drag(hwnd) {
        tracing::debug!(%error, "Native caption drag unavailable");
    }
}

/// Report whether a native drag message was posted for this still-held press.
pub fn try_begin_caption_drag(hwnd: isize) -> Result<bool, String> {
    if is_headless() { return Ok(false); }
    if hwnd == 0 { return Err("Caption drag window is unavailable".into()); }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON};
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetCursorPos, IsWindow, IsWindowVisible, WindowFromPoint, GA_ROOT};

        let handle = HWND(hwnd as *mut core::ffi::c_void);
        if !IsWindow(handle).as_bool() || !IsWindowVisible(handle).as_bool() {
            return Err("Caption drag window is no longer visible".into());
        }
        // An IPC request can arrive after release, or while Windows already
        // owns this press. Never start another modal move loop in either case.
        if is_in_move_size(hwnd) || GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 { return Ok(false); }
        let mut pt = windows::Win32::Foundation::POINT::default();
        GetCursorPos(&mut pt).map_err(|error| error.to_string())?;
        // A delayed IPC from an earlier press must not capture a new press in
        // another application just because the left button is down again.
        if GetAncestor(WindowFromPoint(pt), GA_ROOT) != handle { return Ok(false); }
        let _ = ReleaseCapture();
        post_caption_drag_position(handle, pt.x, pt.y)?;
        return Ok(true);
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
    #[cfg(not(windows))]
    Ok(false)
}

/// Start from the client point captured by pointerdown, before asynchronous IPC
/// allows the desktop cursor to move away from the original grab position.
pub fn try_begin_caption_drag_at_client(hwnd: isize, x: f64, y: f64, device_pixel_ratio: f64) -> Result<bool, String> {
    if is_headless() { return Ok(false); }
    if hwnd == 0 { return Err("Caption drag window is unavailable".into()); }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible};
        let handle = HWND(hwnd as *mut core::ffi::c_void);
        if !IsWindow(handle).as_bool() || !IsWindowVisible(handle).as_bool() {
            return Err("Caption drag window is no longer visible".into());
        }
        if is_in_move_size(hwnd) || GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 { return Ok(false); }
        // The held pointer can already be outside the owner during a fast
        // drag. Its original client point, rather than WindowFromPoint now,
        // identifies where Windows must anchor this caption movement.
        post_caption_drag_client_position(handle, x, y, device_pixel_ratio, true)?;
        return Ok(true);
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, x, y, device_pixel_ratio);
        Ok(false)
    }
}

#[cfg(any(windows, test))]
fn caption_drag_client_position(x: f64, y: f64, device_pixel_ratio: f64, width: i32, height: i32) -> Result<(i32, i32), String> {
    if ![x, y, device_pixel_ratio].iter().all(|value| value.is_finite())
        || device_pixel_ratio <= 0.0 || device_pixel_ratio > 16.0 || width <= 0 || height <= 0 {
        return Err("Caption drag client geometry is invalid".into());
    }
    let (x, y) = (x * device_pixel_ratio, y * device_pixel_ratio);
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0
        || x >= f64::from(width) || y >= f64::from(height) {
        return Err("Caption drag point is outside the window client area".into());
    }
    // A valid fractional position in the last device pixel can round to the
    // next pixel; retain it within the client rectangle.
    Ok(((x.round() as i32).min(width - 1), (y.round() as i32).min(height - 1)))
}

#[cfg(windows)]
unsafe fn post_caption_drag_client_position(hwnd: windows::Win32::Foundation::HWND, x: f64, y: f64, device_pixel_ratio: f64, release_capture: bool) -> Result<(), String> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
    let mut bounds = RECT::default();
    GetClientRect(hwnd, &mut bounds).map_err(|error| error.to_string())?;
    let (x, y) = caption_drag_client_position(x, y, device_pixel_ratio, bounds.right - bounds.left, bounds.bottom - bounds.top)?;
    let mut point = POINT { x, y };
    if !ClientToScreen(hwnd, &mut point).as_bool() { return Err("Cannot locate caption drag client point".into()); }
    // Invalid or stale geometry must not release the WebView's existing mouse
    // capture. Hidden message fixtures use this same path without capture changes.
    if release_capture { let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture(); }
    post_caption_drag_position(hwnd, point.x, point.y)
}

#[cfg(windows)]
unsafe fn post_caption_drag_position(hwnd: windows::Win32::Foundation::HWND, x: i32, y: i32) -> Result<(), String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_NCLBUTTONDOWN};
    PostMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(caption_drag_position(x, y)))
        .map_err(|error| error.to_string())
}

/// WM_NCLBUTTONDOWN expects coordinate bits, never a pointer to a POINTS.
/// Preserve each signed 16-bit screen coordinate, including negative monitors.
#[cfg(any(windows, test))]
fn caption_drag_position(x: i32, y: i32) -> isize {
    (((y as u16 as u32) << 16) | (x as u16 as u32)) as isize
}

#[cfg(windows)]
mod win {
    use super::hit_test_client_area;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GetAncestor, GetClientRect, GetForegroundWindow, GetWindowLongPtrW,
        SetWindowLongPtrW, GA_ROOT, GWL_STYLE, GWLP_WNDPROC, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCLBUTTONDBLCLK, WM_NCRBUTTONDOWN, WM_NCRBUTTONUP, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_ACTIVATE, WA_INACTIVE, WS_SIZEBOX,
    };

    type CaptionDragObserver = Arc<dyn Fn(bool) + Send + Sync + 'static>;

    #[derive(Clone)]
    struct NativeChromeState {
        original_proc: isize,
        move_size_active: bool,
        active: bool,
        caption_drag_active: bool,
        caption_drag_observer: Option<CaptionDragObserver>,
    }

    fn orig_procs() -> &'static Mutex<HashMap<isize, NativeChromeState>> {
        static ORIG: OnceLock<Mutex<HashMap<isize, NativeChromeState>>> = OnceLock::new();
        ORIG.get_or_init(|| Mutex::new(HashMap::new()))
    }

    #[cfg(test)]
    std::thread_local! {
        // Routing fixtures replace only the native caption sink, so they can
        // inspect the installed HWND hook without entering a desktop move loop.
        pub(super) static CAPTION_DEFAULT_TEST_HOOK: std::cell::Cell<Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn try_input_snapshot(hwnds: [isize; 3]) -> Option<[super::InputChromeState; 3]> {
        let states = orig_procs().try_lock()?;
        Some(hwnds.map(|hwnd| super::InputChromeState {
            hwnd,
            move_size: states.get(&hwnd).is_some_and(|state| state.move_size_active),
            active: states.get(&hwnd).map(|state| state.active),
        }))
    }

    #[cfg(test)]
    pub(super) fn with_registry_held_for_test<T>(work: impl FnOnce() -> T) -> T {
        let _states = orig_procs().lock();
        work()
    }

    pub fn is_in_move_size(hwnd: isize) -> bool {
        if crate::hooks::is_input_callback() {
            return orig_procs().try_lock()
                .map(|states| states.get(&hwnd).is_some_and(|state| state.move_size_active))
                .unwrap_or(true);
        }
        orig_procs().lock().get(&hwnd).is_some_and(|state| state.move_size_active)
    }

    pub fn is_active(hwnd: isize) -> Option<bool> {
        if crate::hooks::is_input_callback() {
            return orig_procs().try_lock()
                .and_then(|states| states.get(&hwnd).map(|state| state.active));
        }
        orig_procs().lock().get(&hwnd).map(|state| state.active)
    }

    pub fn set_caption_drag_observer(hwnd: isize, observer: CaptionDragObserver) {
        if let Some(state) = orig_procs().lock().get_mut(&hwnd) {
            state.caption_drag_observer = Some(observer);
        }
    }

    fn notify_caption_drag(hwnd: isize, active: bool) {
        let observer = {
            let mut map = orig_procs().lock();
            let Some(state) = map.get_mut(&hwnd) else { return; };
            if state.caption_drag_active == active { return; }
            state.caption_drag_active = active;
            state.caption_drag_observer.clone()
        };
        // Observers may query this HWND or dispatch work that re-enters its
        // window proc. Never retain the registry guard across a callback.
        if let Some(observer) = observer { observer(active); }
    }

    pub unsafe fn install_root(hwnd: HWND) {
        subclass(hwnd);
    }

    unsafe fn subclass(hwnd: HWND) {
        if hwnd.0.is_null() {
            return;
        }
        let key = hwnd.0 as isize;
        let mut map = orig_procs().lock();
        if map.contains_key(&key) {
            return;
        }
        let prev = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, chrome_wndproc as usize as isize);
        map.insert(key, NativeChromeState {
            original_proc: prev, move_size_active: false, active: GetForegroundWindow() == hwnd,
            caption_drag_active: false, caption_drag_observer: None,
        });
    }

    unsafe extern "system" fn chrome_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let key = hwnd.0 as isize;
        if msg == WM_NCHITTEST {
            return LRESULT(nchittest(hwnd, lparam) as isize);
        }
        if wparam.0 == super::HTCAPTION as usize
            && matches!(msg, WM_NCLBUTTONDBLCLK | WM_NCRBUTTONDOWN | WM_NCRBUTTONUP) {
            // CSS caption regions provide movement only, matching the existing
            // HTML title bar. Do not add maximize or a Windows system menu.
            return LRESULT(0);
        }
        if msg == WM_NCLBUTTONDOWN && wparam.0 == super::HTCAPTION as usize {
            // WebView2's native CSS caption press already carries the real
            // input message position. Tao inserts a synthetic WM_MOUSEMOVE
            // here; skip that extra event and enter Windows' caption handling.
            notify_caption_drag(key, true);
            #[cfg(test)]
            let result = if let Some(procedure) = CAPTION_DEFAULT_TEST_HOOK.with(|hook| hook.get()) {
                procedure(hwnd, msg, wparam, lparam)
            } else { DefWindowProcW(hwnd, msg, wparam, lparam) };
            #[cfg(not(test))]
            let result = DefWindowProcW(hwnd, msg, wparam, lparam);
            // A rejected press can return without ENTER/EXIT. Clear the host
            // interaction even then; a real EXIT is deduplicated above.
            notify_caption_drag(key, false);
            return result;
        }
        if msg == WM_ENTERSIZEMOVE || msg == WM_EXITSIZEMOVE {
            if let Some(state) = orig_procs().lock().get_mut(&key) {
                state.move_size_active = msg == WM_ENTERSIZEMOVE;
            }
            // Native CSS regions may enter via SC_MOVE rather than this root's
            // caption press. Observe Windows' loop in either path.
            notify_caption_drag(key, msg == WM_ENTERSIZEMOVE);
        }
        if msg == WM_ACTIVATE {
            if let Some(state) = orig_procs().lock().get_mut(&key) {
                state.active = (wparam.0 & 0xffff) != WA_INACTIVE as usize;
            }
        }
        if msg == WM_NCDESTROY {
            // Never hold our registry lock while calling another window proc:
            // destruction may re-enter a subclass on this UI thread.
            let orig = orig_procs().lock().remove(&key);
            if let Some(orig) = orig {
                if orig.caption_drag_active {
                    if let Some(observer) = orig.caption_drag_observer { observer(false); }
                }
                return call_orig(orig.original_proc, hwnd, msg, wparam, lparam);
            }
        }
        let orig = orig_procs().lock().get(&key).map(|state| state.original_proc).unwrap_or(0);
        call_orig(orig, hwnd, msg, wparam, lparam)
    }

    unsafe fn call_orig(
        orig: isize,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if orig == 0 {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        CallWindowProcW(Some(std::mem::transmute(orig)), hwnd, msg, wparam, lparam)
    }

    fn nchittest(hwnd: HWND, lparam: LPARAM) -> i32 {
        unsafe {
            let mut root = GetAncestor(hwnd, GA_ROOT);
            if root.0.is_null() {
                root = hwnd;
            }
            // Folded windows disable resizing, including our custom border hit tests.
            if GetWindowLongPtrW(root, GWL_STYLE) & WS_SIZEBOX.0 as isize == 0 {
                return super::HTCLIENT;
            }
            let x = (lparam.0 as i32) as i16 as i32;
            let y = ((lparam.0 >> 16) as i32) as i16 as i32;
            let mut pt = POINT { x, y };
            let _ = ScreenToClient(root, &mut pt);
            let mut rc = RECT::default();
            if GetClientRect(root, &mut rc).is_err() {
                return super::HTCLIENT;
            }
            let dpi = GetDpiForWindow(root).max(96);
            let border = ((super::RESIZE_BORDER_DIP as u32 * dpi + 48) / 96) as i32;
            hit_test_client_area(pt.x, pt.y, rc.right - rc.left, rc.bottom - rc.top, border.max(1))
        }
    }
}

#[cfg(windows)]
use win::install_root;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn native_caption_press_bypasses_original_proc_without_changing_other_window_routes() {
        // Chrome installation locks the process-wide registry other hosts try-lock.
        let _input_globals = crate::hooks::input_globals_test::exclusive();
        use std::cell::Cell;
        use std::sync::{Arc, Mutex};
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        #[derive(Default)]
        struct Probe {
            caption_sink: Cell<usize>,
            enter_loop: Cell<bool>,
            nonclient_down: Cell<usize>,
            nonclient_other: Cell<usize>,
            client_down: Cell<usize>,
            mouse_move: Cell<usize>,
            size: Cell<usize>,
        }
        unsafe fn probe(hwnd: HWND) -> Option<&'static Probe> {
            (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe).as_ref()
        }
        unsafe extern "system" fn original(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            if let Some(probe) = probe(hwnd) {
                let counter = match msg {
                    WM_NCLBUTTONDOWN => Some(&probe.nonclient_down),
                    WM_NCLBUTTONDBLCLK | WM_NCRBUTTONDOWN | WM_NCRBUTTONUP => Some(&probe.nonclient_other),
                    WM_LBUTTONDOWN => Some(&probe.client_down),
                    WM_MOUSEMOVE => Some(&probe.mouse_move),
                    WM_SIZE => Some(&probe.size),
                    _ => None,
                };
                if let Some(counter) = counter { counter.set(counter.get() + 1); return LRESULT(0); }
            }
            // Directed fixture activation must not change desktop focus.
            if msg == WM_ACTIVATE { return LRESULT(0); }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        unsafe extern "system" fn caption_sink(hwnd: HWND, msg: u32, wp: WPARAM, _lp: LPARAM) -> LRESULT {
            assert_eq!(msg, WM_NCLBUTTONDOWN);
            assert_eq!(wp.0, super::HTCAPTION as usize);
            let probe = probe(hwnd).unwrap();
            probe.caption_sink.set(probe.caption_sink.get() + 1);
            if probe.enter_loop.get() {
                // Exercise nested native notifications through the installed
                // root hook, without invoking the desktop's modal move loop.
                for message in [WM_ENTERSIZEMOVE, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_EXITSIZEMOVE] {
                    SendMessageW(hwnd, message, WPARAM(0), LPARAM(0));
                }
            }
            LRESULT(0)
        }
        struct Fixture { hwnd: HWND, instance: HINSTANCE }
        impl Drop for Fixture {
            fn drop(&mut self) {
                super::win::CAPTION_DEFAULT_TEST_HOOK.with(|hook| hook.set(None));
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                    let _ = UnregisterClassW(w!("ClipHostCaptionRouteFixture"), self.instance);
                }
            }
        }
        unsafe {
            let foreground = GetForegroundWindow();
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            assert_ne!(RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(original), hInstance: instance,
                lpszClassName: w!("ClipHostCaptionRouteFixture"), ..Default::default() }), 0);
            // Keep the probe alive through HWND destruction, including unwind.
            let state = Box::<Probe>::default();
            let fixture = Fixture { hwnd: CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("ClipHostCaptionRouteFixture"), w!("hidden caption routing fixture"), WS_POPUP | WS_SIZEBOX,
                -30000, -30000, 420, 520, None, None, instance, None).unwrap(), instance };
            SetWindowLongPtrW(fixture.hwnd, GWLP_USERDATA, &*state as *const Probe as isize);
            super::win::install_root(fixture.hwnd);
            let hwnd = fixture.hwnd.0 as isize;
            let transitions = Arc::new(Mutex::new(Vec::new()));
            let observed = transitions.clone();
            super::win::set_caption_drag_observer(hwnd, Arc::new(move |active| {
                // Reentrant registry access proves callbacks run after releasing
                // its lock. Record the actual native-loop state at each edge.
                let native_loop = super::is_in_move_size(hwnd);
                observed.lock().unwrap().push((active, native_loop));
            }));
            super::win::CAPTION_DEFAULT_TEST_HOOK.with(|hook| hook.set(Some(caption_sink)));
            let point = LPARAM(caption_drag_position(-29900, -29980));
            state.enter_loop.set(true);
            SendMessageW(fixture.hwnd, WM_NCLBUTTONDOWN, WPARAM(super::HTCAPTION as usize), point);
            assert_eq!(state.caption_sink.get(), 1);
            assert_eq!(state.nonclient_down.get(), 0, "caption press reached Tao/original wndproc");
            assert_eq!(*transitions.lock().unwrap(), [(true, false), (false, false)],
                "nested ENTER/EXIT and caption return must produce only one interaction pair");
            state.enter_loop.set(false);
            SendMessageW(fixture.hwnd, WM_NCLBUTTONDOWN, WPARAM(super::HTCAPTION as usize), point);
            assert_eq!(state.caption_sink.get(), 2);
            assert_eq!(*transitions.lock().unwrap(), [(true, false), (false, false), (true, false), (false, false)],
                "caption handling that returns without entering the native loop must clear interaction");
            for message in [WM_NCLBUTTONDBLCLK, WM_NCRBUTTONDOWN, WM_NCRBUTTONUP] {
                SendMessageW(fixture.hwnd, message, WPARAM(super::HTCAPTION as usize), point);
            }
            assert_eq!(state.nonclient_other.get(), 0, "caption double/right click reached a native default handler");
            assert_eq!(state.caption_sink.get(), 2);
            for message in [WM_NCLBUTTONDBLCLK, WM_NCRBUTTONDOWN, WM_NCRBUTTONUP] {
                SendMessageW(fixture.hwnd, message, WPARAM(super::HTLEFT as usize), point);
            }
            assert_eq!(state.nonclient_other.get(), 3, "non-caption double/right clicks lost their original routes");
            SendMessageW(fixture.hwnd, WM_NCLBUTTONDOWN, WPARAM(super::HTLEFT as usize), point);
            SendMessageW(fixture.hwnd, WM_NCLBUTTONDOWN, WPARAM(super::HTCLIENT as usize), point);
            SendMessageW(fixture.hwnd, WM_LBUTTONDOWN, WPARAM(0), LPARAM(0));
            SendMessageW(fixture.hwnd, WM_SIZE, WPARAM(0), LPARAM(0));
            assert_eq!(state.nonclient_down.get(), 2, "non-caption routes bypassed their original handler");
            assert_eq!(state.client_down.get(), 1);
            assert_eq!(state.size.get(), 1);
            SendMessageW(fixture.hwnd, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            assert!(super::is_in_move_size(fixture.hwnd.0 as isize));
            SendMessageW(fixture.hwnd, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            SendMessageW(fixture.hwnd, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
            assert!(!super::is_in_move_size(fixture.hwnd.0 as isize));
            SendMessageW(fixture.hwnd, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
            assert_eq!(*transitions.lock().unwrap(), [(true, false), (false, false), (true, false), (false, false),
                (true, true), (false, false)], "direct native-loop entry must also observe and deduplicate edges");
            SendMessageW(fixture.hwnd, WM_ACTIVATE, WPARAM(WA_ACTIVE as usize), LPARAM(0));
            assert_eq!(super::is_active(fixture.hwnd.0 as isize), Some(true));
            SendMessageW(fixture.hwnd, WM_ACTIVATE, WPARAM(WA_INACTIVE as usize), LPARAM(0));
            assert_eq!(super::is_active(fixture.hwnd.0 as isize), Some(false));
            let mut message = MSG::default();
            assert!(!PeekMessageW(&mut message, fixture.hwnd, WM_MOUSEMOVE, WM_MOUSEMOVE, PM_REMOVE).as_bool());
            assert_eq!(state.mouse_move.get(), 0);
            assert_eq!(GetForegroundWindow(), foreground);
            SendMessageW(fixture.hwnd, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            drop(fixture);
            assert!(!super::is_in_move_size(hwnd));
            assert_eq!(super::is_active(hwnd), None);
            assert_eq!(*transitions.lock().unwrap(), [(true, false), (false, false), (true, false), (false, false),
                (true, true), (false, false), (true, true), (false, false)],
                "destroying an active root must clean registration before its final callback");
            assert_eq!(GetForegroundWindow(), foreground);
        }
    }

    #[test]
    fn caption_drag_packs_signed_screen_coordinates() {
        for (x, y) in [(100, 200), (-1920, 120), (80, -1080), (-32768, 32767)] {
            let packed = caption_drag_position(x, y);
            assert_eq!((packed as u16 as i16 as i32, (packed >> 16) as u16 as i16 as i32), (x, y));
        }
        assert_eq!(caption_drag_position(-1920, -1080) as u32, 0xfbc8_f880);
    }

    #[test]
    fn caption_drag_client_point_preserves_fractional_dpr_and_rejects_invalid_geometry() {
        assert_eq!(caption_drag_client_position(12.4, 16.8, 1.25, 420, 520).unwrap(), (16, 21));
        assert_eq!(caption_drag_client_position(13.0, 10.0, 1.5, 420, 520).unwrap(), (20, 15));
        assert_eq!(caption_drag_client_position(335.9, 415.9, 1.25, 420, 520).unwrap(), (419, 519));
        for (x, y, dpr) in [(f64::NAN, 1.0, 1.0), (1.0, f64::INFINITY, 1.0), (1.0, 1.0, 0.0),
            (1.0, 1.0, -1.0), (1.0, 1.0, f64::NAN), (1.0, 1.0, 17.0),
            (-1.0, 1.0, 1.0), (1.0, -1.0, 1.0), (336.0, 1.0, 1.25), (1.0, 416.0, 1.25)] {
            assert!(caption_drag_client_position(x, y, dpr, 420, 520).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_caption_drag_posts_original_client_point_with_fractional_dpr() {
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, POINT};
        use windows::Win32::Graphics::Gdi::ClientToScreen;
        use windows::Win32::UI::WindowsAndMessaging::*;
        struct Fixture(HWND);
        impl Drop for Fixture {
            fn drop(&mut self) { unsafe { let _ = DestroyWindow(self.0); } }
        }
        unsafe {
            let fixture = Fixture(CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("ClipHost client grab point fixture"), WS_POPUP, -18000, -16000,
                420, 520, None, None, None, None).unwrap());
            // This expected physical point comes from the captured 12.4/16.8
            // CSS press at 125%, regardless of the live desktop cursor.
            let mut expected = POINT { x: 16, y: 21 };
            assert!(ClientToScreen(fixture.0, &mut expected).as_bool());
            post_caption_drag_client_position(fixture.0, 12.4, 16.8, 1.25, false).unwrap();
            let mut message = MSG::default();
            assert!(PeekMessageW(&mut message, fixture.0, WM_NCLBUTTONDOWN, WM_NCLBUTTONDOWN, PM_REMOVE).as_bool());
            let received = (message.lParam.0 as u16 as i16 as i32,
                (message.lParam.0 >> 16) as u16 as i16 as i32);
            assert_eq!(received, (expected.x, expected.y));
            assert_eq!(message.hwnd, fixture.0);
            assert_eq!(message.wParam.0, HTCAPTION as usize);
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_caption_drag_posts_screen_position_bits_to_owned_window() {
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, POINT};
        use windows::Win32::UI::WindowsAndMessaging::*;
        // Intercept only this hidden fixture's message. Do not dispatch the
        // caption press, move the desktop pointer, or start a modal move loop.
        struct Fixture(HWND);
        impl Drop for Fixture {
            fn drop(&mut self) { unsafe { let _ = DestroyWindow(self.0); } }
        }
        unsafe {
            let fixture = Fixture(CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("ClipHost caption message fixture"), WS_POPUP, -30000, -30000,
                100, 100, None, None, None, None).unwrap());
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor).unwrap();
            post_caption_drag_position(fixture.0, cursor.x, cursor.y).unwrap();
            let mut message = MSG::default();
            assert!(PeekMessageW(&mut message, fixture.0, WM_NCLBUTTONDOWN, WM_NCLBUTTONDOWN, PM_REMOVE).as_bool());
            assert_eq!(message.hwnd, fixture.0);
            assert_eq!(message.wParam.0, HTCAPTION as usize);
            let received_x = message.lParam.0 as u16 as i16;
            let received_y = (message.lParam.0 >> 16) as u16 as i16;
            assert_eq!((received_x, received_y), (cursor.x as i16, cursor.y as i16));
        }
    }

    #[test]
    fn title_band_is_client_not_caption() {
        assert_eq!(hit_test_client_area(100, 20, 420, 760, 8), HTCLIENT);
        assert_eq!(hit_test_client_area(100, 0, 420, 760, 8), HTTOP);
        assert_eq!(hit_test_client_area(0, 20, 420, 760, 8), HTLEFT);
        assert_eq!(hit_test_client_area(0, 0, 420, 760, 8), HTTOPLEFT);
    }

    #[cfg(windows)]
    #[test]
    fn native_activation_distinguishes_webview_blur_from_leaving_host() {
        // Chrome installation locks the process-wide registry other hosts try-lock.
        let _input_globals = crate::hooks::input_globals_test::exclusive();
        use windows::core::{HSTRING, PCWSTR};
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;

        // Directed WM_ACTIVATE must not let DefWindowProc call SetFocus.
        unsafe extern "system" fn fixture_proc(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            if msg == WM_ACTIVATE {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        struct Fixture {
            hwnd: HWND,
            instance: HINSTANCE,
            class: HSTRING,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                    let _ = UnregisterClassW(&self.class, self.instance);
                }
            }
        }
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let host = crate::HostState::open(
            crate::paths::AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
            28965,
        )
        .unwrap();
        unsafe {
            let foreground_before = GetForegroundWindow();
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let class = HSTRING::from(format!("ClipHostActivation{}", std::process::id()));
            assert_ne!(
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(fixture_proc),
                    hInstance: instance,
                    lpszClassName: PCWSTR(class.as_ptr()),
                    ..Default::default()
                }),
                0
            );
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                &class,
                &class,
                WS_POPUP,
                -30000,
                -30000,
                100,
                100,
                None,
                None,
                instance,
                None,
            )
            .unwrap();
            let window = Fixture {
                hwnd,
                instance,
                class,
            };
            super::win::install_root(hwnd);
            host.set_host_hwnd(hwnd.0 as isize);
            host.chrome_web_action(&serde_json::json!({
                "kind":"clipboardHostChrome", "requestHostKeyboardFocus":true,
            }));
            assert_eq!(super::is_active(hwnd.0 as isize), Some(false));
            assert!(
                !host.hide_snapshot().is_active,
                "a queued focus request isn't native activation"
            );

            SendMessageW(hwnd, WM_ACTIVATE, WPARAM(WA_ACTIVE as usize), LPARAM(0));
            host.notify_activated();
            host.notify_deactivated();
            let internal_blur = host.hide_snapshot();
            assert!(
                internal_blur.is_active && internal_blur.has_fake_focus,
                "WebView blur to the active root must not clear engagement"
            );
            // Reinstalling chrome must not reset the cached native state.
            super::win::install_root(hwnd);
            assert_eq!(super::is_active(hwnd.0 as isize), Some(true));

            SendMessageW(hwnd, WM_ACTIVATE, WPARAM(WA_INACTIVE as usize), LPARAM(0));
            host.notify_activated();
            assert!(
                !host.hide_snapshot().is_active,
                "native deactivation wins over a delayed WebView event"
            );
            host.notify_deactivated();
            assert!(!host.hide_snapshot().has_fake_focus);

            host.chrome_web_action(&serde_json::json!({
                "kind":"clipboardHostChrome", "requestHostKeyboardFocus":true,
            }));
            assert!(!host.hide_snapshot().has_fake_focus && !host.hide_snapshot().is_active,
                "duplicate input focus revived a natively deactivated window");

            // An overlay defers a real root deactivation. A delayed WebView
            // activation cannot cancel it while WM_ACTIVATE still says inactive.
            host.request_show();
            host.chrome_web_action(&serde_json::json!({
                "kind":"clipboardHostChrome", "requestHostKeyboardFocus":true,
            }));
            host.apply_window_action_for("setSpaOverlayOpen", true, "clipboard");
            host.notify_deactivated();
            assert!(host.hide_snapshot().has_fake_focus);
            host.notify_activated();
            assert!(!host.hide_snapshot().is_active);
            host.chrome_web_action(&serde_json::json!({
                "kind":"clipboardHostChrome", "requestHostKeyboardFocus":true,
            }));
            host.apply_window_action_for("setSpaOverlayOpen", false, "clipboard");
            host.poll_hide();
            assert!(!host.hide_snapshot().has_fake_focus,
                "delayed WebView activation erased deferred native deactivation");

            SendMessageW(
                hwnd,
                WM_ACTIVATE,
                WPARAM(WA_CLICKACTIVE as usize),
                LPARAM(0),
            );
            assert!(host.hide_snapshot().is_active);
            host.window_hide();
            host.notify_activated();
            assert!(
                !host.hide_snapshot().is_active,
                "late activation cannot revive a hidden host"
            );
            drop(window);
            assert_eq!(super::is_active(hwnd.0 as isize), None);
            assert_eq!(
                GetForegroundWindow(),
                foreground_before,
                "fixture must not change desktop focus"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_move_size_blocks_auto_dismiss_and_releases_after_exit() {
        // Chrome installation locks the process-wide registry other hosts try-lock.
        let _input_globals = crate::hooks::input_globals_test::exclusive();
        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::*;
        use crate::hide_policy::{evaluate, HostDismissAction, HostHideTrigger};
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let host = crate::HostState::open(crate::paths::AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
        // Only hidden, owned HWNDs and directed messages. No desktop mouse/keys.
        unsafe {
            let first = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("resize fixture"),
                WS_POPUP | WS_SIZEBOX, 0, 0, 420, 520, None, None, None, None).unwrap();
            let second = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("other fixture"),
                WS_POPUP, 0, 0, 420, 520, None, None, None, None).unwrap();
            super::win::install_root(first);
            super::win::install_root(second);
            host.set_host_hwnd(first.0 as isize);
            SendMessageW(first, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            let moving = host.hide_snapshot();
            host.set_window_runtime_ready(true);
            host.set_clipboard_web_view_ready(true);
            let swallowed = [0x31, 0x28, 0x0D, 0x1B].map(|vk| host.dispatch_low_level_key(crate::quick_paste::KeyEvent {
                vk, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
            }));
            host.set_host_hwnd(second.0 as isize);
            let other_moving = host.hide_snapshot().move_interaction_active;
            host.set_host_hwnd(first.0 as isize);
            SendMessageW(first, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
            let exited = host.hide_snapshot().move_interaction_active;
            // Panel input has its own no-focus session and outside-click path.
            host.favor_folder_create(&serde_json::json!({"title":"native panel fixture"})).unwrap();
            host.panel_show(&serde_json::json!({})).unwrap();
            host.set_host_hwnd(second.0 as isize);
            host.set_panel_hwnd(first.0 as isize);
            SendMessageW(first, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            host.notify_outside_click_at(99999, 99999);
            let panel_stayed = host.panel_window_state_dto()["panelVisible"].as_bool().unwrap();
            let panel_escape_swallowed = host.dispatch_low_level_key(crate::quick_paste::KeyEvent {
                vk: 0x1B, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 2,
            });
            SendMessageW(first, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
            host.notify_outside_click_at(99999, 99999);
            let panel_closed = !host.panel_window_state_dto()["panelVisible"].as_bool().unwrap();
            host.set_host_hwnd(first.0 as isize);
            SendMessageW(first, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
            DestroyWindow(first).unwrap();
            let destroyed = host.hide_snapshot().move_interaction_active;
            DestroyWindow(second).unwrap();
            assert!(moving.move_interaction_active, "native edge resize must guard auto-hide");
            assert!(swallowed.into_iter().all(|value| !value), "native resize owns keys");
            assert!(panel_stayed && !panel_escape_swallowed, "panel stays in the native resize loop");
            assert!(panel_closed, "outside-click dismissal resumes after native resize");
            assert!(!other_moving, "resizing another window cannot block this host");
            assert!(!exited, "end of the native loop restores normal hide decisions");
            assert!(!destroyed, "destroyed HWNDs cannot leave a stale guard");
            for trigger in [HostHideTrigger::Poll, HostHideTrigger::Pointer,
                HostHideTrigger::OutsideClick, HostHideTrigger::ForegroundChange] {
                let decision = evaluate(trigger, &moving);
                assert_eq!(decision.window_action, HostDismissAction::None);
                assert!(decision.path.iter().any(|p| p == "move-active"));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_resize_overlay_keeps_transparent_hit_test_after_host_install() {
        // Chrome installation locks the process-wide registry other hosts try-lock.
        let _input_globals = crate::hooks::input_globals_test::exclusive();
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;

        unsafe extern "system" fn transparent_proc(
            hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
        ) -> LRESULT {
            if msg == WM_NCHITTEST {
                LRESULT(HTTRANSPARENT as isize)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        // Hidden windows only: reproduce the native overlay contract without
        // showing UI, installing hooks, or touching the user's clipboard.
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let class = w!("ClipHostHitTestRegression");
            assert_ne!(RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(transparent_proc),
                hInstance: instance, lpszClassName: class, ..Default::default()
            }), 0);
            let root = CreateWindowExW(
                WINDOW_EX_STYLE::default(), class, w!("host"), WS_POPUP,
                0, 0, 420, 760, None, None, instance, None,
            ).unwrap();
            let child = CreateWindowExW(
                WINDOW_EX_STYLE::default(), class, w!("TAURI_DRAG_RESIZE_WINDOW"), WS_CHILD,
                0, 0, 420, 760, root, None, instance, None,
            ).unwrap();
            super::win::install_root(root);
            super::win::install_root(root); // page-load reinstallation is idempotent
            let point = LPARAM((30 << 16) | 100);
            let child_hit = SendMessageW(child, WM_NCHITTEST, WPARAM(0), point);
            let root_hit = SendMessageW(root, WM_NCHITTEST, WPARAM(0), point);
            let edge = LPARAM((30 << 16) | 1);
            let fixed_edge = SendMessageW(root, WM_NCHITTEST, WPARAM(0), edge);
            let style = GetWindowLongPtrW(root, GWL_STYLE);
            SetWindowLongPtrW(root, GWL_STYLE, style | WS_SIZEBOX.0 as isize);
            let resizable_edge = SendMessageW(root, WM_NCHITTEST, WPARAM(0), edge);
            DestroyWindow(root).unwrap();
            UnregisterClassW(class, instance).unwrap();
            assert_eq!(child_hit.0, HTTRANSPARENT as isize,
                "the resize overlay must pass client clicks through to the WebView");
            assert_eq!(root_hit.0, super::HTCLIENT as isize);
            assert_eq!(fixed_edge.0, super::HTCLIENT as isize);
            assert_eq!(resizable_edge.0, super::HTLEFT as isize);
        }
    }
}
