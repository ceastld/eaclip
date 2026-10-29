//! Debug-only category-row filter popovers (custom categories, process filters) over fixed rows.
//! Disposable headless settings; no NativeHost, API listener, hooks or OS input injection.
//! `--wheel` dispatches in-process wheel notches over the popover body and records the offsets.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("filter_editors_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    fixture::run()
}

#[cfg(all(windows, debug_assertions))]
mod fixture {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_core::{
        extra_settings::GlobalAppConfig,
        i18n::{set_language, LanguageMode},
        AppPaths, HostState,
    };
    use cliphost_gpui::{
        platform,
        preview::{DesignTokens, PreviewModel, FIXTURE_JSON, TOKENS_JSON},
        view::{ClipboardPreview, UiModel},
        DesignAssets,
    };
    use gpui::{
        point, prelude::*, px, size, Application, Bounds, Modifiers, PlatformInput, Timer,
        WindowBounds, WindowOptions,
    };
    use serde_json::{json, Value};

    #[derive(Clone, Copy, PartialEq)]
    enum Editor {
        Categories,
        ProcessAdd,
        ProcessEdit,
    }

    struct Options {
        dark: bool,
        english: bool,
        width: f32,
        height: f32,
        editor: Editor,
        restore: bool,
        wheel: bool,
        report: Option<PathBuf>,
        exit_ms: u64,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut options = Self {
                dark: true,
                english: false,
                width: 380.0,
                height: 600.0,
                editor: Editor::Categories,
                restore: false,
                wheel: false,
                report: None,
                exit_ms: 9_000,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--theme" => {
                        options.dark = match args.next().as_deref() {
                            Some("dark") => true,
                            Some("light") => false,
                            _ => bail!("--theme requires dark or light"),
                        }
                    }
                    "--lang" => {
                        options.english = match args.next().as_deref() {
                            Some("zh") => false,
                            Some("en") => true,
                            _ => bail!("--lang requires zh or en"),
                        }
                    }
                    "--width" | "--height" => {
                        let value: f32 = args.next().context("size requires a value")?.parse()?;
                        if !value.is_finite() || !(250.0..=1600.0).contains(&value) {
                            bail!("size must be 250..1600 DIP");
                        }
                        if arg == "--width" { options.width = value } else { options.height = value }
                    }
                    "--editor" => {
                        options.editor = match args.next().as_deref() {
                            Some("categories") => Editor::Categories,
                            Some("process-add") => Editor::ProcessAdd,
                            Some("process-edit") => Editor::ProcessEdit,
                            _ => bail!("--editor requires categories, process-add or process-edit"),
                        }
                    }
                    "--restore" => options.restore = true,
                    "--wheel" => options.wheel = true,
                    "--report" => options.report = Some(args.next().context("--report requires a path")?.into()),
                    "--exit-after-ms" => {
                        options.exit_ms = args.next().context("--exit-after-ms requires a value")?.parse()?;
                        if !(2_000..=60_000).contains(&options.exit_ms) {
                            bail!("--exit-after-ms must be 2000..60000");
                        }
                    }
                    "--help" | "-h" => {
                        println!("filter_editors_visual [--editor categories|process-add|process-edit] [--theme dark|light] [--lang zh|en] [--width 380] [--height 600] [--restore] [--wheel] [--report <repository .tmp/dir/report.json>] [--exit-after-ms 9000]");
                        println!("--restore presses the categories editor's Restore defaults. --wheel dispatches wheel notches (in-process, not OS input) over three columns of the popover body.");
                        return Ok(None);
                    }
                    _ => bail!("unknown argument: {arg}"),
                }
            }
            if options.restore && options.editor != Editor::Categories {
                bail!("--restore applies to --editor categories");
            }
            Ok(Some(options))
        }
    }

    fn output_paths(report: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
        let scratch = repo.join(".tmp");
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = scratch.join(format!("filter-editors-visual-{}-{nonce}", std::process::id()));
        fs::create_dir(&root)?;
        let report = match report {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => repo.join(path),
            None => root.join("report.json"),
        };
        let parent = report.parent().context("report needs a parent")?.canonicalize()
            .context("report parent must already exist under repository .tmp")?;
        if parent == scratch || !parent.starts_with(&scratch) {
            bail!("report must be inside a child directory of repository .tmp");
        }
        Ok((root, parent.join(report.file_name().context("report needs a file name")?)))
    }

    // Own-client PrintWindow capture, as in list_settings_visual.
    fn capture(hwnd_value: isize, path: &Path) -> Result<Value> {
        use windows::Win32::{
            Foundation::{HWND, RECT},
            Graphics::Gdi::*,
            UI::WindowsAndMessaging::{GetClientRect, GetWindowThreadProcessId},
        };
        #[link(name = "user32")]
        extern "system" {
            #[link_name = "PrintWindow"]
            fn print_window(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
        }
        let hwnd = HWND(hwnd_value as *mut std::ffi::c_void);
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != std::process::id() {
                bail!("capture requires this fixture's own HWND")
            }
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
                bail!("invalid capture size")
            }
            let source = GetDC(Some(hwnd));
            let dc = CreateCompatibleDC(Some(source));
            let bitmap = CreateCompatibleBitmap(source, width, height);
            let previous = SelectObject(dc, bitmap.into());
            let printed = print_window(hwnd, dc, 3) != 0;
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
            let rows = if printed {
                GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS)
            } else {
                0
            };
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(Some(hwnd), source);
            if !printed || rows != height {
                bail!("PrintWindow/GetDIBits failed (printed={printed}, rows={rows})")
            }
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            image::save_buffer(path, &pixels, width as u32, height as u32, image::ColorType::Rgba8)?;
            Ok(json!({"path":path,"width":width,"height":height}))
        }
    }

    /// Wheel probe: for each column, park the pointer, then send notches down
    /// until the offset stops changing, recording every settled offset.
    #[derive(Default)]
    struct WheelProbe {
        column: usize,
        notches: usize,
        settle: usize,
        last: Option<f64>,
        samples: Vec<Value>,
        done: bool,
    }

    impl WheelProbe {
        const COLUMNS: [(&'static str, f32); 3] = [("left", 0.12), ("name", 0.40), ("type", 0.78)];
        const MAX_NOTCHES: usize = 14;
    }

    fn scroll_of(main: &Value, editor: Editor) -> Value {
        let popover = &main["filterPopover"];
        match editor {
            Editor::Categories => popover["categories"]["scroll"].clone(),
            _ => popover["process"]["scroll"].clone(),
        }
    }

    pub fn run() -> Result<()> {
        let Some(options) = Options::parse()? else { return Ok(()) };
        let (root, report) = output_paths(options.report.as_deref())?;
        let paths = AppPaths::from_data_root(root.join("data"));
        fs::create_dir_all(&paths.configs)?;
        let config = GlobalAppConfig {
            ui_theme_mode: if options.dark { "dark" } else { "light" }.into(),
            ui_language: if options.english { "en-US" } else { "zh-CN" }.into(),
            launch_at_startup: false,
            window_background_frosted_glass: false,
            ..GlobalAppConfig::default()
        };
        fs::write(paths.configs.join("GlobalConfig.json"), serde_json::to_vec_pretty(&config)?)?;
        cliphost_core::force_headless(true);
        set_language(if options.english { LanguageMode::English } else { LanguageMode::Chinese });
        let host = Arc::new(HostState::open(paths.clone(), 29065).map_err(|error| anyhow!(error))?);
        let mut tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        tokens.window.width = options.width;
        tokens.window.height = options.height;
        let model = PreviewModel::from_json(FIXTURE_JSON).map_err(|error| anyhow!(error))?;
        let started = Instant::now();
        let stem = report.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let shot = |suffix: &str| report.with_file_name(format!("{stem}-{suffix}.png"));
        let (open_shot, end_shot) = (shot("open"), shot("end"));
        println!("{}", json!({"fixture":"filter-editors-visual","pid":std::process::id(),"report":report}));
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let editor = options.editor;
            let opener_host = host.clone();
            let handle = cx.open_window(WindowOptions { titlebar: None,
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(60.0), px(80.0)), size(px(options.width), px(options.height))))),
                focus: false, show: false, window_min_size: Some(size(px(250.0), px(250.0))), ..Default::default()
            }, move |window, cx| cx.new(|cx| {
                window.set_window_title("ClipHost filter popover fixture");
                let mut view = ClipboardPreview::new(tokens, UiModel::Fixture(model), options.dark,
                    "all".into(), String::new(), true, false, started, window, cx);
                view.enable_diagnostics();
                match editor {
                    Editor::Categories => view.open_tag_editor_visual(opener_host, window, cx),
                    Editor::ProcessAdd => view.open_process_editor_visual(opener_host, false, window, cx),
                    Editor::ProcessEdit => view.open_process_editor_visual(opener_host, true, window, cx),
                }
                view
            })).expect("create isolated filter popover preview");
            handle.update(cx, |_, window, _| {
                platform::register_probe_window(window).expect("register own fixture HWND");
                platform::set_no_activate(window, true).expect("no-activate fixture");
                platform::set_geometry(window, Some((60, 80)), Some((f64::from(options.width), f64::from(options.height))))
                    .expect("size own fixture");
            }).expect("configure fixture");
            cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
            cx.spawn(async move |cx| {
                let mut shown = false;
                let mut restored = false;
                let mut open_capture = None;
                let mut end_capture = None;
                let mut probe = WheelProbe::default();
                let mut hwnd = 0isize;
                loop {
                    Timer::after(Duration::from_millis(120)).await;
                    let elapsed = started.elapsed();
                    let self_exit = elapsed >= Duration::from_millis(options.exit_ms);
                    let result = handle.update(cx, |view, window, cx| {
                        let pending = platform::has_pending(window).unwrap_or(true);
                        if !shown && !pending {
                            platform::show_no_activate(window).expect("show fixture without activation");
                            unsafe {
                                use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE};
                                SetWindowPos(platform::hwnd(window).expect("own fixture HWND"), Some(HWND_BOTTOM), 0, 0, 0, 0,
                                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE).expect("lower own fixture");
                            }
                            shown = true;
                        }
                        hwnd = platform::hwnd(window).map(|hwnd| hwnd.0 as isize).unwrap_or(0);
                        if options.restore && !restored && open_capture.is_some() {
                            restored = view.press_tag_restore_visual(window, cx);
                        }
                        let main = view.report(window, cx);
                        let ready = open_capture.is_some() && (!options.restore || restored);
                        if options.wheel && ready && !probe.done {
                            let scroll = scroll_of(&main, editor);
                            let offset = scroll["offset"].as_f64();
                            let (x, y, w, h) = (scroll["x"].as_f64(), scroll["y"].as_f64(), scroll["width"].as_f64(), scroll["height"].as_f64());
                            if let (Some(offset), Some(x), Some(y), Some(w), Some(h)) = (offset, x, y, w, h) {
                                let (name, fraction) = WheelProbe::COLUMNS[probe.column];
                                let position = point(px((x + w * f64::from(fraction)) as f32), px((y + h * 0.6) as f32));
                                if probe.settle > 0 {
                                    probe.settle -= 1;
                                } else {
                                    let stalled = probe.last.is_some_and(|last| (last - offset).abs() < 0.5);
                                    probe.samples.push(json!({"column":name,"notch":probe.notches,"offset":offset,"max":scroll["max"],
                                        "x":f32::from(position.x),"y":f32::from(position.y)}));
                                    if stalled || probe.notches >= WheelProbe::MAX_NOTCHES {
                                        probe.column += 1;
                                        probe.notches = 0;
                                        probe.last = None;
                                        if probe.column >= WheelProbe::COLUMNS.len() {
                                            probe.done = true;
                                        } else {
                                            view.reset_filter_popover_scroll_visual(window, cx);
                                            probe.settle = 3;
                                        }
                                    } else {
                                        probe.last = Some(offset);
                                        window.dispatch_input(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                            position, pressed_button: None, modifiers: Modifiers::default(),
                                        }), cx);
                                        window.dispatch_input(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                            position, delta: gpui::ScrollDelta::Lines(point(0., -1.)),
                                            modifiers: Modifiers::default(), touch_phase: gpui::TouchPhase::Moved,
                                        }), cx);
                                        probe.notches += 1;
                                        probe.settle = 3;
                                    }
                                }
                            }
                        }
                        json!({"fixture":"filter-editors-visual","debugOnly":true,"output":root,
                            "width":options.width,"height":options.height,"theme":if options.dark {"dark"} else {"light"},
                            "restoreRequested":options.restore,"restored":restored,
                            "wheel":if options.wheel { json!({"done":probe.done,"samples":probe.samples}) } else { Value::Null },
                            "inputInjected":false,"shownWithoutActivation":shown,"pending":pending,
                            "filterPopover":main["filterPopover"],"elapsedMs":elapsed.as_millis()})
                    });
                    let Ok(mut value) = result else { break };
                    if open_capture.is_none() && shown && elapsed >= Duration::from_millis(1400) {
                        open_capture = Some(capture(hwnd, &open_shot).unwrap_or_else(|error| json!({"error":error.to_string()})));
                    }
                    let finished = !options.wheel || probe.done;
                    if end_capture.is_none() && open_capture.is_some() && finished && (options.restore || options.wheel)
                        && elapsed >= Duration::from_millis(2200) {
                        end_capture = Some(capture(hwnd, &end_shot).unwrap_or_else(|error| json!({"error":error.to_string()})));
                    }
                    value["openScreenshot"] = open_capture.clone().unwrap_or(Value::Null);
                    value["endScreenshot"] = end_capture.clone().unwrap_or(Value::Null);
                    let _ = serde_json::to_vec_pretty(&value).map(|bytes| fs::write(&report, bytes));
                    let complete = open_capture.is_some() && finished && (end_capture.is_some() || !(options.restore || options.wheel));
                    if self_exit || complete { let _ = cx.update(|cx| cx.quit()); break }
                }
            }).detach();
        });
        Ok(())
    }
}
