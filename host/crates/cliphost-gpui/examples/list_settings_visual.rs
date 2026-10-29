//! Debug-only production list-settings/feedback/menu-layout rendering over fixed rows.
//! The disposable settings backend never starts NativeHost or an API listener.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("list_settings_visual requires a Windows Debug build");
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
        sync::{atomic::{AtomicBool, Ordering}, Arc},
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
        point, prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions,
    };
    use serde_json::{json, Value};

    struct Options {
        dark: bool,
        width: f32,
        height: f32,
        behavior: bool,
        english: bool,
        choice: Option<String>,
        custom: bool,
        context_menu_layout: bool,
        return_top: bool,
        return_top_reset: bool,
        appearance: Option<String>,
        favorite_feedback: Option<String>,
        hold_favorite_feedback: bool,
        report: Option<PathBuf>,
        exit_ms: u64,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut options = Self {
                dark: true,
                width: 420.0,
                height: 560.0,
                behavior: false,
                english: false,
                choice: None,
                custom: false,
                context_menu_layout: false,
                return_top: false,
                return_top_reset: false,
                appearance: None,
                favorite_feedback: None,
                hold_favorite_feedback: false,
                report: None,
                exit_ms: 10_000,
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
                    "--width" | "--height" => {
                        let value: f32 = args.next().context("size requires a value")?.parse()?;
                        let minimum = if arg == "--width" { 300.0 } else { 250.0 };
                        if !value.is_finite() || !(minimum..=1600.0).contains(&value) {
                            bail!("width must be 300..1600 and height 250..1600 DIP");
                        }
                        if arg == "--width" {
                            options.width = value
                        } else {
                            options.height = value
                        }
                    }
                    "--behavior" => options.behavior = true,
                    "--lang" => {
                        options.english = match args.next().as_deref() {
                            Some("zh") => false,
                            Some("en") => true,
                            _ => bail!("--lang requires zh or en"),
                        };
                    }
                    "--choice" => {
                        options.choice = Some(match args.next().as_deref() {
                            Some("dismiss") => "DismissBehavior".into(),
                            Some("execute") => "ClipListExecuteMode".into(),
                            _ => bail!("--choice requires dismiss or execute"),
                        });
                        options.behavior = true;
                    }
                    "--custom" => options.custom = true,
                    "--context-menu-layout" => options.context_menu_layout = true,
                    "--return-top" => options.return_top = true,
                    "--return-top-reset" => {
                        options.return_top = true;
                        options.return_top_reset = true;
                    }
                    "--appearance" => {
                        let appearance = args.next().context("--appearance requires standard, simple, custom-all or custom-empty")?;
                        if !matches!(appearance.as_str(), "standard" | "simple" | "custom-all" | "custom-empty") {
                            bail!("--appearance requires standard, simple, custom-all or custom-empty");
                        }
                        options.appearance = Some(appearance);
                    }
                    "--favorite-feedback" => {
                        let scenario = args.next().context("--favorite-feedback requires root, nested or undone")?;
                        if !matches!(scenario.as_str(), "root" | "nested" | "undone") {
                            bail!("--favorite-feedback requires root, nested or undone");
                        }
                        options.favorite_feedback = Some(scenario);
                    }
                    "--hold-favorite-feedback" => options.hold_favorite_feedback = true,
                    "--report" => {
                        options.report =
                            Some(args.next().context("--report requires a path")?.into())
                    }
                    "--exit-after-ms" => {
                        options.exit_ms = args
                            .next()
                            .context("--exit-after-ms requires a value")?
                            .parse()?;
                        if !(2_000..=60_000).contains(&options.exit_ms) {
                            bail!("--exit-after-ms must be 2000..60000");
                        }
                    }
                    "--help" | "-h" => {
                        println!("list_settings_visual --theme dark|light --width 420 --height 560 [--behavior] [--custom] [--favorite-feedback root|nested|undone [--hold-favorite-feedback]] [--context-menu-layout --appearance standard|simple|custom-all|custom-empty] [--report <repository .tmp/report.json>] [--exit-after-ms 10000]");
                        println!("--favorite-feedback seeds the production feedback bubble over fixed rows instead of opening list settings.");
                        println!("--hold-favorite-feedback explicitly freezes that fixture bubble for static capture; without it production expiry remains active.");
                        println!("--context-menu-layout checks real row bounds across closed/open/closed menu visibility states, with at least two rendered frames per state and fresh row builds after each toggle; no native context menu is opened.");
                        println!("--return-top seeds the production back-to-top button over fixed rows. --return-top-reset also calls the production reset after 1500 ms and records before/after state and screenshots.");
                        println!("Fixed clipboard rows and disposable headless settings only. Own-HWND PrintWindow screenshot is written beside the report. No API, NativeHost, capture, hooks, RPC, OLE or input injection.");
                        println!("--lang zh|en selects fixture language. --choice dismiss|execute opens a production settings dropdown after its trigger is laid out; implies --behavior.");
                        return Ok(None);
                    }
                    _ => bail!("unknown argument: {arg}"),
                }
            }
            if options.hold_favorite_feedback && options.favorite_feedback.is_none() {
                bail!("--hold-favorite-feedback requires --favorite-feedback");
            }
            if options.appearance.is_some() && !options.context_menu_layout {
                bail!("--appearance requires --context-menu-layout");
            }
            if options.context_menu_layout && (options.behavior || options.custom || options.favorite_feedback.is_some()) {
                bail!("--context-menu-layout cannot be combined with settings or favorite-feedback scenarios");
            }
            if options.return_top && (options.behavior || options.custom || options.context_menu_layout || options.favorite_feedback.is_some()) {
                bail!("--return-top cannot be combined with settings, context-menu-layout or favorite-feedback scenarios");
            }
            Ok(Some(options))
        }
    }

    fn menu_layout_configuration(appearance: &str) -> Value {
        match appearance {
            "standard" | "simple" => json!({"clipListAppearanceSlot":appearance}),
            "custom-all" | "custom-empty" => json!({
                "clipListAppearanceSlot":"custom",
                "clipListCustomAppearance":{
                    "chrome":"rounded", "layout":"standard", "showProcess":true, "showTime":true,
                    "hoverActions":if appearance == "custom-all" {
                        vec!["primary", "edit", "pin", "favorite", "delete"]
                    } else { vec![] }
                }
            }),
            _ => unreachable!("validated appearance"),
        }
    }

    #[derive(Default)]
    struct MenuLayoutProbe {
        phase: usize,
        stage_started_renders: u64,
        samples: Vec<Value>,
        errors: Vec<String>,
    }

    impl MenuLayoutProbe {
        const STAGES: [&'static str; 3] = ["closed-before", "open", "closed-after"];

        fn observe(&mut self, report: &Value) -> Option<bool> {
            if self.complete() { return None; }
            let renders = report["viewRenderCount"].as_u64().unwrap_or(0);
            let rendered_since_toggle = renders.saturating_sub(self.stage_started_renders);
            let rows = report["rowBounds"].as_array();
            if rendered_since_toggle < 2 || rows.is_none_or(Vec::is_empty) { return None; }
            let rows = rows.unwrap();
            let stage = Self::STAGES[self.phase];
            let menu_open = report["menuLease"].as_u64().is_some();
            if menu_open != (self.phase == 1) {
                self.errors.push(format!("{stage}: reported menu state did not match the requested state"));
            }
            for row in rows {
                if ["x", "y", "width", "height"].iter().any(|field| {
                    !row[*field].as_f64().is_some_and(f64::is_finite)
                }) || row["width"].as_f64().unwrap_or(0.0) <= 0.0
                    || row["height"].as_f64().unwrap_or(0.0) <= 0.0 {
                    self.errors.push(format!("{stage}: invalid row bounds: {row}"));
                }
            }
            let rows_built = report["rowsBuilt"].as_u64().unwrap_or(0);
            if rows_built == 0 {
                self.errors.push(format!("{stage}: no production rows were built"));
            }
            if let Some(previous) = self.samples.last() {
                if rows_built <= previous["rowsBuilt"].as_u64().unwrap_or(u64::MAX) {
                    self.errors.push(format!("{stage}: rows were not rebuilt after toggling menu state"));
                }
                if self.samples[0]["rowBounds"] != report["rowBounds"] {
                    self.errors.push(format!("{stage}: row positions or dimensions changed from closed-before"));
                }
            }
            self.samples.push(json!({"stage":stage,"menuLease":report["menuLease"],
                "viewRenderCount":renders,"viewRendersSinceToggle":rendered_since_toggle,
                "rowsBuilt":rows_built,"rowBounds":report["rowBounds"]}));
            self.phase += 1;
            self.stage_started_renders = renders;
            match self.phase { 1 => Some(true), 2 => Some(false), _ => None }
        }

        fn complete(&self) -> bool { self.phase == Self::STAGES.len() }

        fn report(&self, appearance: &str) -> Value {
            json!({"appearance":appearance,"complete":self.complete(),
                "stage":Self::STAGES.get(self.phase).copied().unwrap_or("complete"),
                "minimumViewRendersPerStage":2,"samples":self.samples,"errors":self.errors,
                "passed":self.complete() && self.errors.is_empty(),
                "nativeContextMenuOpened":false})
        }
    }

    fn output_paths(report: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?;
        let scratch = repo.join(".tmp");
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        if scratch.parent() != Some(repo.as_path()) {
            bail!("repository .tmp must resolve directly within the repository");
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = scratch.join(format!(
            "list-settings-visual-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root)?;
        let report = match report {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => repo.join(path),
            None => root.join("report.json"),
        };
        let parent = report
            .parent()
            .context("report needs a parent")?
            .canonicalize()
            .context("report parent must already exist under repository .tmp")?;
        if parent == scratch || !parent.starts_with(&scratch) {
            bail!("report must be inside a child directory of repository .tmp");
        }
        let report = parent.join(report.file_name().context("report needs a file name")?);
        if fs::symlink_metadata(&report).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("report must be a regular local file");
        }
        Ok((root, report))
    }

    fn native_report(window: &gpui::Window, foreground_before: isize) -> Result<Value> {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };
        let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
        unsafe {
            let foreground = GetForegroundWindow();
            Ok(
                json!({"hwnd":hwnd.0 as isize,"foregroundBefore":foreground_before,
                "foregroundNow":foreground.0 as isize,"isForeground":foreground == hwnd,
                "noActivate":GetWindowLongPtrW(hwnd,GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize != 0}),
            )
        }
    }

    // Same own-client PrintWindow approach as editor_visual, after releasing the GPUI borrow.
    fn capture(hwnd_value: isize, path: &Path) -> Result<Value> {
        use windows::Win32::{
            Foundation::{HWND, RECT},
            Graphics::Gdi::*,
            UI::WindowsAndMessaging::GetWindowThreadProcessId,
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
            windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect)?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
                bail!("invalid capture size")
            }
            let source = GetDC(Some(hwnd));
            if source.is_invalid() {
                bail!("GetDC failed")
            }
            let dc = CreateCompatibleDC(Some(source));
            let bitmap = CreateCompatibleBitmap(source, width, height);
            if dc.is_invalid() || bitmap.is_invalid() {
                if !dc.is_invalid() {
                    let _ = DeleteDC(dc);
                }
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(bitmap.into());
                }
                ReleaseDC(Some(hwnd), source);
                bail!("cannot allocate capture bitmap");
            }
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
                GetDIBits(
                    dc,
                    bitmap,
                    0,
                    height as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(Some(hwnd), source);
            if !printed || rows != height {
                bail!("PrintWindow/GetDIBits failed (printed={printed}, rows={rows})")
            }
            let first = [pixels[0], pixels[1], pixels[2]];
            let nonuniform = pixels.chunks_exact(4).any(|pixel| pixel[..3] != first);
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            image::save_buffer(
                path,
                &pixels,
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            )?;
            Ok(
                json!({"method":"PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)","path":path,
                "width":width,"height":height,"supported":nonuniform,"nonuniform":nonuniform,
                "desktopRead":false,"inputInjected":false,"activationRequested":false}),
            )
        }
    }

    pub fn run() -> Result<()> {
        let Some(options) = Options::parse()? else {
            return Ok(());
        };
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
        fs::write(
            paths.configs.join("GlobalConfig.json"),
            serde_json::to_vec_pretty(&config)?,
        )?;
        // Explicit AppPaths plus headless guards avoid all production data resolution and OS services.
        cliphost_core::force_headless(true);
        set_language(if options.english { LanguageMode::English } else { LanguageMode::Chinese });
        let host = Arc::new(HostState::open(paths.clone(), 29065).map_err(|error| anyhow!(error))?);
        let mut tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        tokens.window.width = options.width;
        tokens.window.height = options.height;
        let model = PreviewModel::from_json(FIXTURE_JSON).map_err(|error| anyhow!(error))?;
        let started = Instant::now();
        let foreground_before =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize };
        let screenshot = report.with_file_name(format!(
            "{}-client.png",
            report.file_stem().unwrap_or_default().to_string_lossy()
        ));
        let reset_screenshot = report.with_file_name(format!(
            "{}-after-reset.png",
            report.file_stem().unwrap_or_default().to_string_lossy()
        ));
        let context_menu_layout = options.context_menu_layout;
        let layout_passed = Arc::new(AtomicBool::new(false));
        let layout_result = layout_passed.clone();
        println!(
            "{}",
            json!({"fixture":"list-settings-visual","pid":std::process::id(),"report":report,"dataRoot":paths.data_root,
                "favoriteFeedbackScenario":options.favorite_feedback,"favoriteFeedbackHeld":options.hold_favorite_feedback,
                "contextMenuLayout":options.context_menu_layout,"appearance":options.appearance,
                "returnTop":options.return_top,"returnTopReset":options.return_top_reset})
        );
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let feedback_for_view = options.favorite_feedback.clone();
            let appearance = options.appearance.clone().unwrap_or_else(|| "standard".into());
            let layout_configuration = menu_layout_configuration(&appearance);
            let handle = cx.open_window(WindowOptions { titlebar: None,
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(60.0),px(80.0)),size(px(options.width),px(options.height))))),
                focus:false,show:false,window_min_size:Some(size(px(300.0),px(250.0))),..Default::default()
            }, move |window,cx| cx.new(|cx| {
                window.set_window_title(if options.return_top {
                    "ClipHost back to top fixture"
                } else if options.context_menu_layout {
                    "ClipHost context menu row layout fixture"
                } else if feedback_for_view.is_some() {
                    "ClipHost favorite feedback fixture"
                } else { "ClipHost list settings fixture" });
                let mut view = ClipboardPreview::new(tokens,UiModel::Fixture(model),options.dark,
                    "all".into(),String::new(),true,false,started,window,cx);
                view.enable_diagnostics();
                if options.context_menu_layout {
                    assert!(view.seed_context_menu_layout_visual(layout_configuration,cx), "isolated menu-layout fixture seed refused");
                } else if let Some(scenario) = feedback_for_view.as_deref() {
                    let target = match scenario {
                        "root" => Some("总目录"),
                        "nested" => Some("工作 / 项目 / 剪贴板 / 界面设计 / 跨平台组件与交互规范"),
                        "undone" => None,
                        _ => unreachable!("validated favorite feedback scenario"),
                    };
                    assert!(view.seed_favorite_feedback_visual(target,cx), "isolated feedback fixture seed refused");
                    if options.hold_favorite_feedback {
                        assert!(view.hold_favorite_feedback_visual(cx), "isolated feedback fixture hold refused");
                    }
                } else if !options.return_top {
                    view.open_list_settings_visual(host,options.behavior,options.custom,window,cx);
                }
                view
            })).expect("create isolated list-settings preview");
            handle.update(cx, |_,window,_| {
                platform::register_probe_window(window).expect("register own fixture HWND");
                platform::set_no_activate(window,true).expect("no-activate list-settings fixture");
                platform::set_geometry(window,Some((60,80)),Some((f64::from(options.width),f64::from(options.height))))
                    .expect("size own list-settings fixture");
            }).expect("configure list-settings fixture");
            cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
            cx.spawn(async move |cx| {
                let mut shown = false;
                let mut capture_result = None;
                let mut reset_capture_result = None;
                let mut return_top_before = None;
                let mut return_top_seeded_at = None;
                let mut return_top_reset_at = None;
                let mut layout_probe = MenuLayoutProbe::default();
                let mut choice_opened_at = None;
                loop {
                    Timer::after(Duration::from_millis(250)).await;
                    let self_exit = started.elapsed() >= Duration::from_millis(options.exit_ms);
                    let result = handle.update(cx, |view,window,cx| {
                        let pending = platform::has_pending(window).unwrap_or(true);
                        if !shown && !pending {
                            platform::show_no_activate(window).expect("show fixture without activation");
                            unsafe {
                                use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos,HWND_BOTTOM,SWP_NOACTIVATE,SWP_NOMOVE,SWP_NOSIZE};
                                SetWindowPos(platform::hwnd(window).expect("own fixture HWND"),Some(HWND_BOTTOM),0,0,0,0,
                                    SWP_NOACTIVATE|SWP_NOMOVE|SWP_NOSIZE).expect("lower own fixture without activation");
                            }
                            shown = true;
                        }
                        let mut main = view.report(window,cx);
                        if shown && !pending && choice_opened_at.is_none() {
                            if let Some(key) = options.choice.as_deref() {
                                if view.open_list_settings_choice_visual(key, window, cx) {
                                    choice_opened_at = Some(Instant::now());
                                }
                            }
                        }
                        if options.return_top && return_top_reset_at.is_none() && shown && !pending {
                            if return_top_seeded_at.is_none() && started.elapsed() >= Duration::from_millis(500) {
                                // new() schedules its first refresh; seed only after it
                                // has completed, then observe a later layout frame.
                                assert!(view.seed_history_return_top_visual(3,cx), "isolated return-top fixture seed refused");
                                return_top_seeded_at = Some(Instant::now());
                            } else if return_top_seeded_at.is_some() && return_top_before.is_none()
                                && main["historyReturnTop"]["itemIndex"].as_u64().is_some_and(|index| index > 0)
                                && main["historyReturnTop"]["buttonVisible"] == true {
                                return_top_before = Some(main["historyReturnTop"].clone());
                            }
                            if options.return_top_reset && return_top_before.is_some() && capture_result.is_some()
                                && started.elapsed() >= Duration::from_millis(1500) {
                                assert!(view.return_history_to_top_visual(cx), "isolated return-top reset refused");
                                return_top_reset_at = Some(Instant::now());
                                main = view.report(window,cx);
                            }
                        }
                        if options.context_menu_layout && shown && !pending && !layout_probe.complete() {
                            if let Some(open) = layout_probe.observe(&main) {
                                assert!(view.set_context_menu_layout_visual(open,cx), "isolated menu-layout toggle refused");
                            }
                            if layout_probe.complete() {
                                layout_passed.store(layout_probe.errors.is_empty(), Ordering::SeqCst);
                            } else {
                                // A second render, plus the toggle's list invalidation, makes cached row bounds insufficient.
                                cx.notify();
                            }
                        }
                        json!({"fixture":"list-settings-visual","pid":std::process::id(),"debugOnly":true,
                            "dataRoot":paths.data_root,"output":root,"report":report,"theme":if options.dark{"dark"}else{"light"},
                            "width":options.width,"height":options.height,"behavior":options.behavior,"custom":options.custom,
                            "language":if options.english {"en"} else {"zh"},"choice":options.choice,"choiceOpened":choice_opened_at.is_some(),
                            "favoriteFeedbackScenario":options.favorite_feedback,
                            "favoriteFeedbackHeld":options.hold_favorite_feedback,
                            "contextMenuLayout":if options.context_menu_layout { layout_probe.report(&appearance) } else { Value::Null },
                            "returnTopProbe":if options.return_top { json!({
                                "seeded":return_top_seeded_at.is_some(),"beforeReady":return_top_before.is_some(),
                                "before":return_top_before,"resetPerformed":return_top_reset_at.is_some(),
                                "after":if return_top_reset_at.is_some() { main["historyReturnTop"].clone() } else { Value::Null }
                            }) } else { Value::Null },
                            "headless":cliphost_core::is_headless(),"nativeHost":false,"apiListener":false,"capture":false,
                            "hooks":false,"rpc":false,"ole":false,"loginStartup":false,"focusRequested":false,"inputInjected":false,
                            "shownWithoutActivation":shown,"nativePlacementPending":pending,
                            "native":native_report(window,foreground_before).unwrap_or_else(|error|json!({"error":error.to_string()})),
                            "main":main,"nativeError":platform::take_error(window).ok().flatten(),
                            "elapsedMs":started.elapsed().as_millis(),"selfExitRequested":self_exit})
                    });
                    let Ok(mut value) = result else { break };
                    if capture_result.is_none() && shown && (!options.return_top || return_top_before.is_some())
                        && (options.choice.is_none() || choice_opened_at.is_some_and(|opened: Instant| opened.elapsed() >= Duration::from_millis(500)))
                        && !value["nativePlacementPending"].as_bool().unwrap_or(true)
                        && started.elapsed() >= Duration::from_millis(1250) {
                        capture_result = Some(capture(value["native"]["hwnd"].as_i64().unwrap_or(0) as isize,&screenshot)
                            .unwrap_or_else(|error|json!({"error":error.to_string(),"supported":false})));
                    }
                    value["screenshot"] = capture_result.clone().unwrap_or_else(||json!({"pending":true}));
                    if reset_capture_result.is_none() && return_top_reset_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(500)) {
                        reset_capture_result = Some(capture(value["native"]["hwnd"].as_i64().unwrap_or(0) as isize,&reset_screenshot)
                            .unwrap_or_else(|error|json!({"error":error.to_string(),"supported":false})));
                    }
                    value["resetScreenshot"] = reset_capture_result.clone().unwrap_or(Value::Null);
                    if let Err(error) = serde_json::to_vec_pretty(&value).map_err(|error|anyhow!(error))
                        .and_then(|bytes|fs::write(&report,bytes).map_err(|error|anyhow!(error))) {
                        eprintln!("fixture report failed: {error}");
                        let _ = cx.update(|cx|cx.quit());
                        break;
                    }
                    if self_exit { let _ = cx.update(|cx|cx.quit()); break }
                }
            }).detach();
        });
        if context_menu_layout && !layout_result.load(Ordering::SeqCst) {
            bail!("context-menu row layout regression failed or did not finish; inspect the fixture report");
        }
        Ok(())
    }
}
