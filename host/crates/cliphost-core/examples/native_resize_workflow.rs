//! Explicit screen-sampling acceptance probe for an isolated, already-running Rust host.
//! Usage: native_resize_workflow <host-pid> <artifact-directory> [capture-only]
//! Never finds a window by title alone; input is restricted to the supplied process.

#[cfg(not(windows))]
fn main() {
    eprintln!("This probe requires Windows.");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    native::run()
}

#[cfg(windows)]
mod native {
    use serde_json::json;
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    use windows::Win32::{
        Foundation::{BOOL, HANDLE, HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::*,
        UI::{
            HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::*,
        },
    };
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
    fn handle(raw: isize) -> HWND {
        HWND(raw as *mut _)
    }

    struct Target {
        pid: u32,
        matches: Vec<isize>,
    }
    unsafe extern "system" fn enumerate(hwnd: HWND, data: LPARAM) -> BOOL {
        let target = &mut *(data.0 as *mut Target);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut title = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut title);
        if pid == target.pid
            && String::from_utf16_lossy(&title[..len as usize]) == "ClipHost (Tauri)"
            && IsWindowVisible(hwnd).as_bool()
        {
            target.matches.push(hwnd.0 as isize);
        }
        BOOL(1)
    }
    unsafe fn client_rect(hwnd: HWND) -> Result<RECT> {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect)?;
        let mut origin = POINT::default();
        ClientToScreen(hwnd, &mut origin).ok()?;
        Ok(RECT {
            left: origin.x,
            top: origin.y,
            right: origin.x + rect.right,
            bottom: origin.y + rect.bottom,
        })
    }
    unsafe fn window_rect(hwnd: HWND) -> Result<RECT> {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect)?;
        Ok(rect)
    }
    unsafe fn ensure_target(hwnd: HWND, expected_pid: u32) -> Result<()> {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != expected_pid || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return Err("The test window is no longer available".into());
        }
        Ok(())
    }
    unsafe fn mouse(flags: MOUSE_EVENT_FLAGS) -> Result<()> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) != 1 {
            return Err("Mouse input was not delivered".into());
        }
        Ok(())
    }
    struct ButtonGuard;
    impl Drop for ButtonGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = mouse(MOUSEEVENTF_LEFTUP);
            }
        }
    }

    struct WindowRestore {
        hwnd: HWND,
        pid: u32,
        rect: RECT,
        topmost: bool,
        cursor: POINT,
        foreground: HWND,
        restore_cursor: bool,
    }
    impl Drop for WindowRestore {
        fn drop(&mut self) {
            unsafe {
                if ensure_target(self.hwnd, self.pid).is_ok() {
                    let _ = SetWindowPos(
                        self.hwnd,
                        if self.topmost {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        },
                        self.rect.left,
                        self.rect.top,
                        self.rect.right - self.rect.left,
                        self.rect.bottom - self.rect.top,
                        SWP_NOACTIVATE,
                    );
                    if GetForegroundWindow() == self.hwnd {
                        if self.restore_cursor {
                            let _ = SetCursorPos(self.cursor.x, self.cursor.y);
                        }
                        if IsWindow(self.foreground).as_bool() {
                            let _ = SetForegroundWindow(self.foreground);
                        }
                    }
                }
            }
        }
    }

    struct Capture {
        screen: HDC,
        dc: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u8,
        width: i32,
        height: i32,
    }
    impl Capture {
        unsafe fn new(width: i32, height: i32) -> Result<Self> {
            let screen = GetDC(HWND::default());
            let dc = CreateCompatibleDC(screen);
            if screen.0.is_null() || dc.0.is_null() {
                return Err("Cannot create screen capture DC".into());
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
            let bitmap =
                CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)?;
            let old = SelectObject(dc, bitmap);
            Ok(Self {
                screen,
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                width,
                height,
            })
        }
        unsafe fn pixels(&mut self, rect: RECT) -> Result<&[u8]> {
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            if width <= 0 || height <= 0 || width > self.width || height > self.height {
                return Err("Capture outside allocated surface".into());
            }
            BitBlt(
                self.dc,
                0,
                0,
                width,
                height,
                self.screen,
                rect.left,
                rect.top,
                SRCCOPY | CAPTUREBLT,
            )?;
            GdiFlush().ok()?;
            Ok(std::slice::from_raw_parts(
                self.bits,
                (self.width * height * 4) as usize,
            ))
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            unsafe {
                let _ = SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.bitmap);
                let _ = DeleteDC(self.dc);
                ReleaseDC(HWND::default(), self.screen);
            }
        }
    }

    fn scan(pixels: &[u8], stride: usize, width: usize, height: usize) -> (f64, usize, usize) {
        // Exclude the chrome, resize border and antialiased window corners.
        let mut white = 0usize;
        let mut total = 0usize;
        let mut longest = 0usize;
        let mut band = 0usize;
        let mut patch = 0usize;
        let mut longest_patch = 0usize;
        for y in 100..height.saturating_sub(10) {
            let mut row_white = 0usize;
            let mut run = 0usize;
            let mut longest_run = 0usize;
            for x in 12..width.saturating_sub(12) {
                let i = y * stride + x * 4;
                if pixels[i] >= 245 && pixels[i + 1] >= 245 && pixels[i + 2] >= 245 {
                    row_white += 1;
                    run += 1;
                    longest_run = longest_run.max(run);
                } else {
                    run = 0;
                }
            }
            let count = width.saturating_sub(24);
            white += row_white;
            total += count;
            if row_white * 4 >= count * 3 {
                band += 1;
                longest = longest.max(band);
            } else {
                band = 0;
            }
            if longest_run >= 64 {
                patch += 1;
                longest_patch = longest_patch.max(patch);
            } else {
                patch = 0;
            }
        }
        (white as f64 / total.max(1) as f64, longest, longest_patch)
    }
    fn save_frame(path: PathBuf, bytes: &[u8], width: u32, height: u32) -> Result<()> {
        let rgba: Vec<u8> = bytes
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect();
        image::save_buffer(path, &rgba, width, height, image::ColorType::Rgba8)?;
        Ok(())
    }

    unsafe fn drag(hwnd: HWND, pid: u32, top: bool, samples: usize) -> Result<usize> {
        ensure_target(hwnd, pid)?;
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
            return Err("Left mouse button is already down".into());
        }
        let rect = window_rect(hwnd)?;
        let x = (rect.left + rect.right) / 2;
        let y = if top { rect.top + 2 } else { rect.bottom - 2 };
        let start = POINT { x, y };
        SetCursorPos(x, y)?;
        if GetAncestor(WindowFromPoint(start), GA_ROOT) != hwnd {
            return Err("Another window covers the resize edge".into());
        }
        mouse(MOUSEEVENTF_LEFTDOWN)?;
        let button = ButtonGuard;
        std::thread::sleep(Duration::from_millis(70));
        let thread = GetWindowThreadProcessId(hwnd, None);
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(thread, &mut gui)?;
        if !gui.flags.contains(GUI_INMOVESIZE) || gui.hwndMoveSize != hwnd {
            return Err("The actual native edge drag did not start".into());
        }
        let mut previous = start;
        let mut recent = std::collections::VecDeque::from([(start, Instant::now())]);
        let mut late_cursor_samples = 0;
        for i in 1..=samples {
            ensure_target(hwnd, pid)?;
            if GetAncestor(GetForegroundWindow(), GA_ROOT) != hwnd {
                return Err("Focus left the test window; cancelling drag".into());
            }
            let mut current = POINT::default();
            GetCursorPos(&mut current)?;
            GetGUIThreadInfo(thread, &mut gui)?;
            if !gui.flags.contains(GUI_INMOVESIZE) || gui.hwndMoveSize != hwnd {
                return Err("The native drag was cancelled; stopping pointer input".into());
            }
            if current.x != previous.x || current.y != previous.y {
                // Live sizing can expose the previous queued move's cursor position.
                // Accept only our last three exact coordinates, within 50ms; arbitrary
                // displacement, focus loss, or Escape still cancels this fixture.
                if recent.iter().rev().take(3).any(|(point, time)| {
                    point.x == current.x
                        && point.y == current.y
                        && time.elapsed() <= Duration::from_millis(50)
                }) {
                    late_cursor_samples += 1;
                } else {
                    return Err(format!("Pointer moved outside the test; cancelling drag (expected {},{}; actual {},{}; step {i}; top={top})",previous.x,previous.y,current.x,current.y).into());
                }
            }
            let offset =
                ((i as f64 / samples as f64 * std::f64::consts::TAU * 2.0).sin() * 90.0) as i32;
            previous = POINT { x, y: y + offset };
            SetCursorPos(previous.x, previous.y)?;
            recent.push_back((previous, Instant::now()));
            if recent.len() > 3 {
                recent.pop_front();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(button);
        std::thread::sleep(Duration::from_millis(100));
        Ok(late_cursor_samples)
    }

    pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
        run_inner().map_err(|error| error as Box<dyn std::error::Error>)
    }
    fn run_inner() -> Result<()> {
        unsafe {
            let args: Vec<String> = std::env::args().collect();
            if args.len() < 3 {
                return Err("Expected host PID and artifact directory".into());
            }
            let pid: u32 = args[1].parse()?;
            let output = PathBuf::from(&args[2]);
            std::fs::create_dir_all(&output)?;
            let capture_only = args.get(3).is_some_and(|v| v == "capture-only");
            let _dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let mut target = Target {
                pid,
                matches: Vec::new(),
            };
            EnumWindows(Some(enumerate), LPARAM(&mut target as *mut _ as isize))?;
            if target.matches.len() != 1 {
                return Err(format!(
                    "Expected one visible Rust clipboard window for PID {pid}, found {}",
                    target.matches.len()
                )
                .into());
            }
            let raw = target.matches[0];
            let hwnd = handle(raw);
            ensure_target(hwnd, pid)?;
            let original = window_rect(hwnd)?;
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            )
            .ok()?;
            let area = monitor.rcWork;
            let width = original.right - original.left;
            let height = (area.bottom - area.top - 240).min(620);
            if height < 340 {
                return Err("Monitor work area is too small for the bounded drag".into());
            }
            let was_topmost = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0;
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor)?;
            let mut restore = WindowRestore {
                hwnd,
                pid,
                rect: original,
                topmost: was_topmost,
                cursor,
                foreground: GetForegroundWindow(),
                restore_cursor: false,
            };
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                area.left + 60,
                area.top + 120,
                width,
                height,
                SWP_SHOWWINDOW,
            )?;
            let _ = SetForegroundWindow(hwnd);
            std::thread::sleep(Duration::from_millis(500));
            let stop = Arc::new(AtomicBool::new(false));
            let phase = Arc::new(AtomicUsize::new(0));
            let sampling_stop = stop.clone();
            let sampling_phase = phase.clone();
            let capture_dir = output.clone();
            let max_width = area.right - area.left;
            let max_height = area.bottom - area.top;
            let sampler = std::thread::spawn(move || -> Result<serde_json::Value> {
                let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let mut capture = Capture::new(max_width, max_height)?;
                let mut frames = Vec::new();
                let mut kept = Vec::new();
                let timer = Instant::now();
                while !sampling_stop.load(Ordering::Relaxed) {
                    ensure_target(handle(raw), pid)?;
                    let rect = client_rect(handle(raw))?;
                    let w = (rect.right - rect.left) as usize;
                    let h = (rect.bottom - rect.top) as usize;
                    let stride = max_width as usize * 4;
                    let pixels = capture.pixels(rect)?;
                    let (ratio, band, patch) = scan(pixels, stride, w, h);
                    let current_phase = sampling_phase.load(Ordering::Relaxed);
                    let index = frames.len();
                    if index == 0 || (patch >= 3 && kept.len() < 9) {
                        let packed: Vec<u8> = (0..h)
                            .flat_map(|y| pixels[y * stride..y * stride + w * 4].iter().copied())
                            .collect();
                        kept.push((index, packed, w as u32, h as u32));
                    }
                    frames.push(json!({"frame":index,"ms":timer.elapsed().as_millis(),"phase":current_phase,"width":w,"height":h,"whiteRatio":ratio,"whiteBandRows":band,"whitePatchRows":patch}));
                    std::thread::sleep(Duration::from_millis(2));
                }
                let elapsed = timer.elapsed().as_secs_f64();
                for (index, pixels, w, h) in kept {
                    save_frame(
                        capture_dir.join(format!("frame-{index}.png")),
                        &pixels,
                        w,
                        h,
                    )?;
                }
                let white_frames = frames
                    .iter()
                    .filter(|v| v["whiteBandRows"].as_u64().unwrap_or(0) >= 3)
                    .count();
                let patch_frames = frames
                    .iter()
                    .filter(|v| v["whitePatchRows"].as_u64().unwrap_or(0) >= 3)
                    .count();
                let resize_frames = frames
                    .iter()
                    .filter(|v| v["phase"].as_u64().unwrap_or(0) > 0)
                    .count();
                let per_drag:Vec<_> = (1..=6).map(|phase| {
                let heights:Vec<_> = frames.iter().filter(|v| v["phase"] == phase).filter_map(|v| v["height"].as_u64()).collect();
                json!({"phase":phase,"frames":heights.len(),"minimumHeight":heights.iter().min(),"maximumHeight":heights.iter().max()})
            }).collect();
                let max_gap = frames
                    .windows(2)
                    .map(|pair| pair[1]["ms"].as_u64().unwrap() - pair[0]["ms"].as_u64().unwrap())
                    .max()
                    .unwrap_or(0);
                Ok(
                    json!({"frames":frames,"frameCount":frames.len(),"elapsedSeconds":elapsed,"averageFps":frames.len() as f64/elapsed,"maximumSampleGapMs":max_gap,"whiteBandFrames":white_frames,"whitePatchFrames":patch_frames,"resizeFrames":resize_frames,"perDrag":per_drag}),
                )
            });
            std::thread::sleep(Duration::from_millis(300));
            let mut drags_completed = 0;
            let mut late_cursor_samples = 0;
            let outcome = (|| -> Result<()> {
                if capture_only {
                    std::thread::sleep(Duration::from_secs(5));
                    return Ok(());
                }
                for pass in 1..=6 {
                    phase.store(pass, Ordering::Relaxed);
                    late_cursor_samples += drag(hwnd, pid, pass % 2 == 0, 120)?;
                    drags_completed += 1;
                }
                phase.store(0, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })();
            stop.store(true, Ordering::Relaxed);
            let sampled = sampler.join().map_err(|_| "Sampler panicked")?;
            restore.restore_cursor = outcome.is_ok() && !capture_only;
            let mut report = sampled?;
            report["hostPid"] = json!(pid);
            report["nativeDrags"] = json!(drags_completed);
            report["lateCursorSamples"] = json!(late_cursor_samples);
            report["captureOnly"] = json!(capture_only);
            let sampled_all_drags = report["perDrag"].as_array().unwrap().iter().all(|drag| {
                drag["frames"].as_u64().unwrap_or(0) >= 20
                    && drag["maximumHeight"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(drag["minimumHeight"].as_u64().unwrap_or(0))
                        >= 100
            });
            report["ok"] = json!(
                outcome.is_ok()
                    && (capture_only
                        || (report["whitePatchFrames"] == 0
                            && drags_completed == 6
                            && sampled_all_drags))
            );
            if let Err(error) = outcome {
                report["error"] = json!(error.to_string());
            }
            std::fs::write(
                output.join("report.json"),
                serde_json::to_string_pretty(&report)?,
            )?;
            println!(
                "{}",
                json!({"ok":report["ok"],"frameCount":report["frameCount"],"averageFps":report["averageFps"],"whiteBandFrames":report["whiteBandFrames"],"whitePatchFrames":report["whitePatchFrames"],"error":report["error"],"artifacts":output})
            );
            if report["ok"] != true {
                return Err("Native resize validation failed; see report.json".into());
            }
            Ok(())
        }
    }
}
