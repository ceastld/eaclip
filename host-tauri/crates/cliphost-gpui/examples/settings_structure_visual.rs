//! Debug-only rendering of the production SettingsWindow with disposable data.
//! No NativeHost, API listener, RPC, clipboard capture, hooks or login startup.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("settings_structure_visual is available only in Debug builds.");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("settings_structure_visual: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(debug_assertions)]
mod fixture {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{anyhow, bail, ensure, Context as _, Result};
    use cliphost_core::{
        extra_settings::GlobalAppConfig,
        i18n::{set_language, LanguageMode},
        AppPaths, HostState,
    };
    use cliphost_gpui::{
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        settings::SettingsWindow,
        DesignAssets,
    };
    use gpui::{
        AnyWindowHandle, AppContext, Application, MouseButton, PlatformInput, Timer, WindowHandle,
    };
    use serde_json::{json, Value};

    struct Options {
        dark: bool,
        width: f32,
        height: f32,
        page: String,
        choice: Option<String>,
        scroll_y: f32,
        scroll_after_open: Option<f32>,
        language: LanguageMode,
        exit_after_ms: u64,
        report: Option<PathBuf>,
        super_menu_scenario: bool,
        lan_enabled: bool,
        lan_running_fixture: bool,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut options = Self {
                dark: false,
                width: 640.0,
                height: 600.0,
                page: "global".into(),
                choice: None,
                scroll_y: 0.0,
                scroll_after_open: None,
                language: LanguageMode::Chinese,
                exit_after_ms: 10_000,
                report: None,
                super_menu_scenario: false,
                lan_enabled: false,
                lan_running_fixture: false,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--theme" => {
                        options.dark = match args.next().as_deref() {
                            Some("light") => false,
                            Some("dark") => true,
                            _ => bail!("--theme requires light or dark"),
                        };
                    }
                    "--width" | "--height" => {
                        let value: f32 = args.next().context("size requires a value")?.parse()?;
                        if !value.is_finite() || !(400.0..=1600.0).contains(&value) {
                            bail!("window dimensions must be finite and between 400 and 1600 DIP");
                        }
                        if arg == "--width" {
                            options.width = value;
                        } else {
                            options.height = value;
                        }
                    }
                    "--page" => {
                        options.page =
                            args.next().context("--page requires a settings category")?;
                        if !matches!(
                            options.page.as_str(),
                            "global" | "quickPaste" | "clip" | "menu_global" | "menu_file" | "textAction" | "deviceSync"
                                | "home" | "devices" | "lan" | "textActions"
                        ) {
                            bail!("--page requires global, quickPaste, clip, menu_global, menu_file, textAction, deviceSync, home, devices, lan or textActions");
                        }
                    }
                    "--scenario" => {
                        if args.next().as_deref() != Some("super-menu") {
                            bail!("--scenario requires super-menu");
                        }
                        options.super_menu_scenario = true;
                    }
                    "--lan-enabled" => {
                        if !cfg!(windows) {
                            bail!("--lan-enabled requires Windows for socket-free adapter enumeration");
                        }
                        options.lan_enabled = true;
                    }
                    "--lan-running-fixture" => {
                        if !cfg!(windows) {
                            bail!("--lan-running-fixture requires Windows");
                        }
                        options.lan_running_fixture = true;
                    }
                    "--language" => {
                        options.language = match args.next().as_deref() {
                            Some("zh-CN") => LanguageMode::Chinese,
                            Some("en") => LanguageMode::English,
                            _ => bail!("--language requires zh-CN or en"),
                        };
                    }
                    "--choice" => {
                        options.choice =
                            Some(args.next().context("--choice requires a field key")?);
                    }
                    "--scroll-y" | "--scroll-after-open" => {
                        let value: f32 = args
                            .next()
                            .context("scroll requires a DIP value")?
                            .parse()?;
                        if !value.is_finite() || !(0.0..=20_000.0).contains(&value) {
                            bail!("scroll must be finite and between 0 and 20000 DIP");
                        }
                        if arg == "--scroll-y" {
                            options.scroll_y = value;
                        } else {
                            options.scroll_after_open = Some(value);
                        }
                    }
                    "--exit-after-ms" => {
                        options.exit_after_ms = args
                            .next()
                            .context("--exit-after-ms requires a value")?
                            .parse()?;
                        if !(10_000..=60_000).contains(&options.exit_after_ms) {
                            bail!("--exit-after-ms must be between 10000 and 60000");
                        }
                    }
                    "--report" => {
                        options.report =
                            Some(args.next().context("--report requires a path")?.into());
                    }
                    "--help" | "-h" => {
                        println!("settings_structure_visual --theme light|dark --language zh-CN|en --page global|quickPaste|clip|menu_global|menu_file|textAction|deviceSync|home|devices|lan|textActions --width 640 --height 600 --choice ClipListLayoutDensity --scroll-y 300 --scroll-after-open 350 --exit-after-ms 10000 --report .tmp/report.json");
                        println!("--scenario super-menu --page menu_global --exit-after-ms 20000 verifies real tab clicks, rapid navigation, a pixel wheel and retained forms across old menu routes.");
                        println!("--lan-enabled enables only this fresh disposable host's LAN configuration for pairing-code rendering; no network host or discovery service is started (Windows only).");
                        println!("--lan-running-fixture requires --lan-enabled and simulates only the running status flag, without a listener, to verify automatic pairing-code renewal.");
                        println!("Disposable settings; scroll offsets are positive DIP from the top. Optional own-HWND PrintWindow screenshots are written beside the report, whose parent must exist under this repository's .tmp.");
                        return Ok(None);
                    }
                    _ => bail!("unknown argument: {arg}"),
                }
            }
            if options.lan_running_fixture && !options.lan_enabled {
                bail!("--lan-running-fixture requires --lan-enabled");
            }
            if options.scroll_after_open.is_some() && options.choice.is_none() {
                bail!("--scroll-after-open requires --choice");
            }
            if options.super_menu_scenario {
                if options.page == "global" {
                    options.page = "menu_global".into();
                }
                if !matches!(
                    options.page.as_str(),
                    "menu_global" | "menu_file" | "textAction"
                ) || options.choice.is_some()
                    || options.scroll_after_open.is_some()
                    || options.scroll_y != 0.
                {
                    bail!("super-menu scenario requires a menu page and cannot combine dropdown/explicit scroll probes");
                }
                if options.exit_after_ms < 20_000 {
                    bail!("super-menu scenario requires --exit-after-ms >= 20000");
                }
            }
            Ok(Some(options))
        }
    }

    fn isolated_root(repo: &Path) -> Result<PathBuf> {
        let tmp = repo.join(".tmp");
        fs::create_dir_all(&tmp)?;
        let tmp = tmp.canonicalize()?;
        if !tmp.starts_with(repo) || tmp == repo {
            bail!("repository .tmp must resolve within the repository");
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = tmp.join(format!(
            "settings-structure-visual-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root)?;
        Ok(root)
    }

    fn report_path(repo: &Path, root: &Path, given: Option<&Path>) -> Result<PathBuf> {
        let path = match given {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => repo.join(path),
            None => root.join("diagnostics.json"),
        };
        let parent = path
            .parent()
            .context("report path has no parent")?
            .canonicalize()
            .context("report parent must already exist")?;
        if !parent.starts_with(repo.join(".tmp").canonicalize()?) {
            bail!("report must stay under this repository's .tmp");
        }
        let path = parent.join(path.file_name().context("report path has no file name")?);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("report must be a regular local file");
        }
        Ok(path)
    }

    fn diagnostics(
        settings: &SettingsWindow,
        window: &gpui::Window,
        cx: &gpui::App,
        options: &Options,
        root: &Path,
        started: Instant,
        self_exit: bool,
    ) -> Value {
        #[cfg(windows)]
        let hwnd = platform::hwnd(window).ok().map(|value| value.0 as isize);
        #[cfg(not(windows))]
        let hwnd: Option<isize> = None;
        let viewport = window.viewport_size();
        json!({
            "fixture":"settings-structure-visual", "pid":std::process::id(), "hwnd":hwnd,
            "theme":if options.dark {"dark"} else {"light"}, "page":options.page,
            "language":if options.language == LanguageMode::English { "en" } else { "zh-CN" }, "dataRoot":root,
            "width":options.width, "height":options.height,
            "viewport":{"width":f32::from(viewport.width),"height":f32::from(viewport.height)},
            "scaleFactor":window.scale_factor(), "elapsedMs":started.elapsed().as_millis(),
            "exitAfterMs":options.exit_after_ms, "selfExitRequested":self_exit,
            "debugOnly":true, "headless":cliphost_core::is_headless(),
            "nativeHost":false, "apiListener":false, "capture":false, "hooks":false,
            "rpc":false, "loginStartup":false, "focusRequested":false,
            "lanConfigEnabled":options.lan_enabled,
            "lanRunningSimulated":options.lan_running_fixture,
            "settings":settings.diagnostics(cx),
            "choiceKey":options.choice,
            "scenario":if options.super_menu_scenario { "super-menu" } else { "static" },
            "requestedScrollY":options.scroll_y,
            "requestedScrollAfterOpen":options.scroll_after_open,
            "choiceGeometry":settings.settings_choice_visual_diagnostics(cx),
            "native":native_report(window).unwrap_or_else(|error|json!({"error":error.to_string()})),
            "nativeError":platform::take_error(window).ok().flatten()
        })
    }

    const MENU_SECTIONS: [&str; 5] = ["general", "items", "windows", "files", "text-actions"];
    const MENU_STEPS: [(&str, &str); 10] = [
        ("tab", "general"),
        ("tab", "items"),
        ("tab", "windows"),
        ("tab", "files"),
        ("tab", "text-actions"),
        ("rapid", "items"),
        ("wheel", "text-actions"),
        ("route", "files"),
        ("route", "text-actions"),
        ("route", "general"),
    ];

    #[derive(Default)]
    struct SuperMenuScenario {
        step: usize,
        waiting_since: Option<Instant>,
        last_scroll_top: Option<f64>,
        stable_polls: usize,
        wheel_before: Option<f64>,
        entities: serde_json::Map<String, Value>,
        cases: Vec<Value>,
        error: Option<String>,
        complete: bool,
    }

    fn measured_bounds(value: &Value) -> Result<(f32, f32, f32, f32)> {
        let coordinate = |key: &str| -> Result<f32> {
            let value = value[key]
                .as_f64()
                .with_context(|| format!("Missing {key} in painted bounds"))?
                as f32;
            ensure!(value.is_finite(), "Non-finite painted coordinate");
            Ok(value)
        };
        let bounds = (
            coordinate("x")?,
            coordinate("y")?,
            coordinate("width")?,
            coordinate("height")?,
        );
        ensure!(
            bounds.2 > 0. && bounds.3 >= 0.,
            "Painted marker must have a width and non-negative height"
        );
        Ok(bounds)
    }

    fn finite_bounds(value: &Value) -> Result<(f32, f32, f32, f32)> {
        let bounds = measured_bounds(value)?;
        ensure!(
            bounds.3 > 0.,
            "Painted interactive bounds must have positive area"
        );
        Ok(bounds)
    }

    fn dispatch_click(
        handle: WindowHandle<SettingsWindow>,
        at: (f32, f32),
        cx: &mut impl AppContext,
    ) -> Result<()> {
        AnyWindowHandle::from(handle).update(cx, |_, window, cx| {
            let position = gpui::point(gpui::px(at.0), gpui::px(at.1));
            window.dispatch_input(
                PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_input(
                PlatformInput::MouseDown(gpui::MouseDownEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_input(
                PlatformInput::MouseUp(gpui::MouseUpEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
        })?;
        Ok(())
    }

    fn dispatch_wheel(
        handle: WindowHandle<SettingsWindow>,
        at: (f32, f32),
        delta: (f32, f32),
        cx: &mut impl AppContext,
    ) -> Result<()> {
        AnyWindowHandle::from(handle).update(cx, |_, window, cx| {
            let position = gpui::point(gpui::px(at.0), gpui::px(at.1));
            window.dispatch_input(
                PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_input(
                PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(
                        gpui::px(delta.0),
                        gpui::px(delta.1),
                    )),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                }),
                cx,
            );
        })?;
        Ok(())
    }

    fn dispatch_tab(
        handle: WindowHandle<SettingsWindow>,
        menu: &Value,
        id: &str,
        cx: &mut impl AppContext,
    ) -> Result<bool> {
        let section = menu["sections"]
            .as_array()
            .context("Missing painted sections")?
            .iter()
            .find(|section| section["id"] == id)
            .context("Missing requested tab")?;
        let (x, y, width, height) = finite_bounds(&section["navBounds"])?;
        let (scroll_x, scroll_y, scroll_width, scroll_height) =
            finite_bounds(&menu["navScrollBounds"])?;
        let centre = (x + width / 2., y + height / 2.);
        ensure!(
            centre.1 >= scroll_y && centre.1 <= scroll_y + scroll_height,
            "Tab is outside its navigation viewport vertically"
        );
        if centre.0 < scroll_x || centre.0 > scroll_x + scroll_width {
            // Enter the real tab keyboard handler through a visible tab, then
            // let its reveal behavior paint fresh hitboxes before clicking.
            let visible = menu["sections"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|section| {
                    let bounds = finite_bounds(&section["navBounds"]).ok()?;
                    let point = (bounds.0 + bounds.2 / 2., bounds.1 + bounds.3 / 2.);
                    (point.0 >= scroll_x && point.0 <= scroll_x + scroll_width).then_some(point)
                })
                .context("No visible navigation tab can receive focus")?;
            dispatch_click(handle, visible, cx)?;
            let index = MENU_SECTIONS
                .iter()
                .position(|candidate| *candidate == id)
                .unwrap();
            AnyWindowHandle::from(handle).update(cx, |_, window, cx| {
                for key in std::iter::once("home").chain(std::iter::repeat_n("right", index)) {
                    window.dispatch_input(
                        PlatformInput::KeyDown(gpui::KeyDownEvent {
                            keystroke: gpui::Keystroke {
                                key: key.into(),
                                ..Default::default()
                            },
                            is_held: false,
                        }),
                        cx,
                    );
                }
            })?;
            return Ok(false);
        }
        dispatch_click(handle, centre, cx)?;
        Ok(true)
    }

    impl SuperMenuScenario {
        fn report(&self) -> Value {
            json!({"requested":true,"complete":self.complete,"passed":self.complete && self.error.is_none(),
                "completedSteps":self.step,"expectedSteps":MENU_STEPS.len(),"cases":self.cases,"error":self.error,
                "dispatch":"own-window GPUI MouseDown/MouseUp and pixel ScrollWheel; no system input",
                "retainedFormEntityIds":self.entities})
        }

        fn tick(
            &mut self,
            handle: WindowHandle<SettingsWindow>,
            value: &Value,
            report: &Path,
            cx: &mut impl AppContext,
        ) -> Result<()> {
            if self.complete || self.error.is_some() {
                return Ok(());
            }
            let settings = &value["settings"];
            let menu = &settings["superMenu"];
            if value["elapsedMs"].as_u64().unwrap_or_default() < 1000
                || settings["schemaLoading"] == true
                || menu["sections"]
                    .as_array()
                    .is_none_or(|sections| sections.len() != MENU_SECTIONS.len())
                || !menu["scrollBounds"].is_object()
                || !menu["navScrollBounds"].is_object()
                || menu["sections"].as_array().unwrap().iter().any(|section| {
                    !section["bounds"].is_object() || !section["navBounds"].is_object()
                })
            {
                return Ok(());
            }
            #[cfg(windows)]
            ensure!(
                value["native"]["noActivate"] == true && value["native"]["isForeground"] == false,
                "Scenario window must remain non-activating"
            );
            ensure!(
                settings["activeCategory"] == "menu_global",
                "Merged menu routes must stay in the menu_global category"
            );
            for id in MENU_SECTIONS {
                let section = menu["sections"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|section| section["id"] == id)
                    .with_context(|| format!("Missing long-page section {id}"))?;
                measured_bounds(&section["bounds"])?;
                finite_bounds(&section["navBounds"])?;
            }
            let viewport = finite_bounds(&menu["scrollBounds"])?;
            let ids = settings["formEntityIds"]
                .as_object()
                .context("Missing retained form entity diagnostics")?;
            ensure!(
                ids.contains_key("menu_global") && ids.contains_key("menu_file"),
                "Long page must retain both menu form entities"
            );
            for id in ["menu_global", "menu_file", "textAction"] {
                if let Some(entity) = ids.get(id) {
                    ensure!(entity.as_u64().is_some(), "Form entity id must be numeric");
                    if let Some(previous) = self.entities.get(id) {
                        ensure!(
                            previous == entity,
                            "Navigating recreated the {id} form entity"
                        );
                    } else {
                        self.entities.insert(id.into(), entity.clone());
                    }
                }
            }
            let top = menu["scrollTop"]
                .as_f64()
                .context("Missing positive scrollTop")?;
            let maximum = menu["maximumScrollTop"]
                .as_f64()
                .context("Missing maximum scrollTop")?;
            ensure!(
                top.is_finite() && top >= -2. && top <= maximum + 2.,
                "Scroll is outside its real range"
            );
            let (kind, target) = MENU_STEPS[self.step];
            if self.waiting_since.is_none() {
                match kind {
                    "tab" => {
                        if !dispatch_tab(handle, menu, target, cx)? {
                            return Ok(());
                        }
                    }
                    "rapid" => {
                        // Bring the leading tabs into the real overflow viewport
                        // before sending their clicks in one event batch.
                        let nav = finite_bounds(&menu["navScrollBounds"])?;
                        let leading_visible = ["general", "items"].iter().all(|id| {
                            menu["sections"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .find(|section| section["id"] == *id)
                                .and_then(|section| finite_bounds(&section["navBounds"]).ok())
                                .is_some_and(|bounds| {
                                    let centre = bounds.0 + bounds.2 / 2.;
                                    centre >= nav.0 && centre <= nav.0 + nav.2
                                })
                        });
                        if !leading_visible {
                            let _ = dispatch_tab(handle, menu, "general", cx)?;
                            return Ok(());
                        }
                        for id in ["general", "items", "general", "items"] {
                            ensure!(
                                dispatch_tab(handle, menu, id, cx)?,
                                "Rapid-click tabs must be visible together"
                            );
                        }
                    }
                    "wheel" => {
                        self.wheel_before = Some(top);
                        dispatch_wheel(
                            handle,
                            (viewport.0 + viewport.2 / 2., viewport.1 + viewport.3 / 2.),
                            (0., -20_000.),
                            cx,
                        )?;
                    }
                    "route" => {
                        let page = match target {
                            "files" => "menu_file",
                            "text-actions" => "textAction",
                            _ => "menu_global",
                        };
                        handle.update(cx, |settings, window, cx| {
                            settings.set_route(&format!("/main/settings/{page}"), window, cx)
                        })?;
                    }
                    _ => unreachable!(),
                }
                self.waiting_since = Some(Instant::now());
                self.last_scroll_top = None;
                self.stable_polls = 0;
                return Ok(());
            }
            if self
                .last_scroll_top
                .is_some_and(|previous| (top - previous).abs() <= 0.5)
            {
                self.stable_polls += 1;
            } else {
                self.stable_polls = 0;
            }
            self.last_scroll_top = Some(top);
            if menu["activeSection"] != target
                || !menu["pendingSection"].is_null()
                || self.stable_polls < 2
                || self.waiting_since.unwrap().elapsed() < Duration::from_millis(500)
            {
                return Ok(());
            }
            if kind == "wheel" {
                ensure!(
                    top > self.wheel_before.unwrap_or(top) + 1. && (maximum - top).abs() <= 2.,
                    "Body wheel must reach the last section and change actual offset"
                );
            } else {
                let section = menu["sections"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|section| section["id"] == target)
                    .unwrap();
                let heading = measured_bounds(&section["bounds"])?;
                let inset = menu["anchorInset"]
                    .as_f64()
                    .context("Missing real section anchor inset")?;
                let expected = (top + f64::from(heading.1 - viewport.1) - inset).clamp(0., maximum);
                ensure!(
                    (top - expected).abs() <= 2.,
                    "Navigation did not settle at the requested real section heading"
                );
            }
            if kind == "route" && target == "text-actions" {
                ensure!(
                    settings["superMenuActionsExpanded"] == true && ids.contains_key("textAction"),
                    "Old textAction route must expand the retained native manager"
                );
            }
            let stem = report
                .file_stem()
                .and_then(|name| name.to_str())
                .context("Invalid report filename")?;
            let image = report.with_file_name(format!(
                "{stem}.menu-{:02}-{kind}-{target}.png",
                self.step + 1
            ));
            let screenshot = capture(value["hwnd"].as_i64().unwrap_or_default() as isize, &image)?;
            #[cfg(windows)]
            ensure!(
                screenshot["nonuniform"] == true,
                "Menu scenario capture contains no rendered detail"
            );
            self.cases.push(json!({"step":self.step + 1,"kind":kind,"target":target,
                "superMenu":menu,"formEntityIds":ids,"superMenuActionsExpanded":settings["superMenuActionsExpanded"],
                "screenshot":screenshot,"native":value["native"]}));
            self.step += 1;
            self.complete = self.step == MENU_STEPS.len();
            self.waiting_since = None;
            Ok(())
        }
    }

    #[cfg(windows)]
    fn native_report(window: &gpui::Window) -> Result<Value> {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };
        let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
        unsafe {
            Ok(json!({
                "isForeground":GetForegroundWindow() == hwnd,
                "noActivate":GetWindowLongPtrW(hwnd,GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize != 0,
            }))
        }
    }

    #[cfg(not(windows))]
    fn native_report(_: &gpui::Window) -> Result<Value> {
        Ok(json!({"supported":false}))
    }

    // Reads this fixture's client HWND only, after releasing the GPUI borrow.
    #[cfg(windows)]
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
                bail!("capture requires this fixture's own HWND");
            }
            let mut rect = RECT::default();
            windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect)?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
                bail!("invalid capture size");
            }
            let source = GetDC(Some(hwnd));
            if source.is_invalid() {
                bail!("GetDC failed");
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
                bail!("PrintWindow/GetDIBits failed (printed={printed}, rows={rows})");
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
            Ok(json!({
                "method":"PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)","path":path,
                "width":width,"height":height,"supported":nonuniform,"nonuniform":nonuniform,
                "desktopRead":false,"inputInjected":false,"activationRequested":false,
            }))
        }
    }

    #[cfg(not(windows))]
    fn capture(_: isize, _: &Path) -> Result<Value> {
        Ok(json!({"supported":false}))
    }

    pub fn run() -> Result<()> {
        let Some(options) = Options::parse()? else {
            return Ok(());
        };
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?;
        let root = isolated_root(&repo)?;
        let report = report_path(&repo, &root, options.report.as_deref())?;
        let paths = AppPaths::from_data_root(root.clone());
        fs::create_dir(&paths.configs)?;
        let config = GlobalAppConfig {
            ui_theme_mode: if options.dark { "dark" } else { "light" }.into(),
            ui_language: if options.language == LanguageMode::English {
                "en"
            } else {
                "zh-CN"
            }
            .into(),
            launch_at_startup: false,
            window_background_frosted_glass: false,
            ..GlobalAppConfig::default()
        };
        fs::write(
            paths.configs.join("GlobalConfig.json"),
            serde_json::to_vec_pretty(&config)?,
        )?;
        // Set this before HostState::open: every OS monitor/hotkey/startup entry
        // has a headless guard. Explicit AppPaths never resolves product data.
        cliphost_core::force_headless(true);
        set_language(options.language);
        let host = Arc::new(HostState::open(paths, 29065).map_err(|error| anyhow!(error))?);
        if options.lan_enabled {
            // This fresh root has no peers or queued messages. Changing its
            // configuration never starts NativeHost or HTTP/UDP discovery;
            // Windows status reads enumerate adapters without a probe socket.
            let status = host
                .lan_config_update(&json!({"enabled":true}))
                .map_err(|error| anyhow!(error))?;
            ensure!(status["enabled"] == true && status["running"] == false,
                "Disposable LAN configuration must remain without a running listener");
        }
        if options.lan_running_fixture {
            // No NativeHost is constructed in this example. Verify its
            // observable services remain stopped before changing this flag.
            ensure!(cliphost_core::is_headless()
                && host.main_host_state_dto()["verbRpcListenerReady"] == false
                && host.lan_status()["running"] == false,
                "LAN running simulation requires a headless host without native services");
            // Pure AtomicBool + in-process event; never binds TCP/UDP.
            host.lan_set_running(true);
        }
        let pairing_code_before_open = options.lan_running_fixture
            .then(|| host.lan_status()["pairingCode"]["code"].as_str().unwrap_or_default().to_owned());
        let pairing_host = host.clone();
        let mut tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        tokens.settings.width = options.width;
        tokens.settings.height = options.height;
        let started = Instant::now();
        let final_report = report.clone();
        let expected_choice = options.choice.clone();
        let expect_scroll_after_open = options.scroll_after_open.is_some();
        let expect_super_menu_scenario = options.super_menu_scenario;
        let screenshot = report.with_extension("png");
        let scrolled_screenshot = report.with_extension("scrolled.png");
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let mut handle = None;
            SettingsWindow::show(
                &mut handle,
                host,
                tokens,
                options.dark,
                Some(options.page.clone()),
                cx,
            )
            .expect("create non-activating isolated SettingsWindow");
            let handle = handle.expect("settings window handle");
            // Hidden startup can inherit a work-area-sized client. Establish
            // the requested capture geometry through the own-HWND dispatcher.
            handle
                .update(cx, |_, window, _| {
                    platform::set_geometry(
                        window,
                        None,
                        Some((f64::from(options.width), f64::from(options.height))),
                    )
                })
                .expect("access settings preview")
                .expect("set preview client size");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.spawn(async move |cx| {
                let mut scroll_set_at: Option<Instant> = None;
                let mut choice_opened_at: Option<Instant> = None;
                let mut scroll_after_at: Option<Instant> = None;
                let mut geometry_before_scroll: Option<Value> = None;
                let mut geometry_after_scroll: Option<Value> = None;
                let mut screenshot_result: Option<Value> = None;
                let mut scrolled_screenshot_result: Option<Value> = None;
                let mut menu_scenario = SuperMenuScenario::default();
                let mut pairing_code_changed_on_open = false;
                loop {
                    Timer::after(Duration::from_millis(250)).await;
                    let self_exit =
                        started.elapsed() >= Duration::from_millis(options.exit_after_ms);
                    let value = handle.update(cx, |settings, window, cx| {
                        if scroll_set_at.is_none()
                            && settings.settings_choice_visual_diagnostics(cx)["formsReady"] == true
                            && started.elapsed() >= Duration::from_millis(500)
                        {
                            // Old menu routes have a real anchor request. The
                            // default zero probe must not cancel that request.
                            if !options.super_menu_scenario
                                && (matches!(options.page.as_str(), "global" | "clip")
                                    || options.scroll_y > 0.)
                            {
                                settings.set_settings_visual_scroll(options.scroll_y, cx);
                            }
                            scroll_set_at = Some(Instant::now());
                        }
                        // Waiting for a later frame makes this reproduce the
                        // scrolled production trigger instead of its old rect.
                        if choice_opened_at.is_none()
                            && scroll_set_at
                                .is_some_and(|at| at.elapsed() >= Duration::from_millis(500))
                        {
                            if let Some(key) = &options.choice {
                                if settings.open_settings_choice_visual(key, window, cx) {
                                    choice_opened_at = Some(Instant::now());
                                }
                            }
                        }
                        if geometry_before_scroll.is_none()
                            && choice_opened_at
                                .is_some_and(|at| at.elapsed() >= Duration::from_millis(500))
                        {
                            let geometry = settings.settings_choice_visual_diagnostics(cx);
                            if geometry["popup"]["menu"].is_object() {
                                geometry_before_scroll = Some(geometry);
                            }
                        }
                        if scroll_after_at.is_none() && screenshot_result.is_some() {
                            if let Some(scroll_y) = options.scroll_after_open {
                                settings.set_settings_visual_scroll(scroll_y, cx);
                                scroll_after_at = Some(Instant::now());
                            }
                        }
                        if scroll_after_at
                            .is_some_and(|at| at.elapsed() >= Duration::from_millis(500))
                        {
                            geometry_after_scroll =
                                Some(settings.settings_choice_visual_diagnostics(cx));
                        }
                        diagnostics(settings, window, cx, &options, &root, started, self_exit)
                    });
                    let Ok(mut value) = value else {
                        break;
                    };
                    if !pairing_code_changed_on_open {
                        if let Some(before) = &pairing_code_before_open {
                            pairing_code_changed_on_open = pairing_host.lan_status()["pairingCode"]["code"]
                                .as_str().is_some_and(|current| !before.is_empty() && current != before);
                        }
                    }
                    // Retain authority only in memory; reports never disclose
                    // the before/after pairing codes or their expiration data.
                    value["pairingCodeChangedOnOpen"] = json!(pairing_code_changed_on_open);
                    if options.super_menu_scenario {
                        if let Err(error) = menu_scenario.tick(handle, &value, &report, cx) {
                            menu_scenario.error = Some(format!("{error:#}"));
                        }
                        value["superMenuScenario"] = menu_scenario.report();
                    }
                    if screenshot_result.is_none()
                        && (!options.lan_running_fixture || pairing_code_changed_on_open)
                        && (geometry_before_scroll.is_some()
                            || (options.choice.is_none()
                                && started.elapsed() >= Duration::from_millis(1500)))
                    {
                        screenshot_result = Some(
                            capture(value["hwnd"].as_i64().unwrap_or(0) as isize, &screenshot)
                                .unwrap_or_else(|error| json!({"error":error.to_string()})),
                        );
                    }
                    if scrolled_screenshot_result.is_none() && geometry_after_scroll.is_some() {
                        scrolled_screenshot_result = Some(
                            capture(
                                value["hwnd"].as_i64().unwrap_or(0) as isize,
                                &scrolled_screenshot,
                            )
                            .unwrap_or_else(|error| json!({"error":error.to_string()})),
                        );
                    }
                    value["choiceOpened"] = json!(choice_opened_at.is_some());
                    value["scrollAfterOpenApplied"] = json!(scroll_after_at.is_some());
                    value["choiceGeometryBeforeScroll"] = json!(geometry_before_scroll);
                    value["choiceGeometryAfterScroll"] = json!(geometry_after_scroll);
                    value["screenshot"] = json!(screenshot_result);
                    value["scrolledScreenshot"] = json!(scrolled_screenshot_result);
                    if let Err(error) = serde_json::to_vec_pretty(&value)
                        .map_err(|error| error.to_string())
                        .and_then(|bytes| {
                            fs::write(&report, bytes).map_err(|error| error.to_string())
                        })
                    {
                        eprintln!("fixture report failed: {error}");
                        cx.update(|cx| cx.quit()).ok();
                        break;
                    }
                    if self_exit {
                        cx.update(|cx| cx.quit()).ok();
                        break;
                    }
                }
            })
            .detach();
        });
        let final_value: Value = serde_json::from_slice(&fs::read(&final_report)?)?;
        if expect_super_menu_scenario && final_value["superMenuScenario"]["passed"] != true {
            bail!(
                "super-menu scenario did not pass all real input and route assertions: {}; see {}",
                final_value["superMenuScenario"],
                final_report.display()
            );
        }
        if let Some(key) = expected_choice {
            if final_value["choiceOpened"] != true
                || !final_value["choiceGeometryBeforeScroll"]["popup"]["menu"].is_object()
            {
                bail!(
                    "dropdown {key} did not open and complete layout; see {}",
                    final_report.display()
                );
            }
            if expect_scroll_after_open
                && !final_value["choiceGeometryAfterScroll"]["popup"]["menu"].is_object()
            {
                bail!(
                    "dropdown {key} did not remain rendered after scrolling; see {}",
                    final_report.display()
                );
            }
        }
        #[cfg(windows)]
        if final_value["native"]["noActivate"] != true
            || final_value["native"]["isForeground"] != false
        {
            bail!(
                "fixture must remain non-activating; see {}",
                final_report.display()
            );
        }
        Ok(())
    }
}
