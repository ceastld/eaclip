//! Super Hub window JPEG preview, aligned with C# `WindowThumbnailCapture` + warmup hosted service.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const STORE_WIDTH: i32 = 420;
const LIVE_FRESHNESS: Duration = Duration::from_secs(12);
const MAX_ENTRIES: usize = 48;
// Capture the complete source geometry, then resize. Four 32-bit buffers
// conservatively cover native bitmap, readback, color conversion and resizing.
const MAX_SIDE: i64 = 16384;
const MAX_CAPTURE_WORK_BYTES: u64 = 128 * 1024 * 1024;

fn capture_dimensions(width: i64, height: i64) -> Option<(i32, i32)> {
    if !(8..=MAX_SIDE).contains(&width) || !(8..=MAX_SIDE).contains(&height) {
        return None;
    }
    let work = (width as u64).checked_mul(height as u64)?.checked_mul(16)?;
    (work <= MAX_CAPTURE_WORK_BYTES).then_some((width as i32, height as i32))
}

fn capture_print_flags(window: [i32; 4], monitor: Option<[i32; 4]>) -> [u32; 2] {
    let inside_monitor = monitor.is_some_and(|monitor| {
        window[0] >= monitor[0]
            && window[1] >= monitor[1]
            && window[2] <= monitor[2]
            && window[3] <= monitor[3]
    });
    if inside_monitor {
        [2, 0]
    } else {
        // Both PrintWindow modes can reuse clipped redirection surfaces.
        // Production asks the owner to WM_PRINT first for this geometry;
        // these remain compatibility fallbacks when it does not draw.
        [0, 2]
    }
}
const CAPTURE_SLOTS: i32 = 2;
const CAPTURE_WAIT: Duration = Duration::from_millis(120);
#[cfg(windows)]
const OWNER_PRINT_WAIT: Duration = Duration::from_secs(1);
const PRIORITY_CAP: usize = 64;

#[derive(Clone)]
struct CacheEntry {
    jpeg: Vec<u8>,
    encoded_width: i32,
    captured_at: Instant,
    last_access: Instant,
}

fn cache() -> &'static Mutex<HashMap<isize, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<isize, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn priority() -> &'static Mutex<VecDeque<isize>> {
    static Q: OnceLock<Mutex<VecDeque<isize>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn capture_slots() -> &'static AtomicI32 {
    static SLOTS: OnceLock<AtomicI32> = OnceLock::new();
    SLOTS.get_or_init(|| AtomicI32::new(CAPTURE_SLOTS))
}

struct CaptureGuard;

#[cfg(windows)]
enum OwnerFrame {
    Unavailable,
    Black,
    Image(Vec<u8>),
}

#[cfg(windows)]
fn owner_frame(pixels: Option<Vec<u8>>, width: u32, height: u32) -> OwnerFrame {
    match pixels {
        None => OwnerFrame::Unavailable,
        Some(pixels) if is_mostly_black(&pixels, width, height) => OwnerFrame::Black,
        Some(pixels) => OwnerFrame::Image(pixels),
    }
}

#[cfg(windows)]
fn choose_capture_pixels(
    owner: OwnerFrame,
    flags: &[u32],
    width: u32,
    height: u32,
    mut render: impl FnMut(u32) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    match owner {
        OwnerFrame::Black => None,
        OwnerFrame::Image(pixels) => Some(pixels),
        OwnerFrame::Unavailable => flags.iter().find_map(|flags| {
            render(*flags).filter(|pixels| !is_mostly_black(pixels, width, height))
        }),
    }
}

#[cfg(windows)]
fn uniformly_rgb(pixels: &[u8], color: u8) -> bool {
    if pixels.is_empty() || pixels.len() % 4 != 0 {
        return false;
    }
    // Check every RGB byte, ignoring GDI's undefined alpha. Four pixels per
    // block avoids millions of slice comparisons in large debug captures.
    let color = u128::from(color);
    let pixel = color | color << 8 | color << 16;
    let expected = pixel | pixel << 32 | pixel << 64 | pixel << 96;
    let mask = 0x00ff_ffff_00ff_ffff_00ff_ffff_00ff_ffff;
    let mut blocks = pixels.chunks_exact(16);
    if blocks.any(|block| u128::from_le_bytes(block.try_into().unwrap()) & mask != expected) {
        return false;
    }
    blocks
        .remainder()
        .chunks_exact(4)
        .all(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) & 0x00ff_ffff == pixel as u32)
}

#[cfg(windows)]
struct OwnerCapture {
    gate: CaptureGuard,
    pixels: Option<Vec<u8>>,
    #[cfg(test)]
    trace: Vec<(u32, bool, [[u8; 3]; 4])>,
    #[cfg(test)]
    bitmap_selected: Option<bool>,
}

/// Keep the source's borrowed DC alive until the real WM_PRINT handler returns.
/// A caller deadline does not free resources that a timed-out target still uses.
/// Hung owners retain one of the two existing slots, bounding workers/memory.
#[cfg(windows)]
fn capture_owner_with_deadline(
    hwnd: isize,
    width: i32,
    height: i32,
    gate: CaptureGuard,
    deadline: Duration,
) -> Option<OwnerCapture> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    #[cfg(test)]
    tests::observe_capture_flags(windows::Win32::UI::WindowsAndMessaging::WM_PRINT);
    thread::Builder::new()
        .name("cliphost-owner-print".into())
        .spawn(move || {
            let pixels = unsafe { capture_owner_pixels(hwnd, width, height) };
            let captured = OwnerCapture {
                gate,
                pixels,
                #[cfg(test)]
                trace: tests::take_render_trace(),
                #[cfg(test)]
                bitmap_selected: tests::take_bitmap_selected(),
            };
            // If the receiver timed out, dropping this answer releases the
            // slot only after the borrowed DC has actually been released.
            let _ = sender.send(captured);
        })
        .ok()?;
    receiver.recv_timeout(deadline).ok()
}

#[cfg(windows)]
unsafe fn capture_owner_pixels(hwnd: isize, width: i32, height: i32) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, PatBlt,
        ReleaseDC, SelectObject, BLACKNESS, WHITENESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindow, SendMessageW, PRF_CHILDREN, PRF_CLIENT, PRF_ERASEBKGND, PRF_NONCLIENT, WM_PRINT,
    };
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    if !IsWindow(hwnd).as_bool() {
        return None;
    }
    let screen = GetDC(HWND::default());
    if screen.0.is_null() {
        return None;
    }
    let mem = CreateCompatibleDC(screen);
    if mem.0.is_null() {
        let _ = ReleaseDC(HWND::default(), screen);
        return None;
    }
    let bmp = CreateCompatibleBitmap(screen, width, height);
    if bmp.0.is_null() {
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(HWND::default(), screen);
        return None;
    }
    let old = SelectObject(mem, bmp);
    if old.0.is_null() {
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(HWND::default(), screen);
        return None;
    }
    let render = |clear| {
        if SelectObject(mem, bmp).0.is_null() {
            return None;
        }
        let cleared = PatBlt(mem, 0, 0, width, height, clear).as_bool();
        if cleared {
            // WM_PRINT itself has no meaningful success return value. Its DC
            // contents establish whether the owner implemented the protocol.
            SendMessageW(
                hwnd,
                WM_PRINT,
                windows::Win32::Foundation::WPARAM(mem.0 as usize),
                windows::Win32::Foundation::LPARAM(
                    (PRF_CLIENT | PRF_NONCLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize,
                ),
            );
        }
        SelectObject(mem, old);
        let pixels = cleared
            .then(|| dib_bgra(mem, bmp, width, height))
            .flatten()?;
        #[cfg(test)]
        tests::observe_render(WM_PRINT, &pixels, width as u32, height as u32);
        Some(pixels)
    };
    let pixels = render(BLACKNESS).and_then(|pixels| {
        if !uniformly_rgb(&pixels, 0) {
            return Some(pixels);
        }
        drop(pixels);
        // All black may mean an actual black owner or no drawing at all.
        // A second white backing distinguishes those without changing the
        // existing mostly-black threshold or rejecting legitimate half black.
        render(WHITENESS).filter(|pixels| !uniformly_rgb(pixels, 255))
    });
    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = ReleaseDC(HWND::default(), screen);
    pixels
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        capture_slots().fetch_add(1, Ordering::AcqRel);
    }
}

fn acquire_capture() -> Option<CaptureGuard> {
    let deadline = Instant::now() + CAPTURE_WAIT;
    loop {
        let cur = capture_slots().load(Ordering::Acquire);
        if cur > 0
            && capture_slots()
                .compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return Some(CaptureGuard);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn try_get_cached_or_capture(hwnd: isize, max_width: i32) -> Option<Vec<u8>> {
    if hwnd == 0 {
        return None;
    }
    if !is_window(hwnd) {
        cache().lock().remove(&hwnd);
        return None;
    }
    if crate::clipboard::is_headless() {
        return serve_cache(hwnd);
    }
    let max_width = max_width.clamp(80, 640);
    let minimized = is_iconic(hwnd);
    if let Some(jpeg) = touch_fresh_cache(hwnd, max_width, minimized) {
        return Some(jpeg);
    }
    if minimized {
        return serve_cache(hwnd);
    }
    if let Some(live) = try_capture_jpeg(hwnd, STORE_WIDTH) {
        put(hwnd, live.clone(), STORE_WIDTH);
        return Some(live);
    }
    serve_cache(hwnd)
}

/// C# `WindowThumbnailCapture.TryWarmCapture`.
pub fn try_warm_capture(hwnd: isize, min_age: Duration) -> bool {
    if hwnd == 0 || crate::clipboard::is_headless() || !is_window(hwnd) || is_iconic(hwnd) {
        return false;
    }
    if let Some(existing) = cache().lock().get(&hwnd) {
        if existing.captured_at.elapsed() < min_age {
            return false;
        }
    }
    let Some(live) = try_capture_jpeg(hwnd, STORE_WIDTH) else {
        return false;
    };
    put(hwnd, live, STORE_WIDTH);
    true
}

/// C# `WindowThumbnailWarmupHostedService.Suggest`.
pub fn suggest(hwnd: isize) {
    if hwnd == 0 || crate::clipboard::is_headless() {
        return;
    }
    let mut q = priority().lock();
    if q.len() >= PRIORITY_CAP {
        q.pop_front();
    }
    q.push_back(hwnd);
}

/// C# `SuggestVisibleWindowList`.
pub fn suggest_visible_windows() {
    if crate::clipboard::is_headless() {
        return;
    }
    prune_dead();
    for hwnd in crate::window_menu::list_hwnds(0) {
        suggest(hwnd);
    }
}

pub fn ensure_warmup() {
    if crate::clipboard::is_headless() {
        return;
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = thread::Builder::new()
            .name("cliphost-thumb-warmup".into())
            .spawn(warmup_loop);
    });
}

fn warmup_loop() {
    loop {
        let hwnd = priority().lock().pop_front();
        match hwnd {
            Some(hwnd) => {
                let _ = try_warm_capture(hwnd, Duration::from_secs(20));
                thread::sleep(Duration::from_millis(180));
            }
            None => thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn prune_dead() {
    let dead: Vec<isize> = cache()
        .lock()
        .keys()
        .copied()
        .filter(|h| !is_window(*h))
        .collect();
    if dead.is_empty() {
        return;
    }
    let mut map = cache().lock();
    for h in dead {
        map.remove(&h);
    }
}

fn touch_fresh_cache(hwnd: isize, max_width: i32, minimized: bool) -> Option<Vec<u8>> {
    let mut map = cache().lock();
    let entry = map.get_mut(&hwnd)?;
    if entry.jpeg.is_empty() {
        return None;
    }
    entry.last_access = Instant::now();
    if minimized {
        return Some(entry.jpeg.clone());
    }
    if entry.captured_at.elapsed() < LIVE_FRESHNESS && entry.encoded_width + 40 >= max_width {
        return Some(entry.jpeg.clone());
    }
    None
}

fn serve_cache(hwnd: isize) -> Option<Vec<u8>> {
    let mut map = cache().lock();
    let entry = map.get_mut(&hwnd)?;
    if entry.jpeg.is_empty() {
        return None;
    }
    entry.last_access = Instant::now();
    Some(entry.jpeg.clone())
}

fn put(hwnd: isize, jpeg: Vec<u8>, encoded_width: i32) {
    if hwnd == 0 || jpeg.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut map = cache().lock();
    map.insert(
        hwnd,
        CacheEntry {
            jpeg,
            encoded_width,
            captured_at: now,
            last_access: now,
        },
    );
    if map.len() <= MAX_ENTRIES {
        return;
    }
    let overflow = map.len() - MAX_ENTRIES;
    let mut victims: Vec<(isize, Instant)> = map.iter().map(|(k, e)| (*k, e.last_access)).collect();
    victims.sort_by_key(|(_, at)| *at);
    for (key, _) in victims.into_iter().take(overflow.max(1)) {
        map.remove(&key);
    }
}

fn is_window(hwnd: isize) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        IsWindow(HWND(hwnd as *mut core::ffi::c_void)).as_bool()
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

fn is_iconic(hwnd: isize) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsIconic;
        IsIconic(HWND(hwnd as *mut core::ffi::c_void)).as_bool()
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

fn try_capture_jpeg(hwnd: isize, max_width: i32) -> Option<Vec<u8>> {
    try_capture_jpeg_using_flags(hwnd, max_width, None)
}

fn try_capture_jpeg_using_flags(
    hwnd: isize,
    max_width: i32,
    override_flags: Option<&[u32]>,
) -> Option<Vec<u8>> {
    let gate = acquire_capture()?;
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
            GetMonitorInfoW, MonitorFromRect, PatBlt, ReleaseDC, SelectObject, BLACKNESS,
            MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        if hwnd.0.is_null() || !IsWindow(hwnd).as_bool() {
            return None;
        }
        if is_iconic(hwnd.0 as isize) {
            return None;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return None;
        }
        let (width, height) = capture_dimensions(
            i64::from(rect.right) - i64::from(rect.left),
            i64::from(rect.bottom) - i64::from(rect.top),
        )?;
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor_bounds = GetMonitorInfoW(
            MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST),
            &mut monitor,
        )
        .as_bool()
        .then_some([
            monitor.rcMonitor.left,
            monitor.rcMonitor.top,
            monitor.rcMonitor.right,
            monitor.rcMonitor.bottom,
        ]);
        let native_flags = capture_print_flags(
            [rect.left, rect.top, rect.right, rect.bottom],
            monitor_bounds,
        );
        let flags = override_flags.unwrap_or(&native_flags);
        #[cfg(test)]
        tests::observe_source_geometry(
            [rect.left, rect.top, rect.right, rect.bottom],
            monitor_bounds,
            native_flags,
        );
        let (gate, owner) = if override_flags.is_none() && native_flags[0] == 0 {
            let captured = capture_owner_with_deadline(
                hwnd.0 as isize,
                width,
                height,
                gate,
                OWNER_PRINT_WAIT,
            )?;
            #[cfg(test)]
            {
                tests::append_render_trace(captured.trace);
                tests::set_bitmap_observation(captured.bitmap_selected);
            }
            (
                captured.gate,
                owner_frame(captured.pixels, width as u32, height as u32),
            )
        } else {
            (gate, OwnerFrame::Unavailable)
        };
        let _gate = gate;
        // A confirmed owner frame never enters the compositor fallback path,
        // including black: cached color must not replace current black pixels.
        if !matches!(&owner, OwnerFrame::Unavailable) {
            let pixels =
                choose_capture_pixels(owner, flags, width as u32, height as u32, |_| None)?;
            return encode_jpeg(&pixels, width as u32, height as u32, max_width);
        }
        let screen = GetDC(HWND::default());
        if screen.0.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        if mem.0.is_null() {
            let _ = ReleaseDC(HWND::default(), screen);
            return None;
        }
        let bmp = CreateCompatibleBitmap(screen, width, height);
        if bmp.0.is_null() {
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(HWND::default(), screen);
            return None;
        }
        let old = SelectObject(mem, bmp);
        if old.0.is_null() {
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(HWND::default(), screen);
            return None;
        }
        let render = |flags| {
            if SelectObject(mem, bmp).0.is_null() {
                return None;
            }
            #[cfg(test)]
            tests::observe_capture_flags(flags);
            let ok = PatBlt(mem, 0, 0, width, height, BLACKNESS).as_bool()
                && PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(flags)).as_bool();
            // GetDIBits requires an unselected bitmap, including retry paths.
            SelectObject(mem, old);
            let pixels = if ok {
                dib_bgra(mem, bmp, width, height)
            } else {
                None
            }?;
            #[cfg(test)]
            tests::observe_render(flags, &pixels, width as u32, height as u32);
            Some(pixels)
        };
        // Either mode can succeed with a black surface. Retry with the other
        // mode once before serving the existing cache.
        let pixels = choose_capture_pixels(
            OwnerFrame::Unavailable,
            flags,
            width as u32,
            height as u32,
            render,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(HWND::default(), screen);
        let pixels = pixels?;
        if is_mostly_black(&pixels, width as u32, height as u32) {
            return None;
        }
        encode_jpeg(&pixels, width as u32, height as u32, max_width)
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, max_width, override_flags, gate);
        None
    }
}

#[cfg(windows)]
pub(crate) unsafe fn dib_bgra(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    hbmp: windows::Win32::Graphics::Gdi::HBITMAP,
    width: i32,
    height: i32,
) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::{
        GetDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    #[cfg(test)]
    tests::observe_bitmap_selection(hdc, hbmp);
    let mut info = BITMAPINFO {
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
    let mut buf = vec![0u8; (width * height * 4) as usize];
    let copied = GetDIBits(
        hdc,
        hbmp,
        0,
        height as u32,
        Some(buf.as_mut_ptr().cast()),
        &mut info,
        DIB_RGB_COLORS,
    );
    if copied != height {
        return None;
    }
    Some(buf)
}

fn is_mostly_black(pixels: &[u8], width: u32, height: u32) -> bool {
    let mut samples = 0;
    let mut dark = 0;
    let step_y = (height / 8).max(1);
    let step_x = (width / 8).max(1);
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let i = ((y * width + x) * 4) as usize;
            if i + 2 < pixels.len() {
                samples += 1;
                if pixels[i] < 18 && pixels[i + 1] < 18 && pixels[i + 2] < 18 {
                    dark += 1;
                }
            }
            x += step_x;
        }
        y += step_y;
    }
    samples > 0 && dark * 10 >= samples * 9
}

fn encode_jpeg(bgra: &[u8], width: u32, height: u32, max_width: i32) -> Option<Vec<u8>> {
    // JPEG's raw encoder accepts RGB8/L8, not RGBA8. Window captures are
    // opaque; convert directly to RGB and avoid an unused alpha buffer.
    let mut rgb = Vec::with_capacity(bgra.len() / 4 * 3);
    for chunk in bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[chunk[2], chunk[1], chunk[0]]);
    }
    let img = image::RgbImage::from_raw(width, height, rgb)?;
    let max_width = max_width.clamp(80, 640) as u32;
    let resized = if width > max_width {
        let h = ((height as f64) * (max_width as f64 / width as f64))
            .round()
            .max(1.0) as u32;
        image::imageops::resize(&img, max_width, h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    use image::ImageEncoder;
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70);
    encoder
        .write_image(
            resized.as_raw(),
            resized.width(),
            resized.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    #[cfg(windows)]
    struct FixturePaintTrace {
        message: u32,
        flags: isize,
        client: (i32, i32),
        clip: (i32, i32, i32, i32),
        black: bool,
    }
    #[cfg(windows)]
    impl std::fmt::Debug for FixturePaintTrace {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.debug_struct("Paint")
                .field("message", &format_args!("0x{:x}", self.message))
                .field("flags", &self.flags)
                .field("client", &self.client)
                .field("clip", &self.clip)
                .field("black", &self.black)
                .finish()
        }
    }
    #[cfg(windows)]
    fn fixture_paint_trace() -> &'static parking_lot::Mutex<Vec<FixturePaintTrace>> {
        static TRACE: std::sync::OnceLock<parking_lot::Mutex<Vec<FixturePaintTrace>>> =
            std::sync::OnceLock::new();
        TRACE.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
    }

    #[cfg(windows)]
    thread_local! {
        static BITMAP_SELECTED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }

    #[cfg(windows)]
    thread_local! {
        static CAPTURE_FLAGS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
        static RENDER_TRACE: std::cell::RefCell<Vec<(u32, bool, [[u8; 3]; 4])>> = const { std::cell::RefCell::new(Vec::new()) };
        static SOURCE_GEOMETRY: std::cell::Cell<Option<([i32; 4], Option<[i32; 4]>, [u32; 2])>> = const { std::cell::Cell::new(None) };
    }
    #[cfg(windows)]
    pub(crate) fn observe_source_geometry(
        window: [i32; 4],
        monitor: Option<[i32; 4]>,
        flags: [u32; 2],
    ) {
        SOURCE_GEOMETRY.with(|source| source.set(Some((window, monitor, flags))));
    }
    #[cfg(windows)]
    pub(crate) fn observe_render(flags: u32, pixels: &[u8], width: u32, height: u32) {
        let quadrants = [(1, 1), (3, 1), (1, 3), (3, 3)].map(|(x, y)| {
            let offset = ((height * y / 4 * width + width * x / 4) * 4) as usize;
            [pixels[offset + 2], pixels[offset + 1], pixels[offset]]
        });
        RENDER_TRACE.with(|trace| {
            trace.borrow_mut().push((
                flags,
                super::is_mostly_black(pixels, width, height),
                quadrants,
            ))
        });
    }
    #[cfg(windows)]
    pub(super) fn take_render_trace() -> Vec<(u32, bool, [[u8; 3]; 4])> {
        RENDER_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
    }
    #[cfg(windows)]
    pub(super) fn append_render_trace(trace: Vec<(u32, bool, [[u8; 3]; 4])>) {
        RENDER_TRACE.with(|current| current.borrow_mut().extend(trace));
    }
    #[cfg(windows)]
    pub(crate) fn observe_capture_flags(flags: u32) {
        CAPTURE_FLAGS.with(|list| list.borrow_mut().push(flags));
    }
    #[cfg(windows)]
    fn take_capture_flags() -> Vec<u32> {
        CAPTURE_FLAGS.with(|list| std::mem::take(&mut *list.borrow_mut()))
    }

    #[cfg(windows)]
    pub(crate) unsafe fn observe_bitmap_selection(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    ) {
        use windows::Win32::Graphics::Gdi::{GetCurrentObject, OBJ_BITMAP};
        BITMAP_SELECTED
            .with(|value| value.set(Some(GetCurrentObject(hdc, OBJ_BITMAP).0 == bitmap.0)));
    }

    #[cfg(windows)]
    pub(crate) fn take_bitmap_selected() -> Option<bool> {
        BITMAP_SELECTED.with(|value| value.take())
    }
    #[cfg(windows)]
    pub(super) fn set_bitmap_observation(selected: Option<bool>) {
        BITMAP_SELECTED.with(|value| value.set(selected));
    }

    #[cfg(windows)]
    unsafe extern "system" fn thumbnail_test_wndproc(
        hwnd: windows::Win32::Foundation::HWND,
        message: u32,
        wp: windows::Win32::Foundation::WPARAM,
        lp: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::Foundation::{COLORREF, LRESULT, RECT};
        use windows::Win32::Graphics::Gdi::{
            BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HDC, PAINTSTRUCT,
        };
        use windows::Win32::UI::WindowsAndMessaging::*;
        if message == WM_ERASEBKGND {
            return LRESULT(1);
        }
        if message == WM_PRINT || message == WM_PRINTCLIENT || message == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let target = if message == WM_PAINT {
                BeginPaint(hwnd, &mut paint)
            } else {
                HDC(wp.0 as *mut _)
            };
            let black = GetWindowLongPtrW(hwnd, GWLP_USERDATA) == 1;
            if message == WM_PRINT && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == 2 {
                std::thread::sleep(Duration::from_millis(250));
            }
            let mut bounds = RECT::default();
            let _ = GetClientRect(hwnd, &mut bounds);
            let mut clip = RECT::default();
            windows::Win32::Graphics::Gdi::GetClipBox(target, &mut clip);
            {
                let mut trace = fixture_paint_trace().lock();
                if trace.len() == 32 {
                    trace.remove(0);
                }
                trace.push(FixturePaintTrace {
                    message,
                    flags: lp.0,
                    client: (bounds.right, bounds.bottom),
                    clip: (clip.left, clip.top, clip.right, clip.bottom),
                    black,
                });
            }
            if black {
                let _ = windows::Win32::Graphics::Gdi::PatBlt(
                    target,
                    0,
                    0,
                    bounds.right,
                    bounds.bottom,
                    windows::Win32::Graphics::Gdi::BLACKNESS,
                );
                if message == WM_PAINT {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(1);
            }
            let x = bounds.right / 2;
            let y = bounds.bottom / 2;
            for (r, color) in [
                (
                    RECT {
                        left: 0,
                        top: 0,
                        right: x,
                        bottom: y,
                    },
                    COLORREF(0x00281edc),
                ),
                (
                    RECT {
                        left: x,
                        top: 0,
                        right: bounds.right,
                        bottom: y,
                    },
                    COLORREF(0x003cd21e),
                ),
                (
                    RECT {
                        left: 0,
                        top: y,
                        right: x,
                        bottom: bounds.bottom,
                    },
                    COLORREF(0x00dc4123),
                ),
                (
                    RECT {
                        left: x,
                        top: y,
                        right: bounds.right,
                        bottom: bounds.bottom,
                    },
                    COLORREF(0x0019c8e1),
                ),
            ] {
                let brush = CreateSolidBrush(color);
                FillRect(target, &r, brush);
                let _ = DeleteObject(brush);
            }
            if message == WM_PAINT {
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(1);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }

    #[cfg(windows)]
    #[test]
    fn native_large_window_thumbnail_contains_the_whole_landscape_and_portrait() {
        use windows::core::w;
        use windows::Win32::Foundation::{BOOL, HWND, RECT};
        use windows::Win32::Graphics::Dwm::{DwmFlush, DwmSetWindowAttribute, DWMWA_CLOAK};
        use windows::Win32::Graphics::Gdi::{InvalidateRect, UpdateWindow};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::GetCurrentProcess;
        use windows::Win32::System::Threading::{GetGuiResources, GR_GDIOBJECTS};
        use windows::Win32::UI::HiDpi::*;
        use windows::Win32::UI::WindowsAndMessaging::*;
        struct OwnedWindow(HWND, Option<std::thread::JoinHandle<()>>);
        impl Drop for OwnedWindow {
            fn drop(&mut self) {
                super::cache().lock().remove(&(self.0 .0 as isize));
                unsafe {
                    let _ = PostMessageW(
                        self.0,
                        WM_CLOSE,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
                if let Some(thread) = self.1.take() {
                    thread.join().unwrap();
                }
            }
        }
        struct DpiContext(DPI_AWARENESS_CONTEXT);
        impl Drop for DpiContext {
            fn drop(&mut self) {
                unsafe {
                    SetThreadDpiAwarenessContext(self.0);
                }
            }
        }
        unsafe {
            let _dpi = DpiContext(SetThreadDpiAwarenessContext(
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            ));
            let class = WNDCLASSW {
                lpfnWndProc: Some(thumbnail_test_wndproc),
                hInstance: GetModuleHandleW(None).unwrap().into(),
                lpszClassName: w!("ClipHostThumbnailGeometryRegression"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let before = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            let mut warmed = before;
            {
                // Match real HTTP capture's cross-thread source message pump.
                // PrintWindow may reuse cached surfaces; direct WM_PRINT must
                // render to the supplied full-size worker-owned memory DC.
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                let thread = std::thread::spawn(move || {
                    let _dpi = DpiContext(SetThreadDpiAwarenessContext(
                        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                    ));
                    let hwnd = CreateWindowExW(
                        WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                        w!("ClipHostThumbnailGeometryRegression"),
                        w!("Owned thumbnail geometry probe"),
                        WS_POPUP,
                        40,
                        40,
                        640,
                        360,
                        None,
                        None,
                        GetModuleHandleW(None).unwrap(),
                        None,
                    )
                    .unwrap();
                    DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &BOOL(1) as *const _ as *const _, 4)
                        .unwrap();
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    tx.send(hwnd.0 as isize).unwrap();
                    let mut message = MSG::default();
                    while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                });
                let window = OwnedWindow(
                    HWND(rx.recv_timeout(Duration::from_secs(3)).unwrap() as *mut _),
                    Some(thread),
                );
                for (width, height) in [(640, 360), (3840, 2160), (2160, 3840)] {
                    fixture_paint_trace().lock().clear();
                    SetWindowPos(window.0, None, 40, 40, width, height, SWP_NOACTIVATE).unwrap();
                    let _ = InvalidateRect(window.0, None, false);
                    let _ = UpdateWindow(window.0);
                    DwmFlush().unwrap();
                    let mut bounds = RECT::default();
                    GetWindowRect(window.0, &mut bounds).unwrap();
                    assert_eq!(
                        (bounds.right - bounds.left, bounds.bottom - bounds.top),
                        (width, height)
                    );
                    // The compositor may expose the previous redirection
                    // surface during a resize. Wait for a complete source
                    // repaint; every sampled quadrant remains mandatory.
                    // The old 2400px canvas can never satisfy this condition.
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    let expected_height =
                        ((height as f64 * 420.0 / width as f64).round().max(1.0)) as u32;
                    let mut captures = 0;
                    let mut last_pixels = Vec::new();
                    let mut standard_probe = false;
                    let (image, attempts) = loop {
                        captures += 1;
                        take_capture_flags();
                        take_render_trace();
                        let image = super::try_capture_jpeg(window.0 .0 as isize, 420)
                            .map(|jpeg| image::load_from_memory(&jpeg).unwrap().into_rgb8());
                        let attempts = take_capture_flags();
                        if captures == 1 {
                            println!("initial {width}x{height}: geometry={:?}, attempts={attempts:?}, renders={:?}",SOURCE_GEOMETRY.with(|source|source.get()), take_render_trace());
                        }
                        assert!(
                            attempts == [2]
                                || attempts == [2, 0]
                                || attempts == [0]
                                || attempts == [0, 2]
                                || attempts == [WM_PRINT]
                                || attempts == [WM_PRINT, 0]
                                || attempts == [WM_PRINT, 0, 2],
                            "unexpected native attempts: {attempts:?}"
                        );
                        if let Some(image) = image {
                            assert_eq!(take_bitmap_selected(), Some(false));
                            assert_eq!(image.dimensions(), (420, expected_height));
                            last_pixels.clear();
                            let mut complete = true;
                            for (xpart, ypart, expected) in [
                                (1, 1, [220u8, 30, 40]),
                                (3, 1, [30, 210, 60]),
                                (1, 3, [35, 65, 220]),
                                (3, 3, [225, 200, 25]),
                            ] {
                                let actual = image
                                    .get_pixel(
                                        image.width() * xpart / 4,
                                        image.height() * ypart / 4,
                                    )
                                    .0;
                                last_pixels.push(actual);
                                complete &= (0..3).all(|i| actual[i].abs_diff(expected[i]) <= 8);
                            }
                            if complete {
                                break (image, attempts);
                            }
                        }
                        if !standard_probe {
                            standard_probe = true;
                            let standard = super::try_capture_jpeg_using_flags(
                                window.0 .0 as isize,
                                420,
                                Some(&[0]),
                            )
                            .map(|jpeg| image::load_from_memory(&jpeg).unwrap().into_rgb8());
                            let quadrants = standard.as_ref().map(|image| {
                                [(1, 1), (3, 1), (1, 3), (3, 3)].map(|(x, y)| {
                                    image
                                        .get_pixel(image.width() * x / 4, image.height() * y / 4)
                                        .0
                                })
                            });
                            println!("incomplete full-content {width}x{height}: flags0 quadrants={quadrants:?}, paint trace={:?}",fixture_paint_trace().lock());
                        }
                        assert!(std::time::Instant::now()<deadline,
                            "incomplete {width}x{height} native preview: last quadrant pixels={last_pixels:?}; trace={:?}",fixture_paint_trace().lock());
                        std::thread::sleep(Duration::from_millis(10));
                    };
                    println!("native whole-window preview: {width}x{height} -> {}x{}; all four quadrants matched; attempts={attempts:?}; captures={captures}",image.width(),image.height());
                    let resources = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
                    if (width, height) == (640, 360) {
                        warmed = resources;
                    } else {
                        assert_eq!(
                            resources, warmed,
                            "capture GDI resources must stay stable after initial system warmup"
                        );
                    }
                    println!("GDI initial={before}, warmed={warmed}, after frame={resources}");
                    assert_ne!(GetForegroundWindow(), window.0);
                }
                let cached = super::try_capture_jpeg(window.0 .0 as isize, 420).unwrap();
                super::put(window.0 .0 as isize, cached.clone(), 420);
                // A slow source owns its DC even after our caller's deadline.
                // It must neither block the caller nor draw into a freed DC.
                SetWindowLongPtrW(window.0, GWLP_USERDATA, 2);
                let start = std::time::Instant::now();
                let delayed = super::capture_owner_with_deadline(
                    window.0 .0 as isize,
                    2160,
                    3840,
                    super::acquire_capture().unwrap(),
                    Duration::from_millis(20),
                );
                assert!(
                    delayed.is_none(),
                    "a late owner frame must not become live output"
                );
                assert!(start.elapsed() < Duration::from_millis(200));
                assert!(
                    super::capture_slots().load(std::sync::atomic::Ordering::Acquire)
                        < super::CAPTURE_SLOTS,
                    "a timed-out worker must keep its capture lease until the owner returns"
                );
                let cleanup = std::time::Instant::now() + Duration::from_secs(3);
                while super::capture_slots().load(std::sync::atomic::Ordering::Acquire)
                    != super::CAPTURE_SLOTS
                {
                    assert!(
                        std::time::Instant::now() < cleanup,
                        "owned slow renderer must finish and release its DC and slot"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), warmed);
                assert_eq!(
                    super::serve_cache(window.0 .0 as isize),
                    Some(cached.clone())
                );
                println!("owned delayed WM_PRINT: caller deadline returned before 200ms; original cache unchanged; slot/GDI released after renderer completed");
                SetWindowLongPtrW(window.0, GWLP_USERDATA, 1);
                fixture_paint_trace().lock().clear();
                let _ = InvalidateRect(window.0, None, false);
                let _ = UpdateWindow(window.0);
                DwmFlush().unwrap();
                for mode in [0, 2] {
                    take_render_trace();
                    let old_mode = super::try_capture_jpeg_using_flags(
                        window.0 .0 as isize,
                        420,
                        Some(&[mode]),
                    );
                    println!(
                        "black-source legacy PrintWindow flags={mode}: live={} renders={:?}",
                        old_mode.is_some(),
                        take_render_trace()
                    );
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                let mut black_attempt = 0;
                loop {
                    take_capture_flags();
                    take_render_trace();
                    let frame = super::try_capture_jpeg(window.0 .0 as isize, 420);
                    let flags = take_capture_flags();
                    let trace = take_render_trace();
                    black_attempt += 1;
                    if black_attempt == 1 {
                        println!("black source frame: live={} flags={flags:?} renders={trace:?}; paint trace={:?}",frame.is_some(), fixture_paint_trace().lock());
                    }
                    if frame.is_none() {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "black source frame must use existing cache fallback; flags={flags:?} renders={trace:?}; paint trace={:?}",fixture_paint_trace().lock()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(super::serve_cache(window.0 .0 as isize), Some(cached));
                assert!(
                    fixture_paint_trace()
                        .lock()
                        .iter()
                        .any(|paint| paint.message == WM_PRINT
                            && paint.black
                            && paint.clip == (0, 0, 2160, 3840)),
                    "the black frame must be confirmed by an actual full-geometry owner paint"
                );
                let cleanup = std::time::Instant::now() + Duration::from_secs(3);
                while super::capture_slots().load(std::sync::atomic::Ordering::Acquire)
                    != super::CAPTURE_SLOTS
                {
                    assert!(
                        std::time::Instant::now() < cleanup,
                        "owned black renderer must release its leased DC after caller fallback"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), warmed);
            }
            assert_eq!(
                GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS),
                warmed,
                "GDI resources must stay stable through black fallback and window destruction"
            );
        }
    }

    #[test]
    fn jpeg_encoding_accepts_opaque_bgra_with_correct_colors_and_scaling() {
        let mut pixels = Vec::new();
        for y in 0..100 {
            for x in 0..160 {
                let color = match (x >= 80, y >= 50) {
                    (false, false) => [40u8, 30, 220, 0],
                    (true, false) => [60, 210, 30, 0],
                    (false, true) => [220, 65, 35, 0],
                    (true, true) => [25, 200, 225, 0],
                };
                pixels.extend_from_slice(&color);
            }
        }
        let jpeg = super::encode_jpeg(&pixels, 160, 100, 80)
            .expect("opaque BGRA window image must encode as JPEG");
        let image = image::load_from_memory(&jpeg).unwrap().into_rgb8();
        assert_eq!(image.dimensions(), (80, 50));
        for (x, y, expected) in [
            (20, 12, [220u8, 30, 40]),
            (60, 12, [30, 210, 60]),
            (20, 37, [35, 65, 220]),
            (60, 37, [225, 200, 25]),
        ] {
            let actual = image.get_pixel(x, y).0;
            for i in 0..3 {
                assert!(
                    actual[i].abs_diff(expected[i]) <= 8,
                    "{actual:?} vs {expected:?}"
                );
            }
        }
    }

    #[test]
    fn full_capture_dimensions_are_bounded_before_allocating() {
        assert_eq!(super::capture_dimensions(3840, 2160), Some((3840, 2160)));
        assert_eq!(super::capture_dimensions(2160, 3840), Some((2160, 3840)));
        assert_eq!(super::capture_dimensions(4096, 2048), Some((4096, 2048)));
        assert_eq!(super::capture_dimensions(4096, 2049), None);
        for (width, height) in [
            (7, 360),
            (640, 7),
            (0, 0),
            (-8, 64),
            (16385, 8),
            (7680, 4320),
            (i64::MAX, i64::MAX),
        ] {
            assert_eq!(super::capture_dimensions(width, height), None);
        }
    }

    #[test]
    fn native_print_mode_repaints_windows_beyond_the_monitor_bounds() {
        let monitor = Some([0, 0, 3840, 1440]);
        assert_eq!(
            super::capture_print_flags([40, 40, 680, 400], monitor),
            [2, 0]
        );
        assert_eq!(
            super::capture_print_flags([40, 40, 3880, 2200], monitor),
            [0, 2]
        );
        assert_eq!(
            super::capture_print_flags([40, 40, 2200, 3880], monitor),
            [0, 2]
        );
        assert_eq!(
            super::capture_print_flags([-20, 40, 620, 400], monitor),
            [0, 2]
        );
        assert_eq!(super::capture_print_flags([40, 40, 680, 400], None), [0, 2]);
        assert_eq!(
            super::capture_print_flags([-2400, -1600, -480, -520], Some([-2560, -1800, 0, 0])),
            [2, 0]
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_fresh_black_owner_does_not_accept_old_compositor_color() {
        let fresh_black = vec![0; 64 * 64 * 4];
        let owner = super::owner_frame(Some(fresh_black), 64, 64);
        assert!(super::choose_capture_pixels(owner, &[0, 2], 64, 64, |_| {
            panic!("cached color must not replace a confirmed fresh black frame")
        })
        .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn native_fresh_half_black_owner_keeps_the_existing_dark_threshold() {
        let mut pixels = vec![0; 64 * 64 * 4];
        for row in 0..64 {
            for column in 32..64 {
                let offset = (row * 64 + column) * 4;
                pixels[offset..offset + 3].copy_from_slice(&[40, 30, 220]);
            }
        }
        assert!(!super::is_mostly_black(&pixels, 64, 64));
        let owner = super::owner_frame(Some(pixels.clone()), 64, 64);
        assert_eq!(
            super::choose_capture_pixels(owner, &[0, 2], 64, 64, |_| {
                panic!("fresh half-black content must be preserved")
            }),
            Some(pixels)
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_unavailable_owner_retains_existing_printwindow_fallback() {
        let black = vec![0; 8 * 8 * 4];
        let color = [40, 30, 220, 0].repeat(8 * 8);
        let mut calls = Vec::new();
        let result =
            super::choose_capture_pixels(super::OwnerFrame::Unavailable, &[2, 0], 8, 8, |mode| {
                calls.push(mode);
                Some(if mode == 2 {
                    black.clone()
                } else {
                    color.clone()
                })
            });
        assert_eq!(calls, [2, 0]);
        assert_eq!(result, Some(color));
        assert!(super::uniformly_rgb(&black, 0));
        assert!(!super::uniformly_rgb(&[0, 0, 0, 0, 255, 255, 255, 0], 0));
    }

    #[test]
    fn zero_hwnd_is_empty() {
        crate::clipboard::force_headless(true);
        assert!(super::try_get_cached_or_capture(0, 360).is_none());
        assert!(!super::try_warm_capture(1, Duration::from_secs(20)));
        super::suggest(1);
        super::suggest_visible_windows();
        super::ensure_warmup();
    }
}
