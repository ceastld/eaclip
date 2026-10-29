//! Only adapts the current GPUI root HWND; no child windows or other apps.

#[cfg(windows)]
use cliphost_core::i18n::tr;
use cliphost_core::{host::ChromeTask, HostState};

#[path = "caption_regions.rs"]
mod caption_regions;
#[path = "fold_geometry.rs"]
mod fold_geometry;
#[path = "clipboard_move.rs"]
mod clipboard_move;
#[path = "clipboard_resize.rs"]
mod clipboard_resize;
#[path = "editor_docking.rs"]
mod editor_docking;

/// Publish painted client geometry only. Native hit tests never borrow GPUI's
/// App or reconstruct a layout while Windows is deciding how to move a window.
pub fn record_caption_frame(window: &gpui::Window, bounds: gpui::Bounds<gpui::Pixels>) {
    let bounds = bounds.intersect(&window.content_mask().bounds);
    #[cfg(windows)]
    if let Ok(handle) = hwnd(window) {
        caption_regions::begin(
            handle.0 as isize,
            bounds,
            window.viewport_size(),
            window.scale_factor(),
        );
    }
    #[cfg(not(windows))]
    let _ = (window, bounds);
}

pub fn record_caption_exclusion(window: &gpui::Window, bounds: gpui::Bounds<gpui::Pixels>) {
    let bounds = bounds.intersect(&window.content_mask().bounds);
    if bounds.size.width <= gpui::px(0.0) || bounds.size.height <= gpui::px(0.0) {
        return;
    }
    #[cfg(windows)]
    if let Ok(handle) = hwnd(window) {
        caption_regions::exclude(handle.0 as isize, bounds);
    }
    #[cfg(not(windows))]
    let _ = (window, bounds);
}

#[cfg(windows)]
pub fn hwnd(window: &gpui::Window) -> Result<windows::Win32::Foundation::HWND, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{
        Foundation::HWND,
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    };
    let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window)
        .map_err(|error| error.to_string())?
        .as_raw()
    else {
        return Err(tr(
            "GPUI 未返回 Windows 窗口句柄。",
            "GPUI did not return a Windows window handle.",
        )
        .into());
    };
    let hwnd = HWND(handle.hwnd.get() as *mut _);
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != unsafe { GetCurrentProcessId() } || thread != unsafe { GetCurrentThreadId() } {
        return Err(tr(
            "原生窗口不属于当前界面线程。",
            "Native window is not owned by this UI thread.",
        )
        .into());
    }
    Ok(hwnd)
}

#[cfg(windows)]
fn raw_set_no_activate(
    hwnd: windows::Win32::Foundation::HWND,
    no_activate: bool,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{GetLastError, SetLastError, WIN32_ERROR},
        UI::WindowsAndMessaging::*,
    };
    unsafe {
        retain_style_policy(hwnd, no_activate)?;
        let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let bit = WS_EX_NOACTIVATE.0 as isize;
        let style = if no_activate { old | bit } else { old & !bit };
        if style == old {
            return Ok(());
        }
        SetLastError(WIN32_ERROR(0));
        if SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) == 0 && GetLastError().0 != 0 {
            return Err(cliphost_core::trf!(
                "设置原生窗口样式失败：{:?}",
                "SetWindowLongPtrW failed: {:?}",
                GetLastError()
            ));
        }
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Physical cursor position, for popups that open at the pointer's screen point.
#[cfg(windows)]
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = windows::Win32::Foundation::POINT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}
#[cfg(not(windows))]
pub fn cursor_position() -> Option<(i32, i32)> {
    None
}

/// Convert a painted client rectangle into physical screen pixels. Used by
/// click-open popups so their anchor and companion trigger use the same bounds.
#[cfg(windows)]
pub fn client_rect_on_screen(window: &gpui::Window, bounds: gpui::Bounds<gpui::Pixels>) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::{Foundation::POINT, Graphics::Gdi::ClientToScreen};
    let hwnd = hwnd(window).ok()?;
    let scale = window.scale_factor();
    let mut origin = POINT { x: 0, y: 0 };
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() { return None; }
    let left = (f32::from(bounds.left()) * scale).floor() as i32;
    let top = (f32::from(bounds.top()) * scale).floor() as i32;
    let right = (f32::from(bounds.right()) * scale).ceil() as i32;
    let bottom = (f32::from(bounds.bottom()) * scale).ceil() as i32;
    (right > left && bottom > top).then_some((origin.x + left, origin.y + top, right - left, bottom - top))
}

#[cfg(not(windows))]
pub fn client_rect_on_screen(_: &gpui::Window, _: gpui::Bounds<gpui::Pixels>) -> Option<(i32, i32, i32, i32)> { None }

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[cfg(windows)]
const HOST_STYLE_SUBCLASS: usize = 0xBC01;

#[cfg(windows)]
thread_local! {
    static HIT_TEST_PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // Only the clipboard adapter registers a folded height. Settings, panels
    // and other windows keep their own GPUI constraints.
    static FOLDED_CLIENT_HEIGHT: std::cell::RefCell<std::collections::HashMap<isize, f64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    // Only explicit region callers opt in. Keeping the last physical shape
    // avoids SetWindowRgn messages turning a painted-bounds update into a loop.
    static OVERLAY_REGIONS: std::cell::RefCell<std::collections::HashMap<isize, Vec<(i32, i32, i32, i32)>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Core window probes (`WindowFromPoint`) run while GPUI holds its App borrow
/// and synchronously send WM_NCHITTEST to our own windows. Answer those as
/// client hits without re-entering GPUI; window identity is all they need.
pub fn with_hit_test_probe<T>(probe: impl FnOnce() -> T) -> T {
    #[cfg(windows)]
    {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                HIT_TEST_PROBE.with(|flag| flag.set(self.0));
            }
        }
        let _reset = Reset(HIT_TEST_PROBE.with(|flag| flag.replace(true)));
        probe()
    }
    #[cfg(not(windows))]
    probe()
}

#[cfg(windows)]
unsafe fn retain_style_policy(
    hwnd: windows::Win32::Foundation::HWND,
    no_activate: bool,
) -> Result<(), String> {
    use windows::Win32::UI::Shell::{GetWindowSubclass, SetWindowSubclass};
    let mut current = 0;
    if GetWindowSubclass(
        hwnd,
        Some(host_style_proc),
        HOST_STYLE_SUBCLASS,
        Some(&mut current),
    )
    .as_bool()
        && current != 2
        && current != usize::from(no_activate)
        && !SetWindowSubclass(
            hwnd,
            Some(host_style_proc),
            HOST_STYLE_SUBCLASS,
            usize::from(no_activate),
        )
        .as_bool()
    {
        return Err(tr(
            "无法更新原生工具窗口策略。",
            "Cannot update native tool window policy.",
        )
        .into());
    }
    Ok(())
}

/// Match the existing Tauri tool-window policy on the GPUI root only. A style
/// rewrite cannot expose a taskbar button or silently re-enable activation.
#[cfg(windows)]
fn raw_configure_host(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::core::BOOL;
    use windows::Win32::{
        Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED},
        UI::{Shell::SetWindowSubclass, WindowsAndMessaging::*},
    };
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if !SetWindowSubclass(
            hwnd,
            Some(host_style_proc),
            HOST_STYLE_SUBCLASS,
            usize::from(style & WS_EX_NOACTIVATE.0 as isize != 0),
        )
        .as_bool()
        {
            return Err(tr(
                "无法保留原生工具窗口样式。",
                "Cannot retain native tool window style.",
            )
            .into());
        }
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize),
        );
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(|error| error.to_string())?;
        let disabled = BOOL(1);
        // Older Windows may not support a DWM attribute; window identity and
        // no-activate remain enforced independently of optional appearance.
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const _ as *const _,
            std::mem::size_of_val(&disabled) as u32,
        );
    }
    Ok(())
}

#[cfg(windows)]
const OVERLAY_FRAME_SUBCLASS: usize = 0xBC02;

/// Transparent overlays draw their own card and shadow. GPUI keeps a captioned
/// frame with a frame-thick non-client inset, so Windows 11 DWM outlines the
/// transparent margin with a 1px border, shadow and rounded corners. Make the
/// overlay a plain popup whose client area covers the whole placed rect.
#[cfg(windows)]
unsafe fn raw_frameless_overlay(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::{
        Graphics::Dwm::*,
        UI::{Shell::SetWindowSubclass, WindowsAndMessaging::*},
    };
    if !SetWindowSubclass(hwnd, Some(overlay_frame_proc), OVERLAY_FRAME_SUBCLASS, 0).as_bool() {
        return Err(tr(
            "无法移除原生浮层边框。",
            "Cannot remove the native overlay frame.",
        )
        .into());
    }
    let old = GetWindowLongPtrW(hwnd, GWL_STYLE);
    let keep = (WS_VISIBLE | WS_DISABLED | WS_CLIPSIBLINGS | WS_CLIPCHILDREN).0 as isize;
    let style = (old & keep) | WS_POPUP.0 as isize;
    if style != old {
        SetWindowLongPtrW(hwnd, GWL_STYLE, style);
    }
    // Optional appearance: older Windows ignores attributes it does not know.
    let policy = DWMNCRP_DISABLED;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_NCRENDERING_POLICY,
        &policy as *const _ as *const _,
        std::mem::size_of_val(&policy) as u32,
    );
    let corner = DWMWCP_DONOTROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &corner as *const _ as *const _,
        std::mem::size_of_val(&corner) as u32,
    );
    let border = DWMWA_COLOR_NONE;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &border as *const _ as *const _,
        std::mem::size_of_val(&border) as u32,
    );
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn overlay_frame_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
    id: usize,
    _: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::{
        Foundation::LRESULT,
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass},
            WindowsAndMessaging::*,
        },
    };
    // Whole window is client: skip GPUI's custom-titlebar frame insets.
    if message == WM_NCCALCSIZE && wp.0 != 0 {
        return LRESULT(0);
    }
    // These frameless overlays have no native caption or resize edges. A
    // synchronous hit test (including WindowFromPoint on another thread) must
    // not call back into GPUI while its App or the host state is borrowed.
    if message == WM_NCHITTEST {
        return LRESULT(HTCLIENT as isize);
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(overlay_frame_proc), id);
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

#[cfg(windows)]
unsafe extern "system" fn host_style_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
    id: usize,
    no_activate: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::{
        Foundation::LRESULT,
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass},
            WindowsAndMessaging::*,
        },
    };
    if let Some(result) = clipboard_move::handle_message(hwnd, message, wp, lp) {
        return match result {
            Ok(result) => result,
            Err(error) => {
                NATIVE_QUEUE.with(|queues| queues.borrow_mut().entry(hwnd.0 as isize)
                    .or_default().errors.push_back(error));
                LRESULT(0)
            }
        };
    }
    if message == NATIVE_DISPATCH {
        dispatch_native(hwnd);
        return LRESULT(0);
    }
    if message == WM_GETMINMAXINFO && lp.0 != 0 {
        let folded_height =
            FOLDED_CLIENT_HEIGHT.with(|heights| heights.borrow().get(&(hwnd.0 as isize)).copied());
        if let Some(folded_height) = folded_height {
            // Let GPUI populate width, max bounds and the normal frame policy
            // first; its static minimum height must not clamp Fold to 250 DIP.
            let result = DefSubclassProc(hwnd, message, wp, lp);
            let mut outer = windows::Win32::Foundation::RECT::default();
            let mut client = windows::Win32::Foundation::RECT::default();
            if GetWindowRect(hwnd, &mut outer).is_ok() && GetClientRect(hwnd, &mut client).is_ok() {
                if let Some(height) = fold_geometry::minimum_outer_height(
                    folded_height,
                    windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd),
                    i64::from(outer.bottom) - i64::from(outer.top),
                    i64::from(client.bottom) - i64::from(client.top),
                ) {
                    (*(lp.0 as *mut MINMAXINFO)).ptMinTrackSize.y = height;
                }
            }
            return result;
        }
    }
    if message == WM_NCHITTEST && HIT_TEST_PROBE.with(std::cell::Cell::get) {
        return LRESULT(HTCLIENT as isize);
    }
    if let Some(result) = clipboard_resize::handle_message(
        hwnd, message, wp, lp, caption_regions::hit,
    ) {
        return result;
    }
    if message == WM_NCHITTEST {
        let mut point = windows::Win32::Foundation::POINT {
            x: lp.0 as u16 as i16 as i32,
            y: (lp.0 >> 16) as u16 as i16 as i32,
        };
        let mut client = windows::Win32::Foundation::RECT::default();
        if windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point).as_bool()
            && GetClientRect(hwnd, &mut client).is_ok()
        {
            let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
            if let Some(caption) = caption_regions::hit(
                hwnd.0 as isize,
                point.x,
                point.y,
                client.right,
                client.bottom,
                dpi,
            ) {
                if GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_THICKFRAME.0 as isize != 0 {
                    let resize = cliphost_core::window_chrome::hit_test_client_area(
                        point.x,
                        point.y,
                        client.right,
                        client.bottom,
                        ((cliphost_core::window_chrome::RESIZE_BORDER_DIP as f64 * dpi as f64
                            / 96.0)
                            .round() as i32)
                            .max(1),
                    );
                    if resize != cliphost_core::window_chrome::HTCLIENT {
                        return LRESULT(resize as isize);
                    }
                }
                return LRESULT(if caption {
                    HTCAPTION as isize
                } else {
                    HTCLIENT as isize
                });
            }
        }
    }
    if matches!(message, WM_SIZE | WM_DPICHANGED) {
        caption_regions::invalidate(hwnd.0 as isize);
    }
    if message == WM_MOUSEACTIVATE
        && GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize != 0
    {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_STYLECHANGING
        && wp.0 as isize == GWL_EXSTYLE.0 as isize
        && lp.0 != 0
        && no_activate != 2
    {
        let styles = &mut *(lp.0 as *mut STYLESTRUCT);
        styles.styleNew = (styles.styleNew | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0;
        if no_activate != 0 {
            styles.styleNew |= WS_EX_NOACTIVATE.0;
        } else {
            styles.styleNew &= !WS_EX_NOACTIVATE.0;
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        caption_regions::remove(hwnd.0 as isize);
        editor_docking::forget_frame(hwnd);
        OVERLAY_REGIONS.with(|regions| {
            regions.borrow_mut().remove(&(hwnd.0 as isize));
        });
        FOLDED_CLIENT_HEIGHT.with(|heights| {
            heights.borrow_mut().remove(&(hwnd.0 as isize));
        });
        NATIVE_QUEUE.with(|queues| {
            queues.borrow_mut().remove(&(hwnd.0 as isize));
        });
        let _ = RemoveWindowSubclass(hwnd, Some(host_style_proc), id);
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

#[cfg(windows)]
pub fn geometry(window: &gpui::Window) -> Result<Geometry, String> {
    use windows::Win32::{
        Foundation::RECT,
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{GetClientRect, GetWindowRect},
        },
    };
    let hwnd = hwnd(window)?;
    let mut outer = RECT::default();
    let mut client = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut outer).map_err(|error| error.to_string())?;
        GetClientRect(hwnd, &mut client).map_err(|error| error.to_string())?;
    }
    Ok(Geometry {
        left: outer.left,
        top: outer.top,
        width: (client.right - client.left).max(0) as u32,
        height: (client.bottom - client.top).max(0) as u32,
        // Native DPI is already current during a monitor transition, even if
        // GPUI has not consumed its queued WM_DPICHANGED update yet.
        scale: f64::from(unsafe { GetDpiForWindow(hwnd) }) / 96.0,
    })
}

#[cfg(windows)]
fn raw_set_geometry(
    hwnd: windows::Win32::Foundation::HWND,
    position: Option<(i32, i32)>,
    client_size: Option<(f64, f64)>,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::RECT,
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    };
    unsafe {
        let mut flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER;
        let (left, top) = position.unwrap_or_else(|| {
            flags |= SWP_NOMOVE;
            (0, 0)
        });
        let (width, height) = if let Some((width, height)) = client_size {
            if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                return Err(tr("原生窗口客户区尺寸无效。", "Invalid native client size.").into());
            }
            let scale = f64::from(GetDpiForWindow(hwnd)) / 96.0;
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            let mut outer = RECT::default();
            let mut client = RECT::default();
            GetWindowRect(hwnd, &mut outer).map_err(|error| error.to_string())?;
            GetClientRect(hwnd, &mut client).map_err(|error| error.to_string())?;
            // GPUI owns WM_NCCALCSIZE for its custom frame. Its actual client
            // inset differs from AdjustWindowRectExForDpi's standard-frame math.
            (
                (width * scale).round() as i32 + outer.right - outer.left - client.right
                    + client.left,
                (height * scale).round() as i32 + outer.bottom - outer.top - client.bottom
                    + client.top,
            )
        } else {
            flags |= SWP_NOSIZE;
            (0, 0)
        };
        SetWindowPos(hwnd, None, left, top, width, height, flags)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(windows)]
fn raw_set_resizable(
    hwnd: windows::Win32::Foundation::HWND,
    resizable: bool,
) -> Result<(), String> {
    if let Some(result) = clipboard_move::set_resizable(hwnd, resizable) {
        return result;
    }
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let old = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let style = if resizable {
            old | WS_SIZEBOX.0 as isize
        } else {
            old & !(WS_SIZEBOX.0 as isize)
        };
        if old != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn raw_set_clipboard_geometry(
    hwnd: windows::Win32::Foundation::HWND,
    position: Option<(i32, i32)>,
    client_size: (f64, f64),
    collapsed: bool,
) -> Result<(), String> {
    if !client_size.0.is_finite()
        || !client_size.1.is_finite()
        || client_size.0 <= 0.0
        || client_size.1 <= 0.0
    {
        return Err(tr("原生窗口客户区尺寸无效。", "Invalid native client size.").into());
    }
    // Update before SWP_FRAMECHANGED/SetWindowPos can query minimum geometry.
    // Expanding removes this exception and restores GPUI's creation limits.
    FOLDED_CLIENT_HEIGHT.with(|heights| {
        let mut heights = heights.borrow_mut();
        if collapsed {
            heights.insert(hwnd.0 as isize, client_size.1);
        } else {
            heights.remove(&(hwnd.0 as isize));
        }
    });
    raw_set_resizable(hwnd, !collapsed)?;
    raw_set_geometry(hwnd, position, Some(client_size))
}

#[cfg(windows)]
fn raw_show(
    hwnd: windows::Win32::Foundation::HWND,
    explicit: bool,
    activate: bool,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let mut flags =
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
        // Explicit Show reorders even a window already in the topmost band;
        // maintenance never steals its user's current foreground/z-order.
        if !explicit && GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 {
            flags |= SWP_NOZORDER;
        }
        SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags)
            .map_err(|error| error.to_string())?;
    }
    if explicit && activate {
        raw_activate(hwnd)?;
    }
    Ok(())
}

/// Explicit native input focus without GPUI 0.2.2's synthetic Alt-key workaround.
/// Keep this on the owning thread and call from a deferred UI callback.
#[cfg(windows)]
fn raw_activate(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::{
        System::Threading::GetCurrentThreadId,
        UI::{
            Input::KeyboardAndMouse::{SetActiveWindow, SetFocus},
            WindowsAndMessaging::*,
        },
    };
    unsafe {
        if GetForegroundWindow() != hwnd && !SetForegroundWindow(hwnd).as_bool() {
            return Err(tr(
                "Windows 未将前台焦点交给 ClipHost。",
                "Windows did not grant foreground focus to ClipHost.",
            )
            .into());
        }
        let _ = SetActiveWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(GetCurrentThreadId(), &mut info).map_err(|error| error.to_string())?;
        if GetForegroundWindow() != hwnd
            || info.hwndActive != hwnd
            || !(info.hwndFocus == hwnd || IsChild(hwnd, info.hwndFocus).as_bool())
        {
            return Err(tr(
                "未确认 ClipHost 的原生输入焦点。",
                "ClipHost native input focus was not confirmed.",
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(windows)]
const MAIN_FOREGROUND_SUBCLASS: usize = 0xBC02;

#[cfg(windows)]
fn raw_main_contains_window(
    main: windows::Win32::Foundation::HWND,
    candidate: windows::Win32::Foundation::HWND,
) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOTOWNER};
    !candidate.is_invalid()
        && (candidate == main || unsafe { GetAncestor(candidate, GA_ROOTOWNER) } == main)
}

/// Main windows remain ordinary windows when the user switches away. Raising
/// them temporarily is necessary while a pinned, no-focus clipboard is above
/// the normal Z-order band; an owner relationship would couple their lifetime.
#[cfg(windows)]
fn raw_restore_main_layer(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::UI::{Shell::*, WindowsAndMessaging::*};
    unsafe {
        let mut policy = 0;
        if !GetWindowSubclass(
            hwnd,
            Some(main_foreground_proc),
            MAIN_FOREGROUND_SUBCLASS,
            Some(&mut policy),
        )
        .as_bool()
        {
            return Ok(());
        }
        SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            // Include owned pickers in the restoration. NOOWNERZORDER leaves
            // their inherited TOPMOST bit behind on Windows.
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .map_err(|error| error.to_string())?;
        let _ = KillTimer(Some(hwnd), MAIN_FOREGROUND_SUBCLASS);
        let _ = RemoveWindowSubclass(hwnd, Some(main_foreground_proc), MAIN_FOREGROUND_SUBCLASS);
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn main_foreground_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
    id: usize,
    _: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::{Shell::*, WindowsAndMessaging::*};
    let leaving = if message == WM_ACTIVATE && wp.0 as u16 == WA_INACTIVE as u16 {
        let next_active = if lp.0 == 0 {
            GetForegroundWindow()
        } else {
            windows::Win32::Foundation::HWND(lp.0 as *mut _)
        };
        !raw_main_contains_window(hwnd, next_active)
    } else {
        false
    };
    // The owner gets no second WM_ACTIVATE when its native picker loses focus.
    // Watch only while temporarily raised, including its owned dialog family.
    let family_inactive = message == WM_TIMER
        && wp.0 == MAIN_FOREGROUND_SUBCLASS
        && !raw_main_contains_window(hwnd, GetForegroundWindow());
    if leaving || family_inactive || (message == WM_SHOWWINDOW && wp.0 == 0) {
        if let Err(error) = raw_restore_main_layer(hwnd) {
            log::warn!("Cannot restore main window layer: {error}");
        }
    }
    if message == WM_TIMER && wp.0 == MAIN_FOREGROUND_SUBCLASS {
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        let _ = KillTimer(Some(hwnd), MAIN_FOREGROUND_SUBCLASS);
        let _ = RemoveWindowSubclass(hwnd, Some(main_foreground_proc), id);
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

#[cfg(windows)]
fn raw_prepare_main_show(
    hwnd: windows::Win32::Foundation::HWND,
    pinned: bool,
) -> Result<(), String> {
    use windows::Win32::UI::{Shell::*, WindowsAndMessaging::*};
    raw_set_no_activate(hwnd, false)?;
    unsafe {
        if pinned {
            // An explicit persistent pin supersedes a previous temporary raise.
            let _ = KillTimer(Some(hwnd), MAIN_FOREGROUND_SUBCLASS);
            let _ =
                RemoveWindowSubclass(hwnd, Some(main_foreground_proc), MAIN_FOREGROUND_SUBCLASS);
        } else if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize == 0
            && !SetWindowSubclass(
                hwnd,
                Some(main_foreground_proc),
                MAIN_FOREGROUND_SUBCLASS,
                0,
            )
            .as_bool()
        {
            return Err(tr(
                "无法注册设置窗口的前台显示策略。",
                "Cannot register main window foreground presentation.",
            )
            .into());
        }
        let mut policy = 0;
        if GetWindowSubclass(
            hwnd,
            Some(main_foreground_proc),
            MAIN_FOREGROUND_SUBCLASS,
            Some(&mut policy),
        )
        .as_bool()
            && SetTimer(Some(hwnd), MAIN_FOREGROUND_SUBCLASS, 50, None) == 0
        {
            return Err(tr(
                "无法跟踪设置窗口的前台状态。",
                "Cannot track main window foreground state.",
            )
            .into());
        }
        // A repeated show retains the existing temporary-raise subclass. The
        // current TOPMOST bit alone cannot distinguish it from a persistent pin.
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(windows)]
fn raw_present_main(hwnd: windows::Win32::Foundation::HWND, pinned: bool) -> Result<(), String> {
    let result = raw_prepare_main_show(hwnd, pinned).and_then(|_| raw_activate(hwnd));
    if result.is_err() {
        // A refused foreground request must not leave an ordinary settings
        // window permanently above other applications.
        if let Err(error) = raw_restore_main_layer(hwnd) {
            log::warn!("Cannot undo main window foreground presentation: {error}");
        }
    }
    result
}

#[cfg(not(windows))]
pub fn activate(_: &gpui::Window) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn raw_hide(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{GetLastError, SetLastError, HWND, WIN32_ERROR},
        UI::WindowsAndMessaging::*,
    };
    unsafe {
        // Preserve the existing activation/subclass handoff before changing
        // ownership. Hide alone retains WS_EX_TOPMOST on retained HWNDs.
        let _ = ShowWindow(hwnd, SW_HIDE);
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 {
            // Demotion propagates both up and down the owner chain. Temporarily
            // detach the upper owner to preserve its layer and visible siblings,
            // while clearing this entire hidden family (including nested menus).
            // NOOWNERZORDER alone would leave nested windows topmost.
            let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
            let set_owner = |owner: HWND| -> Result<(), String> {
                SetLastError(WIN32_ERROR(0));
                if SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner.0 as isize) == 0
                    && GetLastError().0 != 0
                {
                    return Err(format!("Cannot update hidden window owner: {:?}", GetLastError()));
                }
                Ok(())
            };
            if !owner.is_invalid() {
                set_owner(HWND::default())?;
            }
            let demoted = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ).map_err(|error| error.to_string());
            // Restore ownership even if demotion fails. Later explicit show
            // continues to use the original owner/lifetime relationship.
            let restored = if owner.is_invalid() { Ok(()) } else { set_owner(owner) };
            demoted.and(restored)?;
        }
    }
    Ok(())
}

/// What one bounded-overlay show must do natively. Pure so the no-flicker
/// rules stay testable without showing a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlayShowPlan {
    /// Owner, tool-window and frameless styles (each forces a frame change).
    pub configure: bool,
    pub move_or_size: bool,
    /// Synchronous draw+present passes before DWM composes the new geometry.
    pub redraw_passes: u8,
    pub show: bool,
}

pub(crate) fn overlay_show_plan(
    visible: bool,
    current: (i32, i32, i32, i32),
    target: (i32, i32, i32, i32),
    settle_passes: u8,
) -> OverlayShowPlan {
    let resized = (current.2, current.3) != (target.2, target.3);
    let moved = (current.0, current.1) != (target.0, target.1);
    OverlayShowPlan {
        configure: !visible,
        move_or_size: !visible || resized || moved,
        // Hidden: the swap chain still holds the previous session's last
        // frame (e.g. the preview on the other side). Showing it at the new
        // rect before GPUI's next vsync is the reveal flash. Extra passes let
        // painted-geometry feedback (preview offset) settle while hidden.
        redraw_passes: if !visible || resized {
            settle_passes.max(1)
        } else {
            0
        },
        show: !visible,
    }
}

#[cfg(windows)]
fn raw_overlay_show(
    hwnd: windows::Win32::Foundation::HWND,
    owner: isize,
    rect: (i32, i32, i32, i32),
    settle_passes: u8,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::*,
    };
    let owner = HWND(owner as *mut _);
    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(owner, Some(&mut pid)) } != unsafe { GetCurrentThreadId() }
        || pid != unsafe { GetCurrentProcessId() }
        || rect.2 <= 0
        || rect.3 <= 0
    {
        return Err(tr(
            "浮层所属窗口或尺寸位置无效。",
            "Overlay owner or geometry is invalid.",
        )
        .into());
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let mut current = RECT::default();
    let current = if unsafe { GetWindowRect(hwnd, &mut current) }.is_ok() {
        (
            current.left,
            current.top,
            current.right - current.left,
            current.bottom - current.top,
        )
    } else {
        (0, 0, 0, 0)
    };
    let plan = overlay_show_plan(visible, current, rect, settle_passes);
    if plan.configure {
        raw_configure_host(hwnd)?;
        raw_set_no_activate(hwnd, true)?;
        unsafe {
            raw_frameless_overlay(hwnd)?;
        }
    }
    let mut flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    if !plan.move_or_size {
        flags |= SWP_NOMOVE | SWP_NOSIZE;
    }
    if plan.configure {
        flags |= SWP_FRAMECHANGED;
    }
    unsafe {
        if GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) != owner.0 as isize {
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner.0 as isize);
        }
        // Visible calls still re-assert topmost; they only skip the restyle.
        SetWindowPos(hwnd, Some(HWND_TOPMOST), rect.0, rect.1, rect.2, rect.3, flags)
            .map_err(|error| error.to_string())?;
        for _ in 0..plan.redraw_passes {
            // Each pass flushes GPUI effects, so geometry deferred by the
            // previous paint is applied before the next one presents.
            SendMessageW(hwnd, GPUI_FORCE_UPDATE_WINDOW, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        if plan.show {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Hide, then replace the retained frame while hidden so no later show path
/// can compose the previous session's layout. The view must already render
/// its hidden state (usually empty) when this is posted.
#[cfg(windows)]
fn raw_hide_cleared(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::SendMessageW,
    };
    raw_hide(hwnd)?;
    unsafe {
        SendMessageW(hwnd, GPUI_FORCE_UPDATE_WINDOW, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    Ok(())
}

/// What one copy-toast placement must do natively. Pure so the no-flicker
/// rules stay testable without showing a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToastPlacePlan {
    /// Owner, tool-window and frameless styles (each forces a frame change).
    pub configure: bool,
    pub move_or_size: bool,
    /// Draw and present synchronously at the new size before DWM composes.
    pub redraw: bool,
    pub show: bool,
}

pub(crate) fn toast_place_plan(
    visible: bool,
    current: (i32, i32, i32, i32),
    target: (i32, i32, i32, i32),
    reveal: bool,
) -> ToastPlacePlan {
    let resized = (current.2, current.3) != (target.2, target.3);
    let moved = (current.0, current.1) != (target.0, target.1);
    ToastPlacePlan {
        // A visible toast is already configured; repeating the style writes
        // and SWP_FRAMECHANGED on every copy only forced extra repaints.
        configure: !visible,
        move_or_size: !visible || resized || moved,
        // Hidden: replace the previous session's last frame before showing.
        // Visible: GPUI otherwise waits for the next vsync after
        // ResizeBuffers, so DWM shows the old frame at the new geometry and
        // a bottom-pinned stack jumps by the height change for a frame.
        redraw: !visible || resized,
        show: reveal && !visible,
    }
}

/// Vendored gpui 0.2.2 `WM_GPUI_FORCE_UPDATE_WINDOW` (events.rs): draws and
/// presents synchronously, even while the HWND is still hidden.
#[cfg(windows)]
const GPUI_FORCE_UPDATE_WINDOW: u32 = windows::Win32::UI::WindowsAndMessaging::WM_USER + 5;

/// Copy-toast variant of `raw_overlay_show`. Hidden windows are sized and
/// drawn first; `reveal` shows them only once the caller knows the size fits.
#[cfg(windows)]
fn raw_toast_place(
    hwnd: windows::Win32::Foundation::HWND,
    owner: isize,
    rect: (i32, i32, i32, i32),
    reveal: bool,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::*,
    };
    let owner = HWND(owner as *mut _);
    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(owner, Some(&mut pid)) } != unsafe { GetCurrentThreadId() }
        || pid != unsafe { GetCurrentProcessId() }
        || rect.2 <= 0
        || rect.3 <= 0
    {
        return Err(tr(
            "浮层所属窗口或尺寸位置无效。",
            "Overlay owner or geometry is invalid.",
        )
        .into());
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let mut current = RECT::default();
    let current = if unsafe { GetWindowRect(hwnd, &mut current) }.is_ok() {
        (
            current.left,
            current.top,
            current.right - current.left,
            current.bottom - current.top,
        )
    } else {
        (0, 0, 0, 0)
    };
    let plan = toast_place_plan(visible, current, rect, reveal);
    if plan.configure {
        raw_configure_host(hwnd)?;
        raw_set_no_activate(hwnd, true)?;
        unsafe {
            raw_frameless_overlay(hwnd)?;
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner.0 as isize);
        }
    }
    let mut flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    if !plan.move_or_size {
        flags |= SWP_NOMOVE | SWP_NOSIZE;
    }
    if plan.configure {
        flags |= SWP_FRAMECHANGED;
    }
    unsafe {
        SetWindowPos(hwnd, Some(HWND_TOPMOST), rect.0, rect.1, rect.2, rect.3, flags)
            .map_err(|error| error.to_string())?;
        if plan.redraw {
            SendMessageW(hwnd, GPUI_FORCE_UPDATE_WINDOW, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        if plan.show {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod toast_place_tests {
    use super::*;

    const RECT: (i32, i32, i32, i32) = (1522, 862, 384, 204);

    #[test]
    fn toast_hidden_window_is_configured_sized_and_drawn_before_reveal() {
        let staged = toast_place_plan(false, (0, 0, 1024, 700), RECT, false);
        assert_eq!(
            staged,
            ToastPlacePlan { configure: true, move_or_size: true, redraw: true, show: false }
        );
        // Even an unchanged rect redraws: the buffer still holds the last
        // frame of the previous (hidden) stack.
        assert!(toast_place_plan(false, RECT, RECT, true).redraw);
        assert!(toast_place_plan(false, RECT, RECT, true).show);
    }

    #[test]
    fn toast_visible_window_skips_restyling_and_redraws_only_on_resize() {
        let same = toast_place_plan(true, RECT, RECT, true);
        assert_eq!(
            same,
            ToastPlacePlan { configure: false, move_or_size: false, redraw: false, show: false }
        );
        let moved = toast_place_plan(true, (1500, 862, 384, 204), RECT, true);
        assert!(moved.move_or_size && !moved.redraw && !moved.configure);
        let grown = toast_place_plan(true, (1522, 950, 384, 116), RECT, true);
        assert!(grown.move_or_size && grown.redraw && !grown.show);
    }

    const QP: (i32, i32, i32, i32) = (600, 300, 1012, 560);

    #[test]
    fn overlay_hidden_window_settles_new_layout_before_reveal() {
        // The reused HWND still holds the previous session's last frame,
        // so even the same rect is redrawn before it becomes visible.
        for current in [(0, 0, 1024, 700), QP] {
            assert_eq!(
                overlay_show_plan(false, current, QP, 2),
                OverlayShowPlan { configure: true, move_or_size: true, redraw_passes: 2, show: true }
            );
        }
        assert_eq!(overlay_show_plan(false, QP, QP, 0).redraw_passes, 1);
        assert_eq!(overlay_show_plan(false, QP, QP, 1).redraw_passes, 1);
    }

    #[test]
    fn overlay_visible_window_reasserts_without_restyle_or_extra_frames() {
        assert_eq!(
            overlay_show_plan(true, QP, QP, 2),
            OverlayShowPlan { configure: false, move_or_size: false, redraw_passes: 0, show: false }
        );
        let moved = overlay_show_plan(true, (580, 300, 1012, 560), QP, 2);
        assert!(moved.move_or_size && moved.redraw_passes == 0 && !moved.show);
        let narrowed = overlay_show_plan(true, (600, 300, 1180, 560), QP, 2);
        assert!(narrowed.move_or_size && narrowed.redraw_passes == 2 && !narrowed.configure);
    }
}

#[cfg(not(windows))]
pub fn configure_host(_: &gpui::Window) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn geometry(window: &gpui::Window) -> Result<Geometry, String> {
    let bounds = window.bounds();
    Ok(Geometry {
        left: f32::from(bounds.origin.x) as i32,
        top: f32::from(bounds.origin.y) as i32,
        width: f32::from(bounds.size.width) as u32,
        height: f32::from(bounds.size.height) as u32,
        scale: 1.0,
    })
}
#[cfg(not(windows))]
pub fn set_geometry(
    _: &gpui::Window,
    _: Option<(i32, i32)>,
    _: Option<(f64, f64)>,
) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn set_resizable(_: &gpui::Window, _: bool) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn set_clipboard_geometry(
    _: &gpui::Window,
    _: Option<(i32, i32)>,
    _: (f64, f64),
    _: bool,
) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn show(window: &mut gpui::Window, _: bool, activate: bool) -> Result<(), String> {
    if activate {
        window.activate_window();
    }
    Ok(())
}
#[cfg(not(windows))]
pub fn hide(_: &gpui::Window) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn hide_cleared(_: &gpui::Window) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn overlay_show(_: &gpui::Window, _: isize, _: (i32, i32, i32, i32)) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn drag_preview_show(_: &gpui::Window, _: (i32, i32, i32, i32)) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn drag_preview_alpha(_: &gpui::Window, _: u8) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn overlay_show_settled(
    _: &gpui::Window,
    _: isize,
    _: (i32, i32, i32, i32),
    _: u8,
) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn toast_place(
    _: &gpui::Window,
    _: isize,
    _: (i32, i32, i32, i32),
    _: bool,
) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
const NATIVE_DISPATCH: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x337;

#[cfg(windows)]
type NativeCallback = Box<dyn FnOnce(windows::Win32::Foundation::HWND) -> Result<(), String>>;
#[cfg(windows)]
type NativeCondition = std::rc::Rc<dyn Fn() -> bool>;
#[cfg(windows)]
struct NativeJob {
    callback: NativeCallback,
    condition: Option<NativeCondition>,
}
#[cfg(windows)]
#[derive(Default)]
struct NativeQueue {
    jobs: std::collections::VecDeque<NativeJob>,
    errors: std::collections::VecDeque<String>,
    posted: bool,
    running: bool,
}
#[cfg(windows)]
thread_local! {
    static NATIVE_QUEUE: std::cell::RefCell<std::collections::HashMap<isize, NativeQueue>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static NATIVE_CONDITION: std::cell::RefCell<Option<NativeCondition>> = const { std::cell::RefCell::new(None) };
}

/// Posted operations are checked at their actual Win32 dispatch, not just when
/// the GPUI view enqueues them. Scope guards retain the original command epoch.
#[cfg(windows)]
pub struct OperationGuard {
    previous: Option<NativeCondition>,
}
#[cfg(windows)]
impl Drop for OperationGuard {
    fn drop(&mut self) {
        NATIVE_CONDITION.with(|condition| *condition.borrow_mut() = self.previous.take());
    }
}
#[cfg(windows)]
pub fn guard_condition(check: impl Fn() -> bool + 'static) -> OperationGuard {
    OperationGuard {
        previous: NATIVE_CONDITION
            .with(|condition| condition.borrow_mut().replace(std::rc::Rc::new(check))),
    }
}

/// Refine an existing queued operation's scope without dropping its owner guard.
#[cfg(windows)]
pub fn guard_condition_inherited(check: impl Fn() -> bool + 'static) -> OperationGuard {
    let parent = NATIVE_CONDITION.with(|condition| condition.borrow().clone());
    guard_condition(move || parent.as_ref().is_none_or(|parent| parent()) && check())
}
#[cfg(windows)]
pub fn guard_task(host: &HostState, task: ChromeTask) -> OperationGuard {
    let host = host.clone();
    let explicit = host.chrome_task_explicit_presentation(task);
    let checked = std::cell::Cell::new(None);
    guard_condition(move || {
        let allowed = checked.get().unwrap_or_else(|| {
            let allowed = host.prepare_chrome_task(task);
            checked.set(Some(allowed));
            allowed
        });
        allowed
            && host.chrome_command_lifecycle_allowed(task.command())
            && host.chrome_task_input_allowed(task)
            && (!explicit || host.chrome_task_explicit_presentation(task))
    })
}

#[cfg(windows)]
fn post_native(window: &gpui::Window, callback: NativeCallback) -> Result<(), String> {
    post_native_hwnd(hwnd(window)?, callback)
}

#[cfg(windows)]
fn post_native_hwnd(
    hwnd: windows::Win32::Foundation::HWND,
    callback: NativeCallback,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{
            Shell::{GetWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::PostMessageW,
        },
    };
    let mut policy = 2;
    unsafe {
        // Registering the message handler does not run GPUI resize/focus code.
        if !GetWindowSubclass(
            hwnd,
            Some(host_style_proc),
            HOST_STYLE_SUBCLASS,
            Some(&mut policy),
        )
        .as_bool()
            && !SetWindowSubclass(hwnd, Some(host_style_proc), HOST_STYLE_SUBCLASS, 2).as_bool()
        {
            return Err(tr(
                "无法注册原生窗口调度器。",
                "Cannot register native window dispatcher.",
            )
            .into());
        }
    }
    let should_post = enqueue_native(hwnd, callback);
    if should_post {
        if let Err(error) =
            unsafe { PostMessageW(Some(hwnd), NATIVE_DISPATCH, WPARAM(0), LPARAM(0)) }
        {
            NATIVE_QUEUE.with(|queues| {
                queues.borrow_mut().remove(&(hwnd.0 as isize));
            });
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// A view can be dropped while GPUI holds an App borrow. Retained popup cleanup
/// must run later on its owning HWND, even after the presentation guard expires.
#[cfg(windows)]
pub(crate) fn hide_retained_overlay(hwnd: isize) -> Result<(), String> {
    use windows::Win32::{
        Foundation::HWND,
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    };
    let hwnd = HWND(hwnd as *mut _);
    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) }
        != unsafe { GetCurrentThreadId() }
        || pid != unsafe { GetCurrentProcessId() }
    {
        return Err("Retained popup is not owned by this UI thread.".into());
    }
    let _cleanup = guard_condition(|| true);
    post_native_hwnd(hwnd, Box::new(raw_hide))
}

#[cfg(windows)]
fn enqueue_native(hwnd: windows::Win32::Foundation::HWND, callback: NativeCallback) -> bool {
    let condition = NATIVE_CONDITION.with(|condition| condition.borrow().clone());
    NATIVE_QUEUE.with(|queues| {
        let mut queues = queues.borrow_mut();
        let queue = queues.entry(hwnd.0 as isize).or_default();
        queue.jobs.push_back(NativeJob {
            callback,
            condition,
        });
        !std::mem::replace(&mut queue.posted, true)
    })
}

#[cfg(windows)]
unsafe fn dispatch_native(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::{
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    };
    let jobs = NATIVE_QUEUE.with(|queues| {
        let mut queues = queues.borrow_mut();
        let queue = queues.entry(hwnd.0 as isize).or_default();
        queue.posted = false;
        queue.running = true;
        std::mem::take(&mut queue.jobs)
    });
    // No queue or GPUI App borrow is held while a SetWindowPos/Show operation
    // synchronously delivers resize, activation and position messages.
    for job in jobs {
        if job.condition.as_ref().is_some_and(|check| !check()) {
            continue;
        }
        let mut pid = 0;
        if GetWindowThreadProcessId(hwnd, Some(&mut pid)) != GetCurrentThreadId()
            || pid != GetCurrentProcessId()
        {
            continue;
        }
        if let Err(error) = (job.callback)(hwnd) {
            eprintln!("ClipHost native window: {error}");
            NATIVE_QUEUE.with(|queues| {
                queues
                    .borrow_mut()
                    .entry(hwnd.0 as isize)
                    .or_default()
                    .errors
                    .push_back(error)
            });
        }
    }
    NATIVE_QUEUE.with(|queues| {
        if let Some(queue) = queues.borrow_mut().get_mut(&(hwnd.0 as isize)) {
            queue.running = false;
        }
    });
}

#[cfg(windows)]
pub fn has_pending(window: &gpui::Window) -> Result<bool, String> {
    let hwnd = hwnd(window)?;
    Ok(NATIVE_QUEUE.with(|queues| {
        queues
            .borrow()
            .get(&(hwnd.0 as isize))
            .is_some_and(|queue| queue.running || !queue.jobs.is_empty())
    }))
}
#[cfg(windows)]
pub fn take_error(window: &gpui::Window) -> Result<Option<String>, String> {
    let hwnd = hwnd(window)?;
    Ok(NATIVE_QUEUE.with(|queues| {
        queues
            .borrow_mut()
            .get_mut(&(hwnd.0 as isize))
            .and_then(|queue| queue.errors.pop_front())
    }))
}

/// Ordinary (activatable) windows still receive the core's `WindowFromPoint`
/// probes; register the dispatcher-only subclass so they answer without
/// re-entering GPUI. Window styles are left untouched.
#[cfg(windows)]
pub fn register_probe_window(window: &gpui::Window) -> Result<(), String> {
    use windows::Win32::UI::Shell::{GetWindowSubclass, SetWindowSubclass};
    let hwnd = hwnd(window)?;
    let mut policy = 2;
    unsafe {
        if !GetWindowSubclass(
            hwnd,
            Some(host_style_proc),
            HOST_STYLE_SUBCLASS,
            Some(&mut policy),
        )
        .as_bool()
            && !SetWindowSubclass(hwnd, Some(host_style_proc), HOST_STYLE_SUBCLASS, 2).as_bool()
        {
            return Err(tr(
                "无法注册原生窗口调度器。",
                "Cannot register native window dispatcher.",
            )
            .into());
        }
    }
    Ok(())
}

/// Attach the independent editor after the current GPUI borrow. Subsequent
/// native owner movement/visibility changes are followed through posted jobs.
#[cfg(windows)]
pub fn dock_editor(window: &gpui::Window, owner: isize) -> Result<(), String> {
    // Host-free editor fixtures have no clipboard root to accompany.
    if owner == 0 {
        return Ok(());
    }
    post_native(window, Box::new(move |hwnd| editor_docking::register(hwnd, owner)))
}

#[cfg(not(windows))]
pub fn dock_editor(_: &gpui::Window, _: isize) -> Result<(), String> {
    Ok(())
}

/// The LAN pane shares the editor's root-only owner and deferred placement.
#[cfg(windows)]
pub fn dock_companion(window: &gpui::Window, owner: isize, minimum_width: f64) -> Result<(), String> {
    if owner == 0 { return Ok(()); }
    post_native(window, Box::new(move |hwnd| editor_docking::register_with_minimum(hwnd, owner, minimum_width)))
}

#[cfg(windows)]
pub fn undock_companion(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(editor_docking::unregister))
}

#[cfg(windows)]
pub fn set_companion_visible(window: &gpui::Window, visible: bool) -> Result<(), String> {
    post_native(window, Box::new(move |hwnd| editor_docking::set_enabled(hwnd, visible)))
}

#[cfg(not(windows))]
pub fn dock_companion(_: &gpui::Window, _: isize, _: f64) -> Result<(), String> { Ok(()) }
#[cfg(not(windows))]
pub fn undock_companion(_: &gpui::Window) -> Result<(), String> { Ok(()) }
#[cfg(not(windows))]
pub fn set_companion_visible(_: &gpui::Window, _: bool) -> Result<(), String> { Ok(()) }

#[cfg(windows)]
pub fn begin_companion_move(window: &gpui::Window, host: &HostState) -> Result<(), String> {
    let host = host.clone();
    post_native(window, Box::new(move |hwnd| {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{GetWindow, GW_OWNER}};
        let owner = HWND(host.host_hwnd() as *mut _);
        if !owner.0.is_null() && unsafe { GetWindow(hwnd, GW_OWNER) }.ok() == Some(owner)
            && cliphost_core::window_chrome::try_begin_caption_drag(owner.0 as isize)? {
            host.arm_caption_drag_interaction(false);
        }
        Ok(())
    }))
}

#[cfg(not(windows))]
pub fn begin_companion_move(_: &gpui::Window, _: &HostState) -> Result<(), String> { Ok(()) }

#[cfg(windows)]
pub fn configure_host(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(raw_configure_host))
}
/// Only NativeWindow (the clipboard root) opts into native-move Snap suppression.
#[cfg(windows)]
pub fn configure_clipboard_movement(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(|hwnd| {
        clipboard_move::register(hwnd)?;
        clipboard_resize::register(hwnd)
    }))
}
#[cfg(not(windows))]
pub fn configure_clipboard_movement(_: &gpui::Window) -> Result<(), String> { Ok(()) }
#[cfg(windows)]
pub fn set_no_activate(window: &gpui::Window, enabled: bool) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_set_no_activate(hwnd, enabled)),
    )
}
#[cfg(windows)]
pub fn show_no_activate(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(raw_show_no_activate))
}
#[cfg(windows)]
pub fn set_geometry(
    window: &gpui::Window,
    position: Option<(i32, i32)>,
    size: Option<(f64, f64)>,
) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_set_geometry(hwnd, position, size)),
    )
}

/// First placement for CeaBox only. Resolve the invocation monitor from the
/// physical pointer, then move the hidden HWND there before correcting its
/// logical client size against the destination monitor's actual window DPI.
/// Callers own the one-time initialization flag; this never shows/activates.
#[cfg(windows)]
pub(crate) fn initialize_ceabox_geometry(
    window: &gpui::Window,
    client_size: (f64, f64),
) -> Result<(), String> {
    use cliphost_core::window_place;
    let pointer = window_place::cursor_pos()
        .ok_or_else(|| tr("无法读取 CeaBox 触发位置。", "Cannot read CeaBox invocation position.").to_string())?;
    let area = window_place::overlay_work_area_from_point(pointer.0, pointer.1)
        .ok_or_else(|| tr("无法读取 CeaBox 所在屏幕工作区。", "Cannot read the CeaBox monitor work area.").to_string())?;
    let initial = ceabox_initial_rect(pointer, area, client_size)
        .ok_or_else(|| tr("CeaBox 初始窗口尺寸无效。", "Invalid CeaBox initial window size.").to_string())?;
    post_native(window, Box::new(move |hwnd| {
        use windows::Win32::{Foundation::RECT, UI::WindowsAndMessaging::*};
        let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER;
        unsafe {
            // Move and size together so a large GPUI creation rectangle cannot
            // keep the HWND assigned to its old monitor during a DPI change.
            SetWindowPos(hwnd, None, initial.0, initial.1, initial.2, initial.3, flags)
                .map_err(|error| error.to_string())?;
        }
        // WM_DPICHANGED has now run on the owning thread, outside GPUI borrows.
        // Use the final native DPI and actual custom-frame inset for exact DIP
        // client dimensions; do not scale against the window's previous screen.
        raw_set_geometry(hwnd, None, Some(client_size))?;
        let mut outer = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut outer) }.map_err(|error| error.to_string())?;
        let position = window_place::compute(
            window_place::ShowWindowLocation::CenterScreen,
            pointer,
            area.work,
            outer.right - outer.left,
            outer.bottom - outer.top,
        ).ok_or_else(|| "Invalid CeaBox centered window rectangle.".to_string())?;
        unsafe {
            SetWindowPos(hwnd, None, position.0, position.1, 0, 0, flags | SWP_NOSIZE)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }))
}

#[cfg(not(windows))]
pub(crate) fn initialize_ceabox_geometry(
    window: &gpui::Window,
    client_size: (f64, f64),
) -> Result<(), String> {
    // GPUI's creation bounds already center the non-Windows fixture.
    set_geometry(window, None, Some(client_size))
}

#[cfg(any(windows, test))]
fn ceabox_initial_rect(
    pointer: (i32, i32),
    area: cliphost_core::window_place::OverlayWorkArea,
    client_size: (f64, f64),
) -> Option<(i32, i32, i32, i32)> {
    let origin = cliphost_core::window_place::compute_for_logical_size(
        cliphost_core::window_place::ShowWindowLocation::CenterScreen,
        pointer,
        area,
        client_size.0,
        client_size.1,
    )?;
    Some((origin.0, origin.1,
        (client_size.0 * area.dpi_scale_x).round() as i32,
        (client_size.1 * area.dpi_scale_y).round() as i32))
}

#[cfg(test)]
mod ceabox_placement_tests {
    use super::ceabox_initial_rect;
    use cliphost_core::{edge_hide::Rect, window_place::OverlayWorkArea};

    fn area(left: f64, top: f64, width: f64, height: f64, scale: f64) -> OverlayWorkArea {
        OverlayWorkArea { work: Rect { left, top, width, height },
            dpi_scale_x: scale, dpi_scale_y: scale }
    }

    #[test]
    fn default_size_centers_in_physical_work_area_at_125_percent() {
        assert_eq!(ceabox_initial_rect((1700, 900), area(0.0, 0.0, 1920.0, 1040.0, 1.25), (680.0, 510.0)),
            Some((535, 201, 850, 638)));
    }

    #[test]
    fn monitor_left_of_primary_keeps_negative_screen_coordinates() {
        assert_eq!(ceabox_initial_rect((-1600, 700), area(-1920.0, 0.0, 1920.0, 1040.0, 1.0), (680.0, 510.0)),
            Some((-1300, 265, 680, 510)));
    }

    #[test]
    fn upper_monitor_uses_its_destination_scale_and_negative_origin() {
        assert_eq!(ceabox_initial_rect((-1200, -700), area(-2560.0, -1440.0, 2560.0, 1400.0, 2.0), (680.0, 510.0)),
            Some((-1960, -1250, 1360, 1020)));
    }

    #[test]
    fn taskbar_work_area_offset_and_small_screens_follow_existing_clamp() {
        assert_eq!(ceabox_initial_rect((900, 600), area(0.0, 40.0, 1920.0, 1000.0, 1.25), (680.0, 510.0)),
            Some((535, 221, 850, 638)));
        assert_eq!(ceabox_initial_rect((-400, 200), area(-600.0, 50.0, 600.0, 400.0, 1.0), (680.0, 510.0)),
            Some((-600, 50, 680, 510)));
    }
}
#[cfg(windows)]
pub fn set_resizable(window: &gpui::Window, enabled: bool) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_set_resizable(hwnd, enabled)),
    )
}
/// Apply the clipboard's Fold exception, resize style and client geometry as
/// one guarded native job, outside GPUI borrows. Other windows are unaffected.
#[cfg(windows)]
pub fn set_clipboard_geometry(
    window: &gpui::Window,
    position: Option<(i32, i32)>,
    client_size: (f64, f64),
    collapsed: bool,
) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_set_clipboard_geometry(hwnd, position, client_size, collapsed)),
    )
}
#[cfg(windows)]
pub fn show(window: &mut gpui::Window, explicit: bool, activate: bool) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_show(hwnd, explicit, activate)),
    )
}
#[cfg(windows)]
pub fn hide(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(raw_hide))
}
#[cfg(windows)]
pub fn activate(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(raw_activate))
}

/// Explicit settings/management presentation, dispatched outside GPUI borrows.
/// Release fake-focus keyboard routing only after native activation succeeds.
#[cfg(windows)]
pub fn present_main(window: &mut gpui::Window, host: &HostState) -> Result<(), String> {
    let expected = hwnd(window)?.0 as isize;
    let owner = host.clone();
    let _permission =
        guard_condition_inherited(move || owner.native_caption_drag_allowed("main", expected));
    let host = host.clone();
    post_native(
        window,
        Box::new(move |hwnd| {
            let pinned = host.main_host_state_dto()["isPinned"]
                .as_bool()
                .unwrap_or(false);
            raw_present_main(hwnd, pinned)?;
            host.chrome_web_action(&serde_json::json!({
                "kind": "clipboardHostChrome",
                "sourceHostWindow": "main",
                "requestHostKeyboardFocus": true,
            }));
            Ok(())
        }),
    )
}

#[cfg(not(windows))]
pub fn present_main(window: &mut gpui::Window, host: &HostState) -> Result<(), String> {
    window.activate_window();
    host.chrome_web_action(&serde_json::json!({
        "kind": "clipboardHostChrome",
        "sourceHostWindow": "main",
        "requestHostKeyboardFocus": true,
    }));
    Ok(())
}
#[cfg(windows)]
pub fn overlay_show(
    window: &gpui::Window,
    owner: isize,
    rect: (i32, i32, i32, i32),
) -> Result<(), String> {
    overlay_show_settled(window, owner, rect, 1)
}
/// `overlay_show` with extra hidden draw passes for views whose layout
/// depends on their own painted geometry; see `overlay_show_plan`.
#[cfg(windows)]
pub fn overlay_show_settled(
    window: &gpui::Window,
    owner: isize,
    rect: (i32, i32, i32, i32),
    settle_passes: u8,
) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_overlay_show(hwnd, owner, rect, settle_passes)),
    )
}

/// Show the drag preview popup at a physical rect. Layered + transparent makes
/// it click-through: WindowFromPoint, OLE drop targets and our own surface
/// routing all resolve the window beneath it. Never activates.
#[cfg(windows)]
pub fn drag_preview_show(window: &gpui::Window, rect: (i32, i32, i32, i32)) -> Result<(), String> {
    post_native(window, Box::new(move |hwnd| raw_drag_preview_show(hwnd, rect)))
}

/// Whole-popup alpha (layered attribute), for the drop fade-out.
#[cfg(windows)]
pub fn drag_preview_alpha(window: &gpui::Window, alpha: u8) -> Result<(), String> {
    post_native(window, Box::new(move |hwnd| unsafe {
        use windows::Win32::{Foundation::COLORREF, UI::WindowsAndMessaging::*};
        SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA).map_err(|error| error.to_string())
    }))
}

/// Run `job` on this window's thread after the current GPUI dispatch, with
/// no App borrow held (e.g. PrintWindow of this window).
#[cfg(windows)]
pub(crate) fn post_window_job(
    window: &gpui::Window,
    job: impl FnOnce(isize) -> Result<(), String> + 'static,
) -> Result<(), String> {
    post_native(window, Box::new(move |hwnd| job(hwnd.0 as isize)))
}

#[cfg(windows)]
fn raw_drag_preview_show(
    hwnd: windows::Win32::Foundation::HWND,
    rect: (i32, i32, i32, i32),
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{COLORREF, LPARAM, WPARAM},
        UI::WindowsAndMessaging::*,
    };
    if rect.2 <= 0 || rect.3 <= 0 {
        return Err(tr("拖动预览尺寸无效。", "Invalid drag preview size.").into());
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    if !visible {
        raw_configure_host(hwnd)?;
        raw_set_no_activate(hwnd, true)?;
    }
    unsafe {
        let mut flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        if !visible {
            raw_frameless_overlay(hwnd)?;
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let through = (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize;
            if style & through != through {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | through);
            }
            // A layered window stays invisible until it has attributes; full
            // alpha keeps GPUI's own per-pixel transparency.
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)
                .map_err(|error| error.to_string())?;
            flags |= SWP_FRAMECHANGED;
        }
        SetWindowPos(hwnd, Some(HWND_TOPMOST), rect.0, rect.1, rect.2, rect.3, flags)
            .map_err(|error| error.to_string())?;
        if !visible {
            // Present the new label while hidden so the previous drag's last
            // frame is never composed at the new position.
            SendMessageW(hwnd, GPUI_FORCE_UPDATE_WINDOW, Some(WPARAM(0)), Some(LPARAM(0)));
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Restrict this popup to the union of its painted cards. Rectangles are
/// client-relative logical (x, y, width, height); include any desired shadow
/// margin. An empty list makes the popup empty rather than removing clipping.
/// Like other posted operations, this inherits the caller's lease guard.
#[cfg(windows)]
pub fn overlay_set_regions(
    window: &gpui::Window,
    regions: Vec<(f32, f32, f32, f32)>,
) -> Result<(), String> {
    post_native(window, Box::new(move |hwnd| {
        use windows::Win32::{
            Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, HRGN, RGN_OR, SetWindowRgn},
            UI::HiDpi::GetDpiForWindow,
        };
        struct OwnedRegion(HRGN);
        impl Drop for OwnedRegion {
            fn drop(&mut self) { unsafe { let _ = DeleteObject(self.0.into()); } }
        }
        let fail = || tr("无法设置原生弹层可点击区域。", "Cannot set native popup input regions.").to_owned();
        let scale = f64::from(unsafe { GetDpiForWindow(hwnd) }) / 96.0;
        let pixels = overlay_region_pixels(&regions, scale).ok_or_else(fail)?;
        if OVERLAY_REGIONS.with(|current| current.borrow().get(&(hwnd.0 as isize)) == Some(&pixels)) {
            return Ok(());
        }
        let union = OwnedRegion(unsafe { CreateRectRgn(0, 0, 0, 0) });
        if union.0.is_invalid() { return Err(fail()); }
        for &(left, top, right, bottom) in &pixels {
            let part = OwnedRegion(unsafe { CreateRectRgn(left, top, right, bottom) });
            if part.0.is_invalid() || unsafe { CombineRgn(Some(union.0), Some(union.0), Some(part.0), RGN_OR) }.0 == 0 {
                return Err(fail());
            }
        }
        // Do not invalidate GPUI or repaint from its measurement callback.
        // Windows owns the HRGN only after a successful SetWindowRgn.
        if unsafe { SetWindowRgn(hwnd, Some(union.0), false) } == 0 { return Err(fail()); }
        std::mem::forget(union);
        OVERLAY_REGIONS.with(|current| { current.borrow_mut().insert(hwnd.0 as isize, pixels); });
        Ok(())
    }))
}

#[cfg(windows)]
fn overlay_region_pixels(regions: &[(f32, f32, f32, f32)], scale: f64) -> Option<Vec<(i32, i32, i32, i32)>> {
    if !scale.is_finite() || scale <= 0.0 { return None; }
    let mut pixels = Vec::with_capacity(regions.len());
    for &(x, y, width, height) in regions {
        if ![x, y, width, height].into_iter().all(f32::is_finite) || width < 0.0 || height < 0.0 { return None; }
        if width == 0.0 || height == 0.0 { continue; }
        let rect = [
            (f64::from(x) * scale).floor(), (f64::from(y) * scale).floor(),
            ((f64::from(x) + f64::from(width)) * scale).ceil(),
            ((f64::from(y) + f64::from(height)) * scale).ceil(),
        ];
        if !rect.into_iter().all(|edge| edge >= f64::from(i32::MIN) && edge <= f64::from(i32::MAX)) { return None; }
        pixels.push((rect[0] as i32, rect[1] as i32, rect[2] as i32, rect[3] as i32));
    }
    Some(pixels)
}

#[cfg(all(test, windows))]
mod overlay_region_tests {
    use super::overlay_region_pixels;

    #[test]
    fn popup_regions_round_outward_at_current_dpi_and_keep_gaps() {
        assert_eq!(overlay_region_pixels(&[(8.25, 8.25, 260.5, 100.25), (480.0, 38.0, 210.0, 40.0)], 1.25),
            Some(vec![(10, 10, 336, 136), (600, 47, 863, 98)]));
        assert_eq!(overlay_region_pixels(&[(-2.0, -2.0, 20.0, 20.0)], 2.0), Some(vec![(-4, -4, 36, 36)]));
    }

    #[test]
    fn popup_regions_keep_empty_shape_and_reject_invalid_geometry() {
        assert_eq!(overlay_region_pixels(&[], 1.0), Some(vec![]));
        assert_eq!(overlay_region_pixels(&[(0.0, 0.0, 0.0, 1.0)], 1.0), Some(vec![]));
        assert!(overlay_region_pixels(&[(0.0, 0.0, -1.0, 1.0)], 1.0).is_none());
        assert!(overlay_region_pixels(&[(f32::NAN, 0.0, 1.0, 1.0)], 1.0).is_none());
        assert!(overlay_region_pixels(&[(0.0, 0.0, f32::MAX, 1.0)], 1.0).is_none());
        assert!(overlay_region_pixels(&[], 0.0).is_none());
    }
}

#[cfg(not(windows))]
pub fn overlay_set_regions(_: &gpui::Window, _: Vec<(f32, f32, f32, f32)>) -> Result<(), String> {
    Ok(())
}

/// Hide and repaint the hidden HWND; see `raw_hide_cleared`.
#[cfg(windows)]
pub fn hide_cleared(window: &gpui::Window) -> Result<(), String> {
    post_native(window, Box::new(raw_hide_cleared))
}
/// Place the copy toast; see `raw_toast_place`.
#[cfg(windows)]
pub fn toast_place(
    window: &gpui::Window,
    owner: isize,
    rect: (i32, i32, i32, i32),
    reveal: bool,
) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| raw_toast_place(hwnd, owner, rect, reveal)),
    )
}
#[cfg(windows)]
pub fn begin_move(window: &gpui::Window, host: &HostState) -> Result<(), String> {
    let host = host.clone();
    post_native(
        window,
        Box::new(move |hwnd| {
            if host.host_hwnd() != hwnd.0 as isize {
                return Ok(());
            }
            if cliphost_core::window_chrome::try_begin_caption_drag(hwnd.0 as isize)? {
                host.arm_caption_drag_interaction(false);
            }
            Ok(())
        }),
    )
}

/// Dispatch outside GPUI's current App borrow. GPUI 0.2.2's Windows
/// start_window_move hook is a no-op, so native titlebars use Win32 directly.
#[cfg(windows)]
pub fn begin_host_move(
    window: &gpui::Window,
    host: &HostState,
    label: &'static str,
) -> Result<(), String> {
    if label == "clipboard" {
        return begin_move(window, host);
    }
    let host = host.clone();
    post_native(
        window,
        Box::new(move |hwnd| {
            if host.native_caption_drag_allowed(label, hwnd.0 as isize) {
                cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd.0 as isize);
                cliphost_core::window_chrome::try_begin_caption_drag(hwnd.0 as isize)?;
            }
            Ok(())
        }),
    )
}

#[cfg(windows)]
pub fn begin_owned_move(window: &gpui::Window) -> Result<(), String> {
    post_native(
        window,
        Box::new(move |hwnd| {
            cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd.0 as isize);
            cliphost_core::window_chrome::try_begin_caption_drag(hwnd.0 as isize)?;
            Ok(())
        }),
    )
}

#[cfg(not(windows))]
pub fn begin_host_move(
    window: &gpui::Window,
    _: &HostState,
    _: &'static str,
) -> Result<(), String> {
    window.start_window_move();
    Ok(())
}

#[cfg(not(windows))]
pub fn begin_owned_move(window: &gpui::Window) -> Result<(), String> {
    window.start_window_move();
    Ok(())
}

/// Delayed caption movement keeps the original client anchor, even if the
/// pointer crossed the window edge before the drag threshold was reached.
pub fn begin_host_move_at_client(
    window: &gpui::Window, host: &HostState, label: &'static str,
    origin: gpui::Point<gpui::Pixels>,
) -> Result<(), String> {
    #[cfg(windows)] {
        let host = host.clone();
        let (x, y, scale) = (f64::from(f32::from(origin.x)), f64::from(f32::from(origin.y)), f64::from(window.scale_factor()));
        post_native(window, Box::new(move |hwnd| {
            if host.native_caption_drag_allowed(label, hwnd.0 as isize) {
                cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd.0 as isize);
                cliphost_core::window_chrome::try_begin_caption_drag_at_client(hwnd.0 as isize, x, y, scale)?;
            }
            Ok(())
        }))
    }
    #[cfg(not(windows))] { let _ = (host, label, origin); begin_owned_move(window) }
}

pub fn begin_owned_move_at_client(
    window: &gpui::Window, origin: gpui::Point<gpui::Pixels>,
) -> Result<(), String> {
    #[cfg(windows)] {
        let (x, y, scale) = (f64::from(f32::from(origin.x)), f64::from(f32::from(origin.y)), f64::from(window.scale_factor()));
        post_native(window, Box::new(move |hwnd| {
            cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd.0 as isize);
            cliphost_core::window_chrome::try_begin_caption_drag_at_client(hwnd.0 as isize, x, y, scale)?;
            Ok(())
        }))
    }
    #[cfg(not(windows))] { let _ = origin; begin_owned_move(window) }
}

#[cfg(not(windows))]
pub struct OperationGuard;
#[cfg(not(windows))]
pub fn guard_task(_: &HostState, _: ChromeTask) -> OperationGuard {
    OperationGuard
}
#[cfg(not(windows))]
pub fn guard_condition(_: impl Fn() -> bool + 'static) -> OperationGuard {
    OperationGuard
}
#[cfg(not(windows))]
pub fn guard_condition_inherited(_: impl Fn() -> bool + 'static) -> OperationGuard {
    OperationGuard
}
#[cfg(not(windows))]
pub fn has_pending(_: &gpui::Window) -> Result<bool, String> {
    Ok(false)
}
#[cfg(not(windows))]
pub fn take_error(_: &gpui::Window) -> Result<Option<String>, String> {
    Ok(None)
}
#[cfg(not(windows))]
pub fn begin_move(_: &gpui::Window, _: &HostState) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn register_probe_window(_: &gpui::Window) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn set_no_activate(_: &gpui::Window, _: bool) -> Result<(), String> {
    Ok(())
}

/// An explicit initial Show request. STARTUPINFO may have hidden the first
/// show when QA launches a console process without a terminal window.
#[cfg(windows)]
fn raw_show_no_activate(hwnd: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
pub fn show_no_activate(_: &gpui::Window) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, windows))]
#[path = "main_presentation_tests.rs"]
mod main_presentation_tests;

#[cfg(all(test, windows))]
#[path = "overlay_frame_tests.rs"]
mod overlay_frame_tests;

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    use windows::{
        core::w,
        Win32::{
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_POPUP},
        },
    };

    #[test]
    fn queued_menu_show_rechecks_original_lease_and_inherited_owner_at_owned_hwnd_dispatch() {
        cliphost_core::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join(".tmp")
            .join("native-menu-queue");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::tempdir_in(root).unwrap();
        let host = HostState::open(
            cliphost_core::AppPaths::from_data_root(directory.path().join("data")),
            29065,
        )
        .unwrap();
        host.show_web_route("/main/home");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("Native menu queue fixture"),
                WS_POPUP,
                0,
                0,
                32,
                32,
                None,
                None,
                Some(GetModuleHandleW(None).unwrap().into()),
                None,
            )
        }
        .unwrap();
        struct Owner(windows::Win32::Foundation::HWND);
        impl Drop for Owner {
            fn drop(&mut self) {
                NATIVE_QUEUE.with(|queues| {
                    queues.borrow_mut().remove(&(self.0 .0 as isize));
                });
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        let _owner = Owner(hwnd);
        let calls = Rc::new(Cell::new(0));
        let owner_live = Rc::new(Cell::new(true));
        let owner_gate = owner_live.clone();
        let _parent = guard_condition(move || owner_gate.get());
        let enqueue = |lease| {
            let host = host.clone();
            let _lease =
                guard_condition_inherited(move || host.native_context_menu_is_current(lease));
            let calls = calls.clone();
            enqueue_native(
                hwnd,
                Box::new(move |_| {
                    calls.set(calls.get() + 1);
                    Ok(())
                }),
            );
        };

        assert!(host.acquire_native_context_menu(41, "main"));
        assert!(host.set_native_context_menu_hwnd(41, hwnd.0 as isize));
        enqueue(41);
        assert!(host.release_native_context_menu(41));
        assert!(host.acquire_native_context_menu(42, "main"));
        unsafe {
            dispatch_native(hwnd);
        }
        assert_eq!(
            calls.get(),
            0,
            "queued A cannot display after replacement by B"
        );
        assert!(host.native_context_menu_is_current(42));
        assert!(!host.set_native_context_menu_hwnd(41, hwnd.0 as isize));

        enqueue(42);
        owner_live.set(false);
        unsafe {
            dispatch_native(hwnd);
        }
        assert_eq!(
            calls.get(),
            0,
            "current lease cannot bypass the inherited owner guard"
        );
        owner_live.set(true);
        enqueue(42);
        unsafe {
            dispatch_native(hwnd);
        }
        assert_eq!(calls.get(), 1, "a current lease and owner run exactly once");
        assert!(host.release_native_context_menu(42));
    }
}
