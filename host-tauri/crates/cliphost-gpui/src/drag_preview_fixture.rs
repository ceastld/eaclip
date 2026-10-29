//! Isolated check of the drag preview popup: it composes on screen with its
//! layered click-through style, WindowFromPoint resolves the window beneath
//! it, and the foreground window never changes. No host, input or OLE.
use super::{open, INSET, WIDTH};
use crate::{
    favorite_drag::ContentDrag, platform, super_menu::style_fixture::output_directory,
    DesignAssets,
};
use anyhow::{anyhow, ensure, Context as _};
use gpui::{Application, Timer};
use serde_json::{json, Value};
use std::{fs, path::{Path, PathBuf}, sync::{Arc, Mutex}, time::Duration};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::*,
};

const SCENARIOS: [&str; 4] = ["short", "long", "edge", "snapshot"];

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut scenario) = (None, "short".to_owned());
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "--help" | "-h") {
            println!("drag_preview_visual --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--scenario short|long|edge|snapshot]");
            return Ok(());
        }
        let value = args.next().with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--scenario" => {
                ensure!(SCENARIOS.contains(&value.as_str()), "Invalid scenario");
                scenario = value;
            }
            _ => return Err(anyhow!("Unknown argument: {key}")),
        }
    }
    let run = output_directory(&output.context("--output is required")?)?;
    let report_path = run.join("drag-preview-report.json");
    let screenshot = run.join("drag-preview.png");
    if scenario == "snapshot" {
        return snapshot_scene::run(report_path, screenshot);
    }
    let title = match scenario.as_str() {
        "long" => "很长的剪贴板条目标题，用来确认拖动预览在固定宽度内截断并显示省略号，而不是撑出窗口或被裁成半个圆角".to_owned(),
        _ => "会议纪要 2026-10-26".to_owned(),
    };
    // The primary monitor contains (0, 0); "edge" puts the hotspot in its
    // bottom-right corner so both flips are exercised.
    let area = cliphost_core::window_place::overlay_work_area_from_point(0, 0)
        .context("Primary work area unavailable")?;
    let hotspot = if scenario == "edge" {
        ((area.work.left + area.work.width) as i32 - 20, (area.work.top + area.work.height) as i32 - 10)
    } else {
        ((area.work.left + area.work.width / 2.0) as i32, (area.work.top + area.work.height / 2.0) as i32)
    };
    let report = Arc::new(Mutex::new(json!({"complete":false,"passed":false,"scenario":scenario})));
    let completed = report.clone();
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let foreground = unsafe { GetForegroundWindow() }.0 as isize;
        let handle = open(cx).expect("Open drag preview fixture");
        let drag = ContentDrag::history(
            vec!["a".into(), "b".into(), "c".into()],
            "fixture".into(),
            1,
            1,
            title,
        );
        let _ = handle.update(cx, |this, window, cx| {
            this.drag = Some(drag);
            this.external = true;
            this.follow_at(hotspot, window, cx);
            cx.notify();
        });
        let report_path = report_path.clone();
        let screenshot = screenshot.clone();
        cx.spawn(async move |cx| {
            // Re-place once the label has painted and measured itself.
            Timer::after(Duration::from_millis(300)).await;
            let _ = handle.update(cx, |this, window, cx| {
                this.origin = None;
                this.follow_at(hotspot, window, cx);
            });
            Timer::after(Duration::from_millis(500)).await;
            let state = handle.update(cx, |this, window, _| {
                let scale = f64::from(window.scale_factor());
                platform::hwnd(window).map(|hwnd| (hwnd.0 as isize, this.measured.get(), this.flip, scale))
            });
            let outcome = (|| -> anyhow::Result<Value> {
                let (hwnd, measured, flip, scale) = state
                    .map_err(|error| anyhow!(error.to_string()))?
                    .map_err(|error| anyhow!(error))?;
                inspect(hwnd, foreground, hotspot, measured, flip, scale, &screenshot)
            })();
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            match outcome {
                Ok(detail) => {
                    value["passed"] = json!(true);
                    value["detail"] = detail;
                }
                Err(error) => value["error"] = json!(format!("{error:?}")),
            }
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });
    let report = completed.lock().unwrap();
    println!("{}", serde_json::to_string_pretty(&*report)?);
    ensure!(report["passed"] == true, "Drag preview fixture failed");
    Ok(())
}

fn inspect(
    hwnd: isize,
    foreground: isize,
    hotspot: (i32, i32),
    measured: (f32, f32),
    flip: (bool, bool),
    scale: f64,
    screenshot: &Path,
) -> anyhow::Result<Value> {
    let window = HWND(hwnd as *mut _);
    unsafe {
        ensure!(IsWindowVisible(window).as_bool(), "Popup is not visible");
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        for (name, bit) in [
            ("WS_EX_LAYERED", WS_EX_LAYERED),
            ("WS_EX_TRANSPARENT", WS_EX_TRANSPARENT),
            ("WS_EX_NOACTIVATE", WS_EX_NOACTIVATE),
            ("WS_EX_TOOLWINDOW", WS_EX_TOOLWINDOW),
            ("WS_EX_TOPMOST", WS_EX_TOPMOST),
        ] {
            ensure!(style & bit.0 as isize != 0, "Missing {name}");
        }
        ensure!(
            GetForegroundWindow().0 as isize == foreground,
            "Showing the popup changed the foreground window"
        );
        let mut rect = RECT::default();
        GetWindowRect(window, &mut rect)?;
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        ensure!(measured.0 > 0.0 && measured.1 > 0.0, "Label never painted");
        ensure!(
            measured.0 <= WIDTH - 2.0 * INSET + 0.5,
            "Label exceeds the popup width: {measured:?}"
        );
        let at = |logical: f32| (f64::from(logical) * scale).round() as i32;
        // A padding pixel just inside the label's leading edge, mid-height.
        let label_left = if flip.0 { width - at(INSET) - at(measured.0) } else { at(INSET) };
        let label_top = if flip.1 { height - at(INSET) - at(measured.1) } else { at(INSET) };
        let sample = (label_left + at(4.0), label_top + at(measured.1) / 2);
        let screen = POINT { x: rect.left + sample.0, y: rect.top + sample.1 };
        let beneath = GetAncestor(WindowFromPoint(screen), GA_ROOT);
        ensure!(beneath != window, "WindowFromPoint hit the popup; it is not click-through");
        // Label must not cover the hotspot itself.
        ensure!(
            !(hotspot.0 >= rect.left + label_left
                && hotspot.0 < rect.left + label_left + at(measured.0)
                && hotspot.1 >= rect.top + label_top
                && hotspot.1 < rect.top + label_top + at(measured.1)),
            "Label covers the pointer hotspot"
        );
        let pixels = capture_screen(rect)?;
        let rgb = |(x, y): (i32, i32)| {
            let index = (y * width + x) as usize * 4;
            [pixels[index + 2], pixels[index + 1], pixels[index]]
        };
        let pill = |color: [u8; 3]| color.iter().all(|channel| channel.abs_diff(0x35) <= 8);
        let [r, g, b] = rgb(sample);
        ensure!(pill([r, g, b]), "Label background not composed on screen: rgb({r},{g},{b})");
        // Trailing padding stays empty: long text ends in an ellipsis rather
        // than running into (and being clipped at) the pill's right edge.
        let trailing = rgb((label_left + at(measured.0) - at(4.0), sample.1));
        ensure!(pill(trailing), "Label text reaches its trailing edge: rgb{trailing:?}");
        let mut rgba = pixels;
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        image::save_buffer(screenshot, &rgba, width as u32, height as u32, image::ColorType::Rgba8)?;
        Ok(json!({
            "rect": [rect.left, rect.top, width, height],
            "hotspot": [hotspot.0, hotspot.1],
            "label": [measured.0, measured.1],
            "flip": [flip.0, flip.1],
            "scale": scale,
            "sample": {"point": [screen.x, screen.y], "rgb": [r, g, b]},
            "windowBeneath": format!("{:#x}", beneath.0 as isize),
            "foregroundUnchanged": true,
            "method": "BitBlt(screen, SRCCOPY|CAPTUREBLT)",
            "screenshot": screenshot.display().to_string(),
        }))
    }
}

/// What is actually composed on screen, layered windows included.
unsafe fn capture_screen(rect: RECT) -> anyhow::Result<Vec<u8>> {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    ensure!((1..=4096).contains(&width) && (1..=4096).contains(&height), "Invalid capture size");
    let screen = GetDC(None);
    ensure!(!screen.is_invalid(), "GetDC failed");
    let dc = CreateCompatibleDC(Some(screen));
    let bitmap = CreateCompatibleBitmap(screen, width, height);
    let previous = SelectObject(dc, bitmap.into());
    let copied = BitBlt(dc, 0, 0, width, height, Some(screen), rect.left, rect.top, SRCCOPY | CAPTUREBLT).is_ok();
    SelectObject(dc, previous);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let rows = GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
    ensure!(copied && rows == height, "Screen capture failed");
    Ok(pixels)
}

/// The real capture path on a sample row: a hover-free frame, PrintWindow,
/// worker shaping, then the popup over a dark window so its translucency can
/// be measured on screen.
mod snapshot_scene {
    use super::super::{snapshot, Mode, PAD};
    use super::{capture_screen, open};
    use crate::{favorite_drag::ContentDrag, platform, DesignAssets};
    use anyhow::{anyhow, ensure};
    use gpui::{
        div, point, prelude::*, px, rgb, size, Application, Bounds, Context, Render, Timer, Window,
        WindowBounds, WindowOptions,
    };
    use serde_json::{json, Value};
    use std::{fs, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
    use windows::Win32::{
        Foundation::{HWND, POINT, RECT},
        UI::WindowsAndMessaging::*,
    };

    const BACKDROP: u32 = 0x2b2f36;
    const ROW: (f32, f32, f32, f32) = (24.0, 40.0, 388.0, 120.0);
    const GRAB: (f32, f32) = (60.0, 20.0);

    struct SampleRow;
    impl Render for SampleRow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().bg(rgb(BACKDROP)).font_family("Microsoft YaHei UI").child(
                div()
                    .absolute()
                    .left(px(ROW.0))
                    .top(px(ROW.1))
                    .w(px(ROW.2))
                    .h(px(ROW.3))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(0xd9dce1))
                    .bg(rgb(0xffffff))
                    .p(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().text_size(px(11.0)).text_color(rgb(0x8a8f98)).child("4  3 分钟前"))
                    .child(
                        div()
                            .flex()
                            .gap(px(10.0))
                            .child(div().w(px(120.0)).h(px(68.0)).rounded(px(4.0)).bg(rgb(0x3b82f6)))
                            .child(div().text_size(px(13.0)).text_color(rgb(0x1f2328)).child("设计对齐缩略图 · 320 × 180")),
                    ),
            )
        }
    }

    pub(super) fn run(report_path: PathBuf, screenshot: PathBuf) -> anyhow::Result<()> {
        let report = Arc::new(Mutex::new(json!({"complete":false,"passed":false,"scenario":"snapshot"})));
        let completed = report.clone();
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let foreground = unsafe { GetForegroundWindow() }.0 as isize;
            let bounds = Bounds::new(point(px(120.0), px(120.0)), size(px(900.0), px(420.0)));
            let source = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        focus: false,
                        show: false,
                        is_resizable: false,
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|_| SampleRow),
                )
                .expect("Open sample row window");
            let source_hwnd = source
                .update(cx, |_, window, _| {
                    let _ = platform::set_no_activate(window, true);
                    let _ = platform::show_no_activate(window);
                    platform::hwnd(window).map(|hwnd| hwnd.0 as isize)
                })
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            let popup = open(cx).expect("Open drag preview fixture");
            let report_path = report_path.clone();
            let screenshot = screenshot.clone();
            cx.spawn(async move |cx| {
                Timer::after(Duration::from_millis(500)).await;
                unsafe {
                    let _ = SetWindowPos(HWND(source_hwnd as *mut _), Some(HWND_TOPMOST), 0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
                let row = Bounds::new(point(px(ROW.0), px(ROW.1)), size(px(ROW.2), px(ROW.3)));
                let scheduled = source.update(cx, |_, window, _| snapshot::capture("fixture-op".into(), row, GRAB, window));
                let mut captured = None;
                for _ in 0..40 {
                    Timer::after(Duration::from_millis(50)).await;
                    if let Some(result) = snapshot::take("fixture-op") {
                        captured = Some(result);
                        break;
                    }
                }
                let hover_cleared = cx.update(|_| !crate::appearance::hover_suppressed()).unwrap_or(false);
                // Hotspot over the dark window, so the row composites over BACKDROP.
                let (hotspot, scale) = source
                    .update(cx, |_, window, _| {
                        let scale = window.scale_factor();
                        let mut point = POINT { x: (520.0 * scale) as i32, y: (160.0 * scale) as i32 };
                        unsafe {
                            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(HWND(source_hwnd as *mut _), &mut point);
                        }
                        ((point.x, point.y), scale)
                    })
                    .unwrap_or(((0, 0), 1.0));
                let shown = match captured {
                    Some(Ok(snapshot)) => {
                        let image = (snapshot.size, snapshot.grab, snapshot.surface);
                        let _ = popup.update(cx, |this, window, cx| {
                            this.drag = Some(ContentDrag::history(
                                vec!["a".into(), "b".into(), "c".into()], "fixture".into(), 1, 1, "row".into(),
                            ));
                            this.count = 3;
                            this.hint = Some("＋ 复制到 记事本".into());
                            this.snapshot = Some(snapshot);
                            this.mode = Mode::Snapshot;
                            this.follow_at(hotspot, window, cx);
                            cx.notify();
                        });
                        Ok(image)
                    }
                    Some(Err(error)) => Err(anyhow!("capture failed: {error}")),
                    None => Err(anyhow!("capture never arrived (scheduled: {scheduled:?})")),
                };
                Timer::after(Duration::from_millis(600)).await;
                let popup_hwnd = popup
                    .update(cx, |_, window, _| platform::hwnd(window).map(|hwnd| hwnd.0 as isize))
                    .ok()
                    .and_then(Result::ok);
                let outcome = (|| -> anyhow::Result<Value> {
                    let (size, grab, surface) = shown?;
                    ensure!(hover_cleared, "Hover suppression was left on after the capture");
                    ensure!((size.0 - (ROW.2 + 2.0 * PAD)).abs() < 1.0 && (size.1 - (ROW.3 + 2.0 * PAD)).abs() < 1.0,
                        "Snapshot size {size:?} does not match the row plus padding");
                    ensure!(grab == GRAB, "Grab offset changed: {grab:?}");
                    ensure!(surface == [255, 255, 255], "Row surface sampled as {surface:?}");
                    inspect(popup_hwnd.ok_or_else(|| anyhow!("popup has no HWND"))?, foreground, hotspot, scale, &screenshot)
                })();
                let mut value = report.lock().unwrap();
                value["complete"] = json!(true);
                match outcome {
                    Ok(detail) => {
                        value["passed"] = json!(true);
                        value["detail"] = detail;
                    }
                    Err(error) => value["error"] = json!(format!("{error:?}")),
                }
                let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
        let report = completed.lock().unwrap();
        println!("{}", serde_json::to_string_pretty(&*report)?);
        ensure!(report["passed"] == true, "Drag preview snapshot fixture failed");
        Ok(())
    }

    fn inspect(hwnd: isize, foreground: isize, hotspot: (i32, i32), scale: f32, screenshot: &std::path::Path) -> anyhow::Result<Value> {
        let window = HWND(hwnd as *mut _);
        unsafe {
            ensure!(IsWindowVisible(window).as_bool(), "Popup is not visible");
            // Only our own windows could have taken it; anything else is the
            // user switching windows during the run.
            let now = GetForegroundWindow();
            let mut pid = 0;
            GetWindowThreadProcessId(now, Some(&mut pid));
            ensure!(now.0 as isize == foreground || pid != std::process::id(),
                "A fixture window took the foreground");
            let beneath = GetAncestor(WindowFromPoint(POINT { x: hotspot.0, y: hotspot.1 }), GA_ROOT);
            ensure!(beneath != window, "WindowFromPoint hit the popup; it is not click-through");
            let mut rect = RECT::default();
            GetWindowRect(window, &mut rect)?;
            let width = rect.right - rect.left;
            let at = |logical: f32| (logical * scale).round() as i32;
            // The grab point stays under the pointer.
            ensure!(
                (rect.left + at(PAD + GRAB.0) - hotspot.0).abs() <= 1 && (rect.top + at(PAD + GRAB.1) - hotspot.1).abs() <= 1,
                "Snapshot is not held at the grab point"
            );
            let pixels = capture_screen(rect)?;
            // Saved before the checks, so a failure can be looked at.
            let mut rgba = pixels.clone();
            for pixel in rgba.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            image::save_buffer(screenshot, &rgba, width as u32, (rect.bottom - rect.top) as u32, image::ColorType::Rgba8)?;
            let rgb = |x: i32, y: i32| {
                let index = (y * width + x) as usize * 4;
                [pixels[index + 2], pixels[index + 1], pixels[index]]
            };
            // White row at 78% over the two white stack layers (3 selected)
            // over BACKDROP (43, 47, 54): about 245 per channel.
            let top = rgb(at(PAD + 300.0), at(PAD + 12.0));
            let expected = |backdrop: u8| {
                let over = |below: f32, alpha: f32| below + alpha * (255.0 - below);
                let layers = over(over(f32::from(backdrop), 0.55 * 0.78), 0.8 * 0.78);
                over(layers, 0.78).round() as u8
            };
            for (channel, backdrop) in top.iter().zip([0x2b, 0x2f, 0x36]) {
                ensure!(channel.abs_diff(expected(backdrop)) <= 12, "Row top is not 78% opaque: {top:?}");
            }
            // The faded bottom shows more of the dark window than the top.
            let bottom = rgb(at(PAD + 300.0), at(PAD + ROW.3 - 3.0));
            ensure!(u32::from(bottom[0]) + 30 < u32::from(top[0]), "Row bottom is not faded: {bottom:?} vs {top:?}");
            // Masked corner: the dark window shows through.
            let corner = rgb(at(PAD + 0.5), at(PAD + 0.5));
            ensure!(corner[0] < 120, "Row corner is not masked: {corner:?}");
            Ok(json!({
                "rect": [rect.left, rect.top, width, rect.bottom - rect.top],
                "hotspot": [hotspot.0, hotspot.1],
                "scale": scale,
                "top": top, "bottom": bottom, "corner": corner,
                "screenshot": screenshot.display().to_string(),
            }))
        }
    }
}
