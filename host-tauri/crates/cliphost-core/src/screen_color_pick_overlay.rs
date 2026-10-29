//! Win32 half of the screen eyedropper. Never compiled into unit tests of this
//! crate: it would put a topmost full-screen window over the developer's desktop.
//!
//! Every GDI object, DC and the window is owned by a guard so each exit path
//! (success, cancel, error, early return) releases it. The snapshot is wiped
//! before its bitmap is freed.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Once,
    },
    time::Instant,
};

use windows::{
    core::w,
    Win32::{
        Foundation::{BOOL, COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED},
            Gdi::{
                BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection,
                CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint,
                FillRect, FrameRect, GdiFlush, GetDC, InvalidateRect, MonitorFromPoint,
                ReleaseDC, SelectObject,
                SetBkMode, SetStretchBltMode, SetTextColor, StretchDIBits, UpdateWindow,
                BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, CLEARTYPE_QUALITY,
                CLIP_DEFAULT_PRECIS, COLORONCOLOR, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
                DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_SEMIBOLD, HBITMAP, HBRUSH,
                HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
            },
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{
                GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
            },
            Input::KeyboardAndMouse::VK_ESCAPE,
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
                GetSystemMetrics, GetWindowLongPtrW, IsWindow, LoadCursorW,
                MsgWaitForMultipleObjects, PeekMessageW, RegisterClassW, SetForegroundWindow,
                SetWindowLongPtrW, SetWindowPos, TranslateMessage, CREATESTRUCTW, GWLP_USERDATA,
                HWND_TOPMOST, IDC_CROSS, MSG, PM_REMOVE, QS_ALLINPUT, SM_CXVIRTUALSCREEN,
                SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_SHOWWINDOW,
                WA_INACTIVE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN,
                WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
                WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_POPUP,
            },
        },
    },
};

use super::{
    contrast_color, hex_text, magnifier_rect, pixel_at, sample_grid, scale_for_dpi, snapshot_point,
    MagnifierMetrics, PixelRect, VirtualBounds, GRID, MAX_SCALE, OVERLAY_TIMEOUT,
};

/// How long the overlay may stay inactive before the run is reported as failed
/// (Windows refused to hand it the foreground).
const ACTIVATION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// Message-pump wake interval; also the `still_current` polling period.
const POLL_MS: u32 = 50;
const CLASS_NAME: windows::core::PCWSTR = w!("ClipHostScreenColorPicker");

struct ThreadDpi(DPI_AWARENESS_CONTEXT);

impl Drop for ThreadDpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(HWND::default(), self.0);
        }
    }
}

/// A memory DC with one bitmap selected into it.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl Surface {
    /// Top-down 32-bit DIB whose pixels this process can read directly.
    unsafe fn dib(reference: HDC, width: i32, height: i32) -> Result<Self, String> {
        let dc = CreateCompatibleDC(reference);
        if dc.is_invalid() {
            return Err("无法创建屏幕缓冲区。".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)
        {
            Ok(bitmap) if !bits.is_null() => bitmap,
            _ => {
                let _ = DeleteDC(dc);
                return Err("无法为屏幕截图分配内存。".into());
            }
        };
        Ok(Self {
            dc,
            bitmap,
            previous: SelectObject(dc, bitmap),
            bits: bits.cast(),
            len: width as usize * height as usize * 4,
        })
    }

    /// Scratch bitmap the magnifier is composed in before it is blitted.
    unsafe fn compatible(reference: HDC, width: i32, height: i32) -> Result<Self, String> {
        let dc = CreateCompatibleDC(reference);
        let bitmap = CreateCompatibleBitmap(reference, width, height);
        if dc.is_invalid() || bitmap.is_invalid() {
            if !bitmap.is_invalid() {
                let _ = DeleteObject(bitmap);
            }
            if !dc.is_invalid() {
                let _ = DeleteDC(dc);
            }
            return Err("无法创建放大镜缓冲区。".into());
        }
        Ok(Self {
            dc,
            bitmap,
            previous: SelectObject(dc, bitmap),
            bits: std::ptr::null_mut(),
            len: 0,
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if !self.bits.is_null() {
                // The snapshot may contain anything on screen; wipe it first.
                std::ptr::write_bytes(self.bits, 0, self.len);
            }
            let _ = SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Brush(HBRUSH);

impl Brush {
    unsafe fn solid((r, g, b): (u8, u8, u8)) -> Self {
        Self(CreateSolidBrush(colorref((r, g, b))))
    }
}

impl Drop for Brush {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0);
        }
    }
}

struct Font(HFONT);

impl Font {
    /// Segoe UI semibold, `height` pixels tall (scaled with the monitor DPI).
    unsafe fn create(height: i32) -> Self {
        Self(CreateFontW(
            -height,
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET.0),
            u32::from(OUT_DEFAULT_PRECIS.0),
            u32::from(CLIP_DEFAULT_PRECIS.0),
            u32::from(CLEARTYPE_QUALITY.0),
            u32::from(DEFAULT_PITCH.0) | u32::from(FF_DONTCARE.0),
            w!("Segoe UI"),
        ))
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0);
        }
    }
}

struct WindowGuard(HWND);

impl Drop for WindowGuard {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.0).as_bool() {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

fn colorref((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn fill(dc: HDC, rect: &RECT, rgb: (u8, u8, u8)) {
    unsafe {
        let brush = Brush::solid(rgb);
        FillRect(dc, rect, brush.0);
    }
}

fn inset(rect: RECT, by: i32) -> RECT {
    RECT {
        left: rect.left + by,
        top: rect.top + by,
        right: rect.right - by,
        bottom: rect.bottom - by,
    }
}

fn rect_of(rect: PixelRect) -> RECT {
    RECT {
        left: rect.x,
        top: rect.y,
        right: rect.x + rect.width,
        bottom: rect.y + rect.height,
    }
}

/// State shared by the pump in `run` and the window procedure (same thread).
struct Overlay {
    /// The window starts at the virtual-screen origin, so client coordinates are
    /// snapshot indices: this is the virtual screen translated to (0, 0).
    client: VirtualBounds,
    /// Screen position of client (0, 0); needed to find the monitor under the cursor.
    origin: (i32, i32),
    snapshot_dc: HDC,
    snapshot_bits: *const u8,
    snapshot_len: usize,
    scratch_dc: HDC,
    /// Font for the current magnifier scale; replaced when the scale changes.
    font: RefCell<Font>,
    font_height: Cell<i32>,
    /// dpi / 96 of the monitor the cursor is on (size of the magnifier).
    scale: Cell<f32>,
    cursor: Cell<Option<(i32, i32)>>,
    magnifier: Cell<Option<PixelRect>>,
    left_down: Cell<bool>,
    right_down: Cell<bool>,
    activated: Cell<bool>,
    /// Closed by the user (click, Esc, right click) rather than by focus loss.
    user_exit: Cell<bool>,
    result: Cell<Option<(u8, u8, u8)>>,
    done: Cell<bool>,
}

impl Overlay {
    fn rgb_at(&self, x: i32, y: i32) -> Option<(u8, u8, u8)> {
        // SAFETY: the DIB outlives the overlay window; nothing writes it after capture.
        let pixels = unsafe { std::slice::from_raw_parts(self.snapshot_bits, self.snapshot_len) };
        pixel_at(pixels, self.client.width, self.client.height, x, y)
    }

    fn magnifier_at(&self, cursor: (i32, i32)) -> PixelRect {
        magnifier_rect(cursor, self.client, self.scale.get())
    }

    /// Effective DPI of the monitor under a client-space point, as a scale.
    /// Any failure means 1.0 (the unscaled magnifier).
    fn scale_at(&self, cursor: (i32, i32)) -> f32 {
        unsafe {
            let point = POINT {
                x: self.origin.0 + cursor.0,
                y: self.origin.1 + cursor.1,
            };
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
            match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) {
                Ok(()) => scale_for_dpi(dpi_x),
                Err(_) => 1.0,
            }
        }
    }

    /// Switches to the scale of the monitor under `cursor`.
    fn track_scale(&self, cursor: (i32, i32)) {
        self.scale.set(self.scale_at(cursor));
    }

    fn finish(&self, hwnd: HWND, user: bool) {
        if self.done.replace(true) {
            return;
        }
        self.user_exit.set(user);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    fn invalidate(&self, hwnd: HWND, rect: Option<PixelRect>) {
        if let Some(rect) = rect {
            let rect = rect_of(rect);
            unsafe {
                let _ = InvalidateRect(hwnd, Some(&rect), false);
            }
        }
    }

    fn move_to(&self, hwnd: HWND, x: i32, y: i32) {
        let point = (
            x.clamp(0, (self.client.width - 1).max(0)),
            y.clamp(0, (self.client.height - 1).max(0)),
        );
        let old = self.magnifier.get();
        self.track_scale(point);
        let new = self.magnifier_at(point);
        self.cursor.set(Some(point));
        self.magnifier.set(Some(new));
        // Only the old and new magnifier areas repaint, never the whole screen.
        self.invalidate(hwnd, old);
        self.invalidate(hwnd, Some(new));
    }

    fn click(&self, hwnd: HWND) {
        let picked = self
            .cursor
            .get()
            .and_then(|(x, y)| self.rgb_at(x, y));
        if let Some(rgb) = picked {
            self.result.set(Some(rgb));
            self.finish(hwnd, true);
        }
    }

    fn paint(&self, hwnd: HWND) {
        unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            if !dc.is_invalid() {
                let area = paint.rcPaint;
                let _ = BitBlt(
                    dc,
                    area.left,
                    area.top,
                    area.right - area.left,
                    area.bottom - area.top,
                    self.snapshot_dc,
                    area.left,
                    area.top,
                    SRCCOPY,
                );
                if let Some(cursor) = self.cursor.get() {
                    let rect = self.magnifier_at(cursor);
                    let outside = rect.x >= area.right
                        || rect.y >= area.bottom
                        || rect.x + rect.width <= area.left
                        || rect.y + rect.height <= area.top;
                    if !outside {
                        self.compose_magnifier(cursor);
                        let _ = BitBlt(
                            dc,
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.height,
                            self.scratch_dc,
                            0,
                            0,
                            SRCCOPY,
                        );
                    }
                }
            }
            let _ = EndPaint(hwnd, &paint);
        }
    }

    /// Draws frame, 11 x 11 grid enlarged by the scaled cell size, centre-cell
    /// outline and `#RRGGBB`, all sized for the current monitor scale.
    unsafe fn compose_magnifier(&self, cursor: (i32, i32)) {
        let metrics = MagnifierMetrics::for_scale(self.scale.get());
        let (frame, cell_px, grid_px) = (metrics.frame, metrics.cell, metrics.grid());
        let dc = self.scratch_dc;
        let full = RECT {
            left: 0,
            top: 0,
            right: metrics.width(),
            bottom: metrics.height(),
        };
        // Black hairline, white hairline, dark body: readable on any content.
        fill(dc, &full, (0, 0, 0));
        fill(dc, &inset(full, 1.min(frame)), (255, 255, 255));
        fill(dc, &inset(full, frame), (28, 28, 28));

        let mut cells = [0u32; (GRID * GRID) as usize];
        for (row, line) in sample_grid(cursor, self.client).iter().enumerate() {
            for (column, &(x, y)) in line.iter().enumerate() {
                let (r, g, b) = self.rgb_at(x, y).unwrap_or((0, 0, 0));
                // BGRA little-endian: 0x00RRGGBB.
                cells[row * GRID as usize + column] =
                    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
            }
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: GRID,
                biHeight: -GRID,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let _ = SetStretchBltMode(dc, COLORONCOLOR);
        StretchDIBits(
            dc,
            frame,
            frame,
            grid_px,
            grid_px,
            0,
            0,
            GRID,
            GRID,
            Some(cells.as_ptr().cast::<c_void>()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );

        let center = self.rgb_at(cursor.0, cursor.1).unwrap_or((0, 0, 0));
        let half = GRID / 2;
        let cell = RECT {
            left: frame + half * cell_px,
            top: frame + half * cell_px,
            right: frame + (half + 1) * cell_px,
            bottom: frame + (half + 1) * cell_px,
        };
        let outline = Brush::solid(contrast_color(center));
        FrameRect(dc, &cell, outline.0);

        let mut text = RECT {
            left: frame,
            top: frame + grid_px,
            right: metrics.width() - frame,
            bottom: frame + grid_px + metrics.text_height,
        };
        let mut label: Vec<u16> = hex_text(center).encode_utf16().collect();
        if self.font_height.get() != metrics.font {
            // Drops (and deletes) the previous font only after the new one exists.
            *self.font.borrow_mut() = Font::create(metrics.font);
            self.font_height.set(metrics.font);
        }
        let previous = SelectObject(dc, self.font.borrow().0);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, colorref((255, 255, 255)));
        DrawTextW(dc, &mut label, &mut text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(dc, previous);
    }
}

fn point_of(lparam: LPARAM) -> (i32, i32) {
    (
        i32::from((lparam.0 & 0xffff) as u16 as i16),
        i32::from(((lparam.0 >> 16) & 0xffff) as u16 as i16),
    )
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Overlay;
    if state.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    if message == WM_NCDESTROY {
        // `run` owns the state; later messages must not see a dangling pointer.
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let overlay = &*state;
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            overlay.paint(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_of(lparam);
            overlay.move_to(hwnd, x, y);
            LRESULT(0)
        }
        // The button must go down and up on the overlay, so the click that
        // opened the eyedropper (or its release) is never taken as a pick.
        WM_LBUTTONDOWN => {
            let (x, y) = point_of(lparam);
            overlay.move_to(hwnd, x, y);
            overlay.left_down.set(true);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if overlay.left_down.replace(false) {
                let (x, y) = point_of(lparam);
                overlay.move_to(hwnd, x, y);
                overlay.click(hwnd);
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            overlay.right_down.set(true);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if overlay.right_down.replace(false) {
                overlay.finish(hwnd, true);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            overlay.finish(hwnd, true);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
                // Alt+Tab, another window, the Start menu: cancel the pick.
                if overlay.activated.get() {
                    overlay.finish(hwnd, false);
                }
            } else {
                overlay.activated.set(true);
            }
            LRESULT(0)
        }
        WM_ACTIVATEAPP => {
            if wparam.0 == 0 && overlay.activated.get() {
                overlay.finish(hwnd, false);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            overlay.finish(hwnd, false);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn register_class() -> Result<(), String> {
    static REGISTER: Once = Once::new();
    static FAILED: AtomicBool = AtomicBool::new(false);
    REGISTER.call_once(|| unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            FAILED.store(true, Ordering::Relaxed);
        }
    });
    if FAILED.load(Ordering::Relaxed) {
        Err("无法注册屏幕吸管窗口。".into())
    } else {
        Ok(())
    }
}

pub(super) fn run(
    owner: isize,
    still_current: &dyn Fn() -> bool,
) -> Result<Option<(u8, u8, u8)>, String> {
    unsafe { run_unsafe(owner, still_current) }
}

unsafe fn run_unsafe(
    owner: isize,
    still_current: &dyn Fn() -> bool,
) -> Result<Option<(u8, u8, u8)>, String> {
    // The overlay lives on this worker thread; make it physical-pixel based
    // whatever the process default is, and put the thread back afterwards.
    let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let _dpi = (!previous.0.is_null()).then(|| ThreadDpi(previous));

    let bounds = VirtualBounds {
        x: GetSystemMetrics(SM_XVIRTUALSCREEN),
        y: GetSystemMetrics(SM_YVIRTUALSCREEN),
        width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
        height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
    };
    if !bounds.is_capturable() {
        return Err("无法确定屏幕范围。".into());
    }
    if !still_current() {
        return Ok(None);
    }

    // Declared in drop-safe order: the window goes first, the snapshot last.
    let screen = ScreenDc(GetDC(HWND::default()));
    if screen.0.is_invalid() {
        return Err("无法访问屏幕。".into());
    }
    let snapshot = Surface::dib(screen.0, bounds.width, bounds.height)?;
    BitBlt(
        snapshot.dc,
        0,
        0,
        bounds.width,
        bounds.height,
        screen.0,
        bounds.x,
        bounds.y,
        SRCCOPY | CAPTUREBLT,
    )
    .map_err(|_| "无法截取屏幕。".to_string())?;
    let _ = GdiFlush();
    // One scratch bitmap big enough for the largest scale; smaller ones use its corner.
    let largest = MagnifierMetrics::for_scale(MAX_SCALE);
    let scratch = Surface::compatible(screen.0, largest.width(), largest.height())?;
    drop(screen);
    let start_font = MagnifierMetrics::for_scale(1.0).font;
    let font = Font::create(start_font);
    register_class()?;

    let overlay = Overlay {
        client: VirtualBounds {
            x: 0,
            y: 0,
            width: bounds.width,
            height: bounds.height,
        },
        snapshot_dc: snapshot.dc,
        snapshot_bits: snapshot.bits,
        snapshot_len: snapshot.len,
        scratch_dc: scratch.dc,
        origin: (bounds.x, bounds.y),
        font: RefCell::new(font),
        font_height: Cell::new(start_font),
        scale: Cell::new(1.0),
        cursor: Cell::new(None),
        magnifier: Cell::new(None),
        left_down: Cell::new(false),
        right_down: Cell::new(false),
        activated: Cell::new(false),
        user_exit: Cell::new(false),
        result: Cell::new(None),
        done: Cell::new(false),
    };
    let mut at = POINT::default();
    if GetCursorPos(&mut at).is_ok() {
        // A cursor outside the virtual screen (rare) starts at the nearest edge.
        let start = snapshot_point((at.x, at.y), bounds).unwrap_or((
            (at.x - bounds.x).clamp(0, bounds.width - 1),
            (at.y - bounds.y).clamp(0, bounds.height - 1),
        ));
        overlay.cursor.set(Some(start));
        if let Some(cursor) = overlay.cursor.get() {
            overlay.track_scale(cursor);
            overlay.magnifier.set(Some(overlay.magnifier_at(cursor)));
        }
    }

    let instance = GetModuleHandleW(None).map_err(|error| error.to_string())?;
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS_NAME,
        w!(""),
        WS_POPUP,
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
        HWND::default(),
        None,
        instance,
        Some(&overlay as *const Overlay as *const c_void),
    )
    .map_err(|error| format!("无法创建屏幕吸管窗口：{error}"))?;
    let window = WindowGuard(hwnd);
    // No fade-in: the frozen frame must replace the live desktop at once.
    let disabled = BOOL(1);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_TRANSITIONS_FORCEDISABLED,
        &disabled as *const _ as *const c_void,
        std::mem::size_of_val(&disabled) as u32,
    );
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
        SWP_SHOWWINDOW,
    )
    .map_err(|error| format!("无法显示屏幕吸管窗口：{error}"))?;
    let _ = SetForegroundWindow(hwnd);
    let _ = UpdateWindow(hwnd);

    let started = Instant::now();
    let mut failure = None;
    let mut message = MSG::default();
    'pump: loop {
        while PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            if message.message == WM_QUIT {
                break 'pump;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if overlay.done.get() {
            break;
        }
        if !still_current() || started.elapsed() >= OVERLAY_TIMEOUT {
            overlay.finish(hwnd, false);
            break;
        }
        if !overlay.activated.get() && started.elapsed() >= ACTIVATION_GRACE {
            failure = Some("屏幕吸管未能获得焦点。".to_string());
            overlay.finish(hwnd, false);
            break;
        }
        MsgWaitForMultipleObjects(None, false, POLL_MS, QS_ALLINPUT);
    }

    drop(window);
    let picked = overlay.result.get();
    if overlay.user_exit.get() && owner != 0 {
        // The user decided: hand the foreground back to the settings window.
        let owner = HWND(owner as *mut c_void);
        if IsWindow(owner).as_bool() {
            let _ = SetForegroundWindow(owner);
        }
    }
    // `overlay` (and its font), `scratch` and `snapshot` drop here, wiping the capture.
    match failure {
        Some(error) => Err(error),
        None => Ok(picked),
    }
}
