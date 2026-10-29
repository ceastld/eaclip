//! Capture only this application's corner colors after layout has settled.
//! PrintWindow runs off the owner thread; a tiny mailbox returns to its subclass.
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM}, Graphics::Gdi::*,
    Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
    System::Threading::GetCurrentProcessId,
    UI::{HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
        WindowsAndMessaging::{GetWindowRect, GetWindowThreadProcessId, PostMessageW, PW_RENDERFULLCONTENT, WM_APP}},
};
use crate::window_shadow_pixels::{CornerColors, CornerPatch, corner_size, rounded_insets};

pub const READY: u32 = WM_APP + 0x2a51;
const MAX_CAPTURE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub offset: (i32, i32),
    pub width: i32,
    pub height: i32,
    pub dpi: u32,
}

pub struct Captured {
    generation: u64,
    pub geometry: Geometry,
    pub colors: CornerColors,
}

#[derive(Default)]
pub struct CaptureState {
    generation: AtomicU64,
    closed: AtomicBool,
    running: AtomicBool,
    pending: Mutex<Option<Captured>>,
}

impl CaptureState {
    pub fn invalidate(&self) { self.generation.fetch_add(1, Ordering::AcqRel); }
    pub fn close(&self) { self.closed.store(true, Ordering::Release); self.invalidate(); }
    fn is_current(&self, generation: u64) -> bool {
        !self.closed.load(Ordering::Acquire) && generation == self.generation.load(Ordering::Acquire)
    }
    pub fn take(&self) -> Option<Captured> {
        self.pending.lock().ok()?.take().filter(|value| {
            !self.closed.load(Ordering::Acquire)
                && value.generation == self.generation.load(Ordering::Acquire)
        })
    }
    pub fn start(self: &Arc<Self>, owner: HWND, geometry: Geometry) -> Result<bool, String> {
        if self.closed.load(Ordering::Acquire) { return Ok(true); }
        if self.running.swap(true, Ordering::AcqRel) { return Ok(false); }
        let generation = self.generation.load(Ordering::Acquire);
        let state = Arc::clone(self);
        let owner = owner.0 as usize;
        let spawned = std::thread::Builder::new().name("cliphost-corners".into()).spawn(move || {
            let colors = if !state.is_current(generation) { None }
                else { unsafe { capture(HWND(owner as *mut _), geometry, &state, generation) } };
            if !state.closed.load(Ordering::Acquire)
                && generation == state.generation.load(Ordering::Acquire) {
                if let Some(colors) = colors {
                    if let Ok(mut pending) = state.pending.lock() {
                        *pending = Some(Captured { generation, geometry, colors });
                    }
                }
            }
            state.running.store(false, Ordering::Release);
            if !state.closed.load(Ordering::Acquire) {
                unsafe { let _ = PostMessageW(HWND(owner as *mut _), READY, WPARAM(0), LPARAM(0)); }
            }
        });
        if let Err(error) = spawned {
            self.running.store(false, Ordering::Release);
            return Err(error.to_string());
        }
        Ok(true)
    }
}

struct Snapshot {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pixels: *mut u32,
    width: i32,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            if !self.old.is_invalid() { SelectObject(self.dc, self.old); }
            if !self.bitmap.is_invalid() { let _ = DeleteObject(self.bitmap); }
            if !self.dc.is_invalid() { let _ = DeleteDC(self.dc); }
        }
    }
}
impl Snapshot {
    unsafe fn new(width: i32, height: i32) -> Option<Self> {
        let count = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?;
        if count == 0 || count.checked_mul(4)? > MAX_CAPTURE_BYTES { return None; }
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() { return None; }
        let mut value = Self { dc, bitmap: HBITMAP::default(), old: HGDIOBJ::default(),
            pixels: std::ptr::null_mut(), width };
        let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: width,
            biHeight: -height, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0,
            ..Default::default() }, ..Default::default() };
        let mut pixels = std::ptr::null_mut();
        value.bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut pixels, None, 0).ok()?;
        value.old = SelectObject(dc, value.bitmap);
        if value.old.is_invalid() || pixels.is_null() { return None; }
        value.pixels = pixels.cast();
        std::ptr::write_bytes(value.pixels, 0, count);
        Some(value)
    }
    unsafe fn rgb(&self, x: i32, y: i32) -> [u8; 3] {
        let pixel = *self.pixels.add((y * self.width + x) as usize);
        [(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]
    }
}

unsafe fn capture(owner: HWND, geometry: Geometry, state: &CaptureState, generation: u64) -> Option<CornerColors> {
    if !state.is_current(generation) { return None; }
    let mut process = 0;
    GetWindowThreadProcessId(owner, Some(&mut process));
    if process != GetCurrentProcessId() { return None; }
    let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    if previous.0.is_null() { return None; }
    struct DpiGuard(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
    impl Drop for DpiGuard { fn drop(&mut self) { unsafe { SetThreadDpiAwarenessContext(self.0); } } }
    let _dpi = DpiGuard(previous);
    let mut rect = RECT::default();
    GetWindowRect(owner, &mut rect).ok()?;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if geometry.offset.0 < 0 || geometry.offset.1 < 0
        || geometry.offset.0 + geometry.width > width
        || geometry.offset.1 + geometry.height > height { return None; }
    let snapshot = Snapshot::new(width, height)?;
    GetWindowThreadProcessId(owner, Some(&mut process));
    if process != GetCurrentProcessId() || !state.is_current(generation) { return None; }
    if !PrintWindow(owner, snapshot.dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool() { return None; }
    let _ = GdiFlush();
    let size = corner_size(geometry.width, geometry.height, geometry.dpi);
    let insets = rounded_insets(geometry.width, geometry.height, geometry.dpi);
    let corners = std::array::from_fn(|corner| {
        let mut pixels = Vec::with_capacity((size * size) as usize);
        for y in 0..size { for x in 0..size {
            let (edge_x, edge_y) = (
                if corner & 1 != 0 { size - 1 - x } else { x },
                if corner & 2 != 0 { size - 1 - y } else { y });
            // Pixels removed by our full-coverage region cannot be captured.
            // Sample the closest included pixel, preserving each corner's tint
            // and wallpaper rather than substituting the root erase color.
            let mut nearest = (size, size);
            let mut distance = i32::MAX;
            for cy in 0..=size { for cx in 0..=size {
                if cy < size && cx < insets[cy as usize] { continue; }
                let candidate = (cx - edge_x).pow(2) + (cy - edge_y).pow(2);
                if candidate < distance { nearest = (cx, cy); distance = candidate; }
            } }
            let sx = geometry.offset.0 + if corner & 1 != 0 {
                geometry.width - 1 - nearest.0 } else { nearest.0 };
            let sy = geometry.offset.1 + if corner & 2 != 0 {
                geometry.height - 1 - nearest.1 } else { nearest.1 };
            pixels.push(snapshot.rgb(sx, sy));
        } }
        CornerPatch { size, pixels }
    });
    Some(CornerColors { corners })
}
