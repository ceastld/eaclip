//! Owner-thread native shadow and rounded clipping, independent of Win11 DWM.
use std::cell::{Cell, RefCell};
use std::sync::{Arc, OnceLock};
use crate::{window_corner_capture::{CaptureState, Geometry as CaptureGeometry},
    window_shadow_pixels::{CornerColors, CornerPatch}};
use windows::{core::w, Win32::{
    Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    Graphics::{Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED}, Gdi::*},
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::GetDpiForWindow, Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*},
}};

const EFFECTS_SUBCLASS: usize = 173;
const SHADOW_CLASS: windows::core::PCWSTR = w!("ClipHost.NativeWindowShadow");
const CORNER_TIMER: usize = 0x2a51;
const CORNER_SETTLE_MS: u32 = 100;

struct Binding {
    depth: Cell<u32>,
    closing: Cell<bool>,
    updating: Cell<bool>,
    tucked: Cell<bool>,
    shadow: RefCell<Shadow>,
    capture: Arc<CaptureState>,
}

// Native calls can synchronously re-enter the owner, including destruction.
// Keep the binding alive until the last callback/native operation has returned.
struct Lease(*mut Binding);
impl Lease {
    unsafe fn new(pointer: *mut Binding) -> Self {
        let binding = &*pointer;
        binding.depth.set(binding.depth.get() + 1);
        Self(pointer)
    }
    unsafe fn binding(&self) -> &Binding { &*self.0 }
}
impl Drop for Lease {
    fn drop(&mut self) {
        unsafe {
            let binding = &*self.0;
            let depth = binding.depth.get() - 1;
            binding.depth.set(depth);
            if depth == 0 && binding.closing.get() { drop(Box::from_raw(self.0)); }
        }
    }
}

impl Binding {
    unsafe fn sync(&self, owner: HWND) {
        if self.closing.get() || self.updating.replace(true) { return; }
        // SetWindowRgn generates another WINDOWPOSCHANGED; it must not borrow
        // the drawing state or update the region recursively.
        let result = self.shadow.borrow_mut().sync(owner, self.tucked.get());
        self.updating.set(false);
        if let Err(error) = result {
            self.shadow.borrow_mut().hide();
            tracing::debug!(%error, "Window shadow update unavailable");
        }
    }
    unsafe fn begin_capture(&self, owner: HWND) {
        if self.updating.get() { SetTimer(owner, CORNER_TIMER, CORNER_SETTLE_MS, None); return; }
        if self.closing.get() || self.tucked.get() || !IsWindowVisible(owner).as_bool()
            || IsIconic(owner).as_bool()
            || cliphost_core::window_chrome::is_in_move_size(owner.0 as isize) { return; }
        let Some(geometry) = self.shadow.borrow().capture_geometry() else { return; };
        match self.capture.start(owner, geometry) {
            Ok(false) => { SetTimer(owner, CORNER_TIMER, CORNER_SETTLE_MS, None); }
            Err(error) => tracing::debug!(%error, "Corner color capture unavailable"),
            _ => {}
        }
    }
    unsafe fn apply_capture(&self, owner: HWND) {
        if self.closing.get() || self.updating.get() {
            if !self.closing.get() { SetTimer(owner, CORNER_TIMER, CORNER_SETTLE_MS, None); }
            return;
        }
        let Some(captured) = self.capture.take() else { return; };
        if self.closing.get() { return; }
        let mut shadow = self.shadow.borrow_mut();
        if shadow.capture_geometry() != Some(captured.geometry) { return; }
        if shadow.corner_colors.as_ref() == Some(&captured.colors) { return; }
        tracing::debug!(owner = owner.0 as usize, "Rounded corner colors refreshed");
        shadow.corner_colors = Some(captured.colors);
        shadow.colors_dirty = true;
        drop(shadow);
        self.sync(owner);
    }
}

pub fn install(owner: HWND) -> Result<(), String> {
    unsafe {
        let mut previous = 0;
        if GetWindowSubclass(owner, Some(owner_proc), EFFECTS_SUBCLASS, Some(&mut previous)).as_bool() { return Ok(()); }
        let instance = shadow_instance()?;
        let hwnd = CreateWindowExW(WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            SHADOW_CLASS, w!("ClipHost shadow"), WS_POPUP, 0, 0, 1, 1,
            owner, None, instance, None).map_err(|error| error.to_string())?;
        let disabled = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const _ as *const _, 4);
        let pointer = Box::into_raw(Box::new(Binding {
            depth: Cell::new(0), closing: Cell::new(false), updating: Cell::new(false),
            tucked: Cell::new(false), shadow: RefCell::new(Shadow::new(hwnd)),
            capture: Arc::new(CaptureState::default()),
        }));
        if !SetWindowSubclass(owner, Some(owner_proc), EFFECTS_SUBCLASS, pointer as usize).as_bool() {
            drop(Box::from_raw(pointer));
            return Err("Could not register window shadow lifecycle".into());
        }
        let lease = Lease::new(pointer);
        lease.binding().sync(owner);
        tracing::info!(owner = owner.0 as usize, shadow = hwnd.0 as usize, "Native window shadow registered");
        Ok(())
    }
}

/// Coalesce layout/theme changes. Movement and animation never capture pixels.
pub fn request_corner_refresh(owner: HWND) {
    unsafe {
        let mut data = 0;
        if GetWindowSubclass(owner, Some(owner_proc), EFFECTS_SUBCLASS, Some(&mut data)).as_bool() {
            let lease = Lease::new(data as *mut Binding);
            let binding = lease.binding();
            if binding.closing.get() { return; }
            binding.capture.invalidate();
            SetTimer(owner, CORNER_TIMER, CORNER_SETTLE_MS, None);
        }
    }
}

pub fn set_tucked(owner: HWND, tucked: bool) {
    unsafe {
        let mut data = 0;
        if GetWindowSubclass(owner, Some(owner_proc), EFFECTS_SUBCLASS, Some(&mut data)).as_bool() {
            let lease = Lease::new(data as *mut Binding);
            let binding = lease.binding();
            binding.tucked.set(tucked);
            binding.sync(owner);
        }
    }
}

unsafe extern "system" fn owner_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, data: usize) -> LRESULT {
    let lease = Lease::new(data as *mut Binding);
    let binding = lease.binding();
    if msg == WM_NCDESTROY {
        binding.closing.set(true);
        binding.capture.close();
        let _ = KillTimer(hwnd, CORNER_TIMER);
        let _ = RemoveWindowSubclass(hwnd, Some(owner_proc), id);
    }
    if msg == WM_TIMER && wp.0 == CORNER_TIMER {
        let _ = KillTimer(hwnd, CORNER_TIMER);
        binding.apply_capture(hwnd);
        binding.begin_capture(hwnd);
        return LRESULT(0);
    }
    if msg == crate::window_corner_capture::READY {
        binding.apply_capture(hwnd);
        return LRESULT(0);
    }
    let result = DefSubclassProc(hwnd, msg, wp, lp);
    if matches!(msg, WM_WINDOWPOSCHANGED | WM_SIZE | WM_SHOWWINDOW | WM_DPICHANGED | WM_DISPLAYCHANGE) {
        binding.sync(hwnd);
    }
    if (msg == WM_SHOWWINDOW && wp.0 != 0) || msg == WM_EXITSIZEMOVE {
        request_corner_refresh(hwnd);
    }
    result
}

unsafe fn shadow_instance() -> Result<HINSTANCE, String> {
    static INSTANCE: OnceLock<Result<usize, String>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let instance = GetModuleHandleW(None).map_err(|error| error.to_string())?;
        let class = WNDCLASSW { lpfnWndProc: Some(shadow_proc), hInstance: instance.into(),
            lpszClassName: SHADOW_CLASS, ..Default::default() };
        if RegisterClassW(&class) == 0 { return Err("Could not register shadow window class".into()); }
        Ok(instance.0 as usize)
    }).as_ref().map(|instance| HINSTANCE(*instance as *mut _)).map_err(Clone::clone)
}

unsafe extern "system" fn shadow_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

struct Shadow {
    hwnd: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    raster_size: Option<(i32, i32, u32)>,
    region: Option<(i32, i32, i32, i32, u32)>,
    margin: i32,
    position: Option<(i32, i32)>,
    shown: bool,
    corner_colors: Option<CornerColors>,
    colors_dirty: bool,
}

impl Shadow {
    fn new(hwnd: HWND) -> Self {
        Self { hwnd, dc: HDC::default(), bitmap: HBITMAP::default(), original: HGDIOBJ::default(),
            raster_size: None, region: None, margin: 0, position: None, shown: false,
            corner_colors: None, colors_dirty: true }
    }
    fn capture_geometry(&self) -> Option<CaptureGeometry> {
        let (x, y, width, height, dpi) = self.region?;
        Some(CaptureGeometry { offset: (x, y), width, height, dpi })
    }
    fn colors_for_size(&self, size: i32) -> CornerColors {
        let corners = std::array::from_fn(|index| {
            let previous = self.corner_colors.as_ref().map(|colors| &colors.corners[index]);
            let mut pixels = Vec::with_capacity((size * size) as usize);
            for y in 0..size { for x in 0..size {
                let color = previous.filter(|patch| patch.size > 0).map(|patch| {
                    let sx = x * patch.size / size;
                    let sy = y * patch.size / size;
                    patch.pixels[(sy * patch.size + sx) as usize]
                }).unwrap_or([27, 27, 27]);
                pixels.push(color);
            } }
            CornerPatch { size, pixels }
        });
        CornerColors { corners }
    }
    unsafe fn hide(&mut self) {
        if self.shown { let _ = ShowWindow(self.hwnd, SW_HIDE); self.shown = false; }
    }
    unsafe fn sync(&mut self, owner: HWND, tucked: bool) -> Result<(), String> {
        if IsZoomed(owner).as_bool() {
            if self.region.take().is_some() { SetWindowRgn(owner, HRGN::default(), true); }
            self.hide(); return Ok(());
        }
        let mut outer = RECT::default();
        GetWindowRect(owner, &mut outer).map_err(|error| error.to_string())?;
        let mut client = RECT::default();
        GetClientRect(owner, &mut client).map_err(|error| error.to_string())?;
        let mut origin = POINT::default();
        if !ClientToScreen(owner, &mut origin).as_bool() { return Err("Cannot locate window content".into()); }
        let width = client.right - client.left;
        let height = client.bottom - client.top;
        if width <= 0 || height <= 0 { self.hide(); return Ok(()); }
        let dpi = GetDpiForWindow(owner);
        let dpi = if dpi == 0 { 96 } else { dpi };
        let region = (origin.x - outer.left, origin.y - outer.top, width, height, dpi);
        if self.region != Some(region) {
            // Store before setting: Windows sends synchronous position events.
            let previous = self.region.replace(region);
            let clip = full_coverage_region(region.0, region.1, width, height, dpi);
            if clip.is_invalid() || SetWindowRgn(owner, clip, IsWindowVisible(owner)) == 0 {
                if !clip.is_invalid() { let _ = DeleteObject(clip); }
                self.region = previous;
                return Err("Cannot apply native rounded clipping".into());
            }
            // Windows owns a successfully applied HRGN.
        }
        if tucked || IsIconic(owner).as_bool() || !IsWindowVisible(owner).as_bool() {
            self.hide(); return Ok(());
        }
        let dimensions = (width, height, dpi);
        let geometry_changed = self.raster_size != Some(dimensions);
        if geometry_changed || self.colors_dirty {
            let colors = self.colors_for_size(crate::window_shadow_pixels::corner_size(width, height, dpi));
            let raster = crate::window_shadow_pixels::render_with_corners(width, height, dpi, &colors)
                .ok_or_else(|| "Window shadow exceeds drawing budget".to_string())?;
            self.prepare(&raster)?;
            self.margin = raster.margin;
            let target = POINT { x: origin.x - raster.margin, y: origin.y - raster.margin };
            let size = SIZE { cx: raster.width, cy: raster.height };
            let source = POINT::default();
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0,
                SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(self.hwnd, None, Some(&target), Some(&size), self.dc,
                Some(&source), COLORREF(0), Some(&blend), ULW_ALPHA).map_err(|error| error.to_string())?;
            self.raster_size = Some(dimensions);
            self.colors_dirty = false;
            self.position = Some((target.x, target.y));
            if geometry_changed { request_corner_refresh(owner); }
        }
        let target = (origin.x - self.margin, origin.y - self.margin);
        // Moving/showing an owned popup must not restack its owner. NOZORDER
        // protects this shadow only; without NOOWNERZORDER Windows can leave
        // the topmost shadow above other apps while its content owner drops
        // behind them and appears transparent.
        let mut flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOSIZE;
        if !self.shown { flags |= SWP_SHOWWINDOW; }
        if self.position == Some(target) { flags |= SWP_NOMOVE; }
        if !self.shown || self.position != Some(target) {
            SetWindowPos(self.hwnd, HWND::default(), target.0, target.1, 0, 0, flags)
                .map_err(|error| error.to_string())?;
            self.position = Some(target);
            self.shown = true;
        }
        Ok(())
    }
    unsafe fn prepare(&mut self, raster: &crate::window_shadow_pixels::Raster) -> Result<(), String> {
        if self.dc.is_invalid() {
            self.dc = CreateCompatibleDC(None);
            if self.dc.is_invalid() { return Err("Cannot allocate shadow drawing context".into()); }
        }
        let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: raster.width,
            biHeight: -raster.height, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0,
            ..Default::default() }, ..Default::default() };
        let mut pixels = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut pixels, None, 0)
            .map_err(|error| error.to_string())?;
        let old = SelectObject(self.dc, bitmap);
        if old.is_invalid() { let _ = DeleteObject(bitmap); return Err("Cannot select shadow drawing buffer".into()); }
        if self.bitmap.is_invalid() { self.original = old; }
        else { let _ = DeleteObject(self.bitmap); }
        self.bitmap = bitmap;
        std::ptr::copy_nonoverlapping(raster.pixels.as_ptr(), pixels.cast::<u32>(), raster.pixels.len());
        Ok(())
    }
}

// HRGN itself has no antialiasing. Keep only fully covered pixels here; the
// existing layered companion supplies the fractional coverage around them.
unsafe fn full_coverage_region(x: i32, y: i32, width: i32, height: i32, dpi: u32) -> HRGN {
    let insets = crate::window_shadow_pixels::rounded_insets(width, height, dpi);
    let corner = insets.len() as i32;
    let region = CreateRectRgn(x, y + corner, x + width, y + height - corner);
    if region.is_invalid() { return region; }
    for (row, inset) in insets.into_iter().enumerate() {
        for py in [y + row as i32, y + height - 1 - row as i32] {
            let band = CreateRectRgn(x + inset, py, x + width - inset, py + 1);
            let ok = !band.is_invalid() && CombineRgn(region, region, band, RGN_OR) != RGN_ERROR;
            if !band.is_invalid() { let _ = DeleteObject(band); }
            if !ok { let _ = DeleteObject(region); return HRGN::default(); }
        }
    }
    region
}

impl Drop for Shadow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.hwnd).as_bool() { let _ = DestroyWindow(self.hwnd); }
            if !self.dc.is_invalid() {
                if !self.original.is_invalid() { SelectObject(self.dc, self.original); }
                if !self.bitmap.is_invalid() { let _ = DeleteObject(self.bitmap); }
                let _ = DeleteDC(self.dc);
            }
        }
    }
}
