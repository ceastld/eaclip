//! Debug-only drawing fixture for the real keyboard-status entity.
//! No HostState, clipboard capture, data root, API/RPC, hooks or input injection.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("keyboard_paste_visual requires Windows and a Debug build");
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
        path::{Component, Path, PathBuf},
        time::{Duration, Instant},
    };

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_gpui::{
        keyboard_paste_status::KeyboardPasteStatus,
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        titlebar_layout::history_titlebar_layout,
        DesignAssets,
    };
    use gpui::{
        div, point, prelude::*, px, size, svg, AnyElement, Application, Bounds, Context, Entity,
        Hsla, IntoElement, Render, SharedString, Timer, Window, WindowBounds, WindowKind,
        WindowOptions,
    };
    use serde_json::{json, Value};
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId, SetWindowLongPtrW,
            GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    };

    #[derive(Clone)]
    struct Options {
        dark: bool,
        width: f32,
        state: String,
        output: PathBuf,
        exit_ms: u64,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut options = Self {
                dark: true,
                width: 420.0,
                state: "active".into(),
                output: PathBuf::new(),
                exit_ms: 6_000,
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
                    "--width" => {
                        options.width = args.next().context("--width requires a value")?.parse()?
                    }
                    "--state" => options.state = args.next().context("--state requires a value")?,
                    "--output" => {
                        options.output =
                            args.next().context("--output requires a directory")?.into()
                    }
                    "--exit-after-ms" => {
                        options.exit_ms = args
                            .next()
                            .context("--exit-after-ms requires a value")?
                            .parse()?
                    }
                    "--help" | "-h" => {
                        println!("keyboard_paste_visual --theme dark|light --width 300|420 --state active|search|editing|typing|folded --output <fresh absolute repository .tmp directory> --exit-after-ms 6000");
                        println!("Pure fixture: no production data, HostState, capture monitor, hooks, RPC or input injection. Captures after two visible seconds; checks automatic dismissal after five. Inactive states never seed hover.");
                        return Ok(None);
                    }
                    _ => bail!("unknown option: {arg}"),
                }
            }
            if options.width != 300.0 && options.width != 420.0 {
                bail!("--width must be 300 or 420 DIP");
            }
            if !matches!(
                options.state.as_str(),
                "active" | "search" | "editing" | "typing" | "folded"
            ) {
                bail!("--state requires active, search, editing, typing or folded");
            }
            if !(3_000..=60_000).contains(&options.exit_ms) {
                bail!("--exit-after-ms must be 3000..60000");
            }
            if !options.output.is_absolute() {
                bail!("--output must be an absolute child of this repository's .tmp");
            }
            Ok(Some(options))
        }

        fn active(&self) -> bool {
            matches!(self.state.as_str(), "active" | "folded")
        }

        fn reason(&self) -> Option<String> {
            match self.state.as_str() {
                "search" => Some("正在搜索，数字会输入搜索框。".into()),
                "editing" => Some("正在编辑，数字会输入编辑器。".into()),
                "typing" => Some("已让出键盘。点击剪贴板内部或重新唤起后恢复数字粘贴。".into()),
                _ => None,
            }
        }

        fn height(&self) -> f32 {
            if self.state == "folded" {
                36.0
            } else {
                240.0
            }
        }
    }

    fn fresh_output(output: &Path) -> Result<PathBuf> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?;
        let scratch = repo.join(".tmp");
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        if scratch.parent() != Some(repo.as_path()) {
            bail!("repository .tmp must not resolve outside the repository");
        }
        if output
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            bail!("--output must not contain parent traversal");
        }
        let ancestor = output
            .ancestors()
            .find(|path| path.exists())
            .context("output has no existing ancestor")?
            .canonicalize()?;
        if !ancestor.starts_with(&scratch) || output.exists() {
            bail!("--output must be a fresh directory beneath the canonical repository .tmp");
        }
        fs::create_dir_all(output.parent().context("output has no parent")?)?;
        fs::create_dir(output).context("create fresh output directory")?;
        let output = output.canonicalize()?;
        if output == scratch || !output.starts_with(&scratch) {
            bail!("resolved output escaped repository .tmp");
        }
        Ok(output)
    }

    fn color(hex: &str) -> Hsla {
        gpui::rgb(u32::from_str_radix(hex.trim_start_matches('#'), 16).expect("design color"))
            .into()
    }

    struct KeyboardVisual {
        options: Options,
        tokens: DesignTokens,
        status: Entity<KeyboardPasteStatus>,
        render_count: u64,
    }

    impl KeyboardVisual {
        fn icon(&self, name: &str) -> AnyElement {
            svg()
                .path(self.tokens.icons[name].clone())
                .size(px(16.0))
                .into_any_element()
        }

        fn button(&self, name: &str, selected: bool) -> AnyElement {
            let theme = self.tokens.theme(self.options.dark);
            div()
                .w(px(24.0))
                .h(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .text_color(color(&theme.text_muted))
                .when(selected, |button| button.bg(color(&theme.neutral_active)))
                .child(self.icon(name))
                .into_any_element()
        }
    }

    impl Render for KeyboardVisual {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.render_count += 1;
            let theme = self.tokens.theme(self.options.dark).clone();
            let layout = history_titlebar_layout(self.options.width, &self.tokens);
            let caption = div()
                .h(px(36.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(6.0))
                .bg(color(&theme.surface))
                .border_b_1()
                .border_color(color(&theme.border))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .gap(px(2.0))
                        .child(self.button("clipboard", true))
                        .child(self.button("star", false)),
                )
                .child(
                    div()
                        .w(px(layout.search_width))
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(2.0))
                        .pr(px(8.0))
                        .rounded(px(7.0))
                        .bg(color(&theme.secondary))
                        .text_color(color(&theme.text_muted))
                        .child(self.button("filter", false))
                        .child(div().min_w_0().flex_1().text_size(px(12.0)).child(
                            if self.options.state == "search" {
                                "快速"
                            } else {
                                ""
                            },
                        )),
                )
                .child(
                    div()
                        .w(px(layout.drag_width))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_end()
                        .child(self.status.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .gap(px(2.0))
                        .child(self.button("pin", true))
                        .child(self.button("collapse", false))
                        .child(self.button("close", false)),
                );
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(color(&theme.background))
                .font_family(SharedString::from(
                    self.tokens.typography.font_family.clone(),
                ))
                .text_color(color(&theme.text))
                .child(caption)
                .when(self.options.state != "folded", |root| {
                    root.child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .p(px(10.0))
                            .gap(px(8.0))
                            .children(
                                [
                                    "固定样例 · 第一条内容",
                                    "固定样例 · 第二条内容",
                                    "固定样例 · 第三条内容",
                                ]
                                .into_iter()
                                .enumerate()
                                .map(|(index, text)| {
                                    div()
                                        .h(px(42.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .px(px(10.0))
                                        .rounded(px(8.0))
                                        .border_1()
                                        .border_color(color(&theme.border))
                                        .text_size(px(12.0))
                                        .child(format!("{}  {text}", index + 1))
                                }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(color(&theme.text_muted))
                                    .child("隔离绘制样例 · 不连接实际键盘或剪贴板"),
                            ),
                    )
                })
        }
    }

    fn own_hwnd(value: isize) -> Result<HWND> {
        let hwnd = HWND(value as *mut std::ffi::c_void);
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid != std::process::id() {
            bail!("operation requires this fixture's own HWND");
        }
        Ok(hwnd)
    }

    fn native_report(window: &Window, foreground_before: isize) -> Result<Value> {
        let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
        own_hwnd(hwnd.0 as isize)?;
        unsafe {
            let foreground = GetForegroundWindow();
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            Ok(json!({"hwnd":hwnd.0 as isize,"ownedByFixture":true,
                "noActivate":style & WS_EX_NOACTIVATE.0 as isize != 0,
                "toolWindow":style & WS_EX_TOOLWINDOW.0 as isize != 0,
                "foregroundBefore":foreground_before,"foregroundNow":foreground.0 as isize,
                "foregroundIsFixture":foreground == hwnd,
                "foregroundUnchanged":foreground.0 as isize == foreground_before}))
        }
    }

    // Own-client PrintWindow, after releasing GPUI's window borrow. No desktop fallback.
    fn capture_window(hwnd_value: isize, path: &Path) -> Result<Value> {
        use windows::Win32::{Foundation::RECT, Graphics::Gdi::*};
        #[link(name = "user32")]
        extern "system" {
            #[link_name = "PrintWindow"]
            fn print_window(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
        }
        let hwnd = own_hwnd(hwnd_value)?;
        unsafe {
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
                bail!("cannot allocate own-window capture bitmap");
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
            Ok(
                json!({"method":"PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)",
                "path":path,"width":width,"height":height,"nonuniform":nonuniform,
                "success":nonuniform,"supported":nonuniform,
                "ownedByFixture":true,"screenCaptureFallback":false}),
            )
        }
    }

    pub fn run() -> Result<()> {
        let Some(mut options) = Options::parse()? else {
            return Ok(());
        };
        options.output = fresh_output(&options.output)?;
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let foreground_before = unsafe { GetForegroundWindow().0 as isize };
        let started = Instant::now();
        let root = options.output.clone();
        let final_output = options.output.clone();
        let window_options = options.clone();
        // Fixture-only HWND swapchain: GPUI still draws the real entity, while
        // PrintWindow can inspect its redirected surface without desktop capture.
        // The production direct-composition path is not validated by this mode.
        std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "1");
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let handle = cx.open_window(WindowOptions {
                titlebar: None, focus: false, show: false, kind: WindowKind::PopUp,
                is_resizable: false, is_minimizable: false,
                window_min_size: Some(size(px(300.0), px(36.0))),
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(80.0), px(80.0)),
                    size(px(window_options.width), px(window_options.height()))))),
                ..Default::default()
            }, move |window, cx| {
                window.set_window_title("ClipHost · isolated keyboard status fixture");
                let status = cx.new(|_| KeyboardPasteStatus::new(tokens.clone(), window_options.dark));
                cx.new(|_| KeyboardVisual { options: window_options, tokens, status, render_count: 0 })
            }).expect("create pure keyboard-status fixture");
            let hwnd = handle.update(cx, |_, window, _| {
                platform::set_no_activate(window, true).expect("queue no-activate fixture");
                platform::set_geometry(window, Some((100, 100)),
                    Some((f64::from(options.width), f64::from(options.height()))))
                    .expect("queue exact own-client fixture geometry");
                window.refresh();
                platform::hwnd(window).expect("own fixture HWND").0 as isize
            }).expect("configure fixture window");
            let hwnd = own_hwnd(hwnd).expect("own native fixture");
            unsafe {
                let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE,
                    (style | WS_EX_TOOLWINDOW.0 as isize | WS_EX_NOACTIVATE.0 as isize)
                        & !(WS_EX_APPWINDOW.0 as isize));
            }
            cx.spawn(async move |cx| {
                let mut shown = false;
                let mut shown_at = None;
                let mut ever_automatic = false;
                let mut ever_hint_visible = false;
                let mut ever_popup_shown = false;
                let mut capture_snapshot = Value::Null;
                let mut post_timeout_snapshot = Value::Null;
                let mut capture_attempted = false;
                let mut ready_frames = 0;
                let mut previous_ready = None;
                let mut geometry_attempts = 1;
                loop {
                    Timer::after(Duration::from_millis(100)).await;
                    // Device/layout setup must not consume the notice's four
                    // visible seconds or the later dismissal observation.
                    let post_timeout_due = shown_at.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(5));
                    let hard_timeout = started.elapsed() >= Duration::from_millis(options.exit_ms + 10_000);
                    let done = (started.elapsed() >= Duration::from_millis(options.exit_ms)
                        && !post_timeout_snapshot.is_null()) || hard_timeout;
                    let value = handle.update(cx, |view, window, cx| {
                        let pending = platform::has_pending(window).unwrap_or(true);
                        let geometry = platform::geometry(window).ok();
                        let viewport = window.viewport_size();
                        let size_ready = geometry.as_ref().is_some_and(|native|
                            (f64::from(native.width) - f64::from(options.width) * native.scale).abs() <= 2.0
                            && (f64::from(native.height) - f64::from(options.height()) * native.scale).abs() <= 2.0)
                            && (f32::from(viewport.width) - options.width).abs() <= 2.0
                            && (f32::from(viewport.height) - options.height()).abs() <= 2.0;
                        if !size_ready && !pending && geometry_attempts < 5 {
                            platform::set_geometry(window, Some((100, 100)),
                                Some((f64::from(options.width), f64::from(options.height()))))
                                .expect("settle exact own-client fixture geometry");
                            geometry_attempts += 1;
                        }
                        if !shown && !pending && size_ready {
                            platform::show_no_activate(window).expect("show fixture without activation");
                            shown = true;
                            shown_at = Some(Instant::now());
                        }
                        view.status.update(cx, |status, cx| {
                            status.update(options.active(), options.reason(), true, shown, window, cx);
                        });
                        window.refresh();
                        let layout = history_titlebar_layout(options.width, &view.tokens);
                        let entity = view.status.read(cx).diagnostics();
                        ever_automatic |= entity["firstHint"] == true;
                        ever_hint_visible |= entity["hintVisible"] == true;
                        ever_popup_shown |= entity["popupShown"] == true;
                        json!({"fixture":"keyboard-paste-visual","pid":std::process::id(),
                            "elapsedMs":started.elapsed().as_millis(),"state":options.state,
                            "elapsedSinceShownMs":shown_at.map(|at|at.elapsed().as_millis()),
                            "expectsAutomaticHint":options.active(),
                            "theme":if options.dark {"dark"} else {"light"},
                            "requestedSize":{"width":options.width,"height":options.height()},
                            "scaleFactor":window.scale_factor(),"shownWithoutActivation":shown,
                            "nativeGeometry":geometry.as_ref().map(|native|json!({"width":native.width,"height":native.height,"scale":native.scale})),
                            "viewport":{"width":f32::from(viewport.width),"height":f32::from(viewport.height)},
                            "geometryReady":size_ready,"nativePlacementPending":pending,
                            "geometryAttempts":geometry_attempts,"renderCount":view.render_count,
                            "renderer":"GPUI DirectX HWND swapchain (fixture-only)","directCompositionDisabled":true,
                            "pureDrawing":true,"hostState":false,"nativeHost":false,
                            "userDataRead":false,"apiListener":false,"clipboardCapture":false,
                            "hooks":false,"rpc":false,"loginStartup":false,"inputInjected":false,
                            "fixtureHoverSeeded":false,"selfExitRequested":done,"hardTimeout":hard_timeout,
                            "everAutomatic":ever_automatic,"everHintVisible":ever_hint_visible,
                            "everPopupShown":ever_popup_shown,
                            "search":{"width":layout.search_width,"dragWidth":layout.drag_width,
                                "filterFirst":true,"placeholder":false,"focusChangesWidth":false},
                            "native":native_report(window,foreground_before).unwrap_or_else(|error|json!({"error":format!("{error:#}")})),
                            "entity":entity,
                            "nativeError":platform::take_error(window).ok().flatten()})
                    });
                    let Ok(mut value) = value else {
                        cx.update(|cx| cx.quit()).ok();
                        break;
                    };
                    let current_popup = value["entity"]["popupHwnd"].as_i64().unwrap_or(0);
                    value["native"]["foregroundIsHint"] = json!(current_popup != 0
                        && value["native"]["foregroundNow"].as_i64() == Some(current_popup));
                    let ready = value["elapsedSinceShownMs"].as_u64().unwrap_or(0) >= 2_000
                        && value["geometryReady"] == true && value["nativePlacementPending"] == false
                        && (!options.active() || (value["entity"]["popupShown"] == true && current_popup != 0))
                        && value["renderCount"].as_u64().unwrap_or(0) > 0;
                    let signature = (current_popup, value["renderCount"].as_u64().unwrap_or(0));
                    if ready && previous_ready.is_some_and(|(popup, frame)| popup == current_popup && frame < signature.1) {
                        ready_frames += 1;
                    } else { ready_frames = 0; }
                    previous_ready = ready.then_some(signature);
                    if !capture_attempted && ready_frames >= 2 {
                        capture_attempted = true;
                        let owner = value["native"]["hwnd"].as_i64().unwrap_or(0) as isize;
                        let popup = value["entity"]["popupHwnd"].as_i64().unwrap_or(0) as isize;
                        let mut captures = json!({});
                        captures["window"] = capture_window(owner, &root.join("window.png"))
                            .unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
                        if popup != 0 && value["entity"]["popupShown"] == true {
                            captures["hint"] = capture_window(popup, &root.join("hint.png"))
                                .unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
                        }
                        capture_snapshot = value.clone();
                        capture_snapshot["captures"] = captures;
                        let _ = fs::write(root.join("capture.json"), serde_json::to_vec_pretty(&capture_snapshot).expect("capture JSON"));
                    }
                    if post_timeout_due && post_timeout_snapshot.is_null() {
                        post_timeout_snapshot = value.clone();
                        let _ = fs::write(root.join("post-timeout.json"), serde_json::to_vec_pretty(&post_timeout_snapshot).expect("post-timeout JSON"));
                    }
                    value["captureSnapshot"] = capture_snapshot.clone();
                    value["postTimeoutSnapshot"] = post_timeout_snapshot.clone();
                    if let Err(error) = serde_json::to_vec_pretty(&value).map_err(anyhow::Error::from)
                        .and_then(|bytes|fs::write(root.join("report.json"), bytes).map_err(anyhow::Error::from))
                    {
                        eprintln!("fixture report failed: {error:#}");
                        cx.update(|cx|cx.quit()).ok();
                        break;
                    }
                    if done { cx.update(|cx|cx.quit()).ok(); break; }
                }
            }).detach();
        });
        let report: Value = serde_json::from_slice(
            &fs::read(final_output.join("report.json")).context("fixture report missing")?,
        )?;
        let snapshot = &report["captureSnapshot"];
        let expects_hint = report["expectsAutomaticHint"] == true;
        let post_timeout = &report["postTimeoutSnapshot"];
        let lifecycle_verified = post_timeout["elapsedSinceShownMs"].as_u64().unwrap_or(0) >= 5_000
            && post_timeout["entity"]["hintVisible"] == false
            && post_timeout["entity"]["popupShown"] == false
            && if expects_hint {
                report["everAutomatic"] == true && report["everHintVisible"] == true
            } else {
                report["everAutomatic"] == false && report["everHintVisible"] == false
                    && report["everPopupShown"] == false
            };
        let verified = snapshot["geometryReady"] == true
            && snapshot["native"]["noActivate"] == true
            && snapshot["native"]["toolWindow"] == true
            && snapshot["native"]["foregroundIsFixture"] == false
            && snapshot["native"]["foregroundIsHint"] == false
            && snapshot["captures"]["window"]["success"] == true
            && (!expects_hint || (snapshot["entity"]["popupNoActivate"] == true
                && snapshot["captures"]["hint"]["success"] == true))
            && report["native"]["foregroundIsFixture"] == false
            && report["native"]["foregroundIsHint"] == false
            && report["hardTimeout"] == false
            && lifecycle_verified;
        if !verified {
            bail!(
                "fixture geometry, non-activation, rendering or passive hint lifecycle failed; retained evidence: {}",
                final_output.join("report.json").display()
            );
        }
        println!(
            "{}",
            json!({"completed":true,"captureVerified":true,"lifecycleVerified":true,"output":final_output,"selfExit":true,
            "report":final_output.join("report.json"),"capture":final_output.join("capture.json")})
        );
        Ok(())
    }
}
