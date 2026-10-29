//! Debug-only visual fixture for the production QuickPasteOverlay renderer.
//! The records mirror frontend/src/ui/quickPasteDevPreview.ts. This executable
//! creates no NativeHost, API listener, clipboard capture, hooks, RPC or session.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("quick_paste_visual is available only in Debug builds.");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("quick_paste_visual: {error}");
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

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_core::{AppPaths, HostState};
    use cliphost_gpui::{
        overlay::QuickPasteOverlay,
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        DesignAssets,
    };
    use gpui::{prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions};
    use serde_json::{json, Value};

    struct Options {
        dark: bool,
        active: usize,
        preview_left: bool,
        width: f32,
        height: f32,
        exit_after_ms: u64,
        report: Option<PathBuf>,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut options = Self {
                dark: false,
                active: 3,
                preview_left: true,
                width: 820.0,
                height: 560.0,
                exit_after_ms: 30_000,
                report: None,
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
                    "--active" => {
                        options.active =
                            args.next().context("--active requires an index")?.parse()?;
                        if options.active >= 10 {
                            bail!("--active must be a zero-based index from 0 to 9");
                        }
                    }
                    "--preview-left" => options.preview_left = true,
                    "--preview-right" => options.preview_left = false,
                    "--width" | "--height" => {
                        let value: f32 = args.next().context("size requires a value")?.parse()?;
                        if !value.is_finite() || !(100.0..=4096.0).contains(&value) {
                            bail!("window dimensions must be finite and between 100 and 4096 DIP");
                        }
                        if arg == "--width" {
                            options.width = value;
                        } else {
                            options.height = value;
                        }
                    }
                    "--exit-after-ms" => {
                        options.exit_after_ms = args
                            .next()
                            .context("--exit-after-ms requires a value")?
                            .parse()?;
                        if options.exit_after_ms == 0 {
                            bail!("--exit-after-ms must be positive");
                        }
                    }
                    "--report" => {
                        options.report =
                            Some(args.next().context("--report requires a path")?.into());
                    }
                    "--help" | "-h" => {
                        println!("quick_paste_visual --theme light|dark --active 0..9 --preview-left|--preview-right --width 820 --height 560 --exit-after-ms 30000 --report .tmp/report.json");
                        println!("Report parent must already exist under this repository's .tmp. Default exit: 30 seconds.");
                        return Ok(None);
                    }
                    _ => bail!("unknown argument: {arg}"),
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
        let root = tmp.join(format!("quick-paste-visual-{}-{nonce}", std::process::id()));
        // create_dir is exclusive: never reuse a previous database or sample.
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
        let tmp = repo.join(".tmp").canonicalize()?;
        if !parent.starts_with(&tmp) {
            bail!("report must stay under this repository's .tmp");
        }
        let path = parent.join(path.file_name().context("report path has no file name")?);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("report must be a regular local file");
        }
        Ok(path)
    }

    fn preview_items(repo: &Path, root: &Path) -> Result<Vec<Value>> {
        let samples = root.join("samples");
        fs::create_dir(&samples)?;
        let readme = samples.join("readme.txt");
        let notes = samples.join("notes.md");
        let report = samples.join("report.pdf");
        fs::write(&readme, "Quick Paste visual fixture\n")?;
        fs::write(&notes, "# Quick Paste visual fixture\n")?;
        fs::write(&report, b"%PDF-1.4\n% Quick Paste visual fixture\n%%EOF\n")?;
        let image = repo.join("packaging/about/wepay.png").canonicalize()?;
        let paths = [readme, notes, report];
        let mut items = vec![
            json!({"id":"dev-qp-1", "kind":"TextClipItem", "title":"短文本", "displayText":"npm run dev", "summary":"npm run dev"}),
            json!({"id":"dev-qp-1b", "kind":"TextClipItem", "title":"路径", "displayText":"C:\\Projects\\quicker\\README.md", "summary":"C:\\Projects\\quicker\\README.md"}),
            json!({"id":"dev-qp-2", "kind":"TextClipItem", "title":"代码片段", "displayText":"export function AppShell(): JSX.Element {\n  const location = useLocation();\n  return <div className=\"app-shell\" />;\n}", "summary":"export function AppShell()", "textLength":120}),
            json!({"id":"dev-qp-3", "kind":"TextClipItem", "title":"长文本", "displayText":"快捷粘贴界面用于在按住 Alt 时预览最近剪贴记录，并通过连按反引号切换条目。松开 Alt 后粘贴当前选中项。此样例用于浏览器开发预览，无需连接 ClipHost。", "summary":"快捷粘贴界面用于在按住 Alt 时预览最近剪贴记录", "textLength":96, "isTop":true}),
            json!({"id":"dev-qp-4", "kind":"TextClipItem", "title":"颜色", "displayText":"#3b82f6", "summary":"#3b82f6"}),
            json!({"id":"dev-qp-5", "kind":"FileClipItem", "title":"文件", "displayText":"readme.txt 等 3 个文件", "summary":"readme.txt", "fileCount":3, "filePaths":paths}),
            json!({"id":"dev-qp-6", "kind":"LinkClipItem", "title":"链接", "displayText":"Quicker 官网 — https://getquicker.net", "summary":"https://getquicker.net"}),
            json!({"id":"dev-qp-7", "kind":"ImageClipItem", "title":"截图", "displayText":"screenshot.png", "summary":"screenshot.png", "filePaths":[image], "fileCount":1, "imageSizeBytes":128_000}),
            json!({"id":"dev-qp-8", "kind":"HtmlClipItem", "title":"HTML 片段", "displayText":"段落标题与正文摘要（已去除标签）", "summary":"段落标题与正文摘要", "textLength":42}),
            json!({"id":"dev-qp-9", "kind":"QQRichTextItem", "title":"IM 图文", "displayText":"项目进度更新了 [表情] 详见附件 report.pdf", "summary":"项目进度更新了", "imRichOrigin":"qq", "imRichSegments":[{"kind":"text", "text":"项目进度更新了，"}, {"kind":"placeholder", "text":"[表情]"}, {"kind":"text", "text":" 详见附件 "}, {"kind":"file", "path":paths[2]}]}),
        ];
        let now = chrono::Utc::now().to_rfc3339();
        for item in &mut items {
            let item = item.as_object_mut().expect("fixture item is an object");
            let highlighted = item.get("displayText").cloned().unwrap_or_default();
            item.entry("highlightedText").or_insert(highlighted);
            item.entry("fileCount").or_insert(json!(0));
            item.entry("filePaths").or_insert(json!([]));
            item.entry("isTop").or_insert(json!(false));
            item.entry("sourceProcessName")
                .or_insert(json!("dev-preview"));
            item.entry("createTime").or_insert(json!(now));
            item.entry("hasDetails").or_insert(json!(true));
        }
        Ok(items)
    }

    fn preview_state(options: &Options, items: Vec<Value>, scale: f64) -> Value {
        // Core placement fields are physical pixels; open_window's viewport is
        // DIP. Normalize exactly once so a 125% desktop still draws 380 DIP rows.
        let physical = |dip: f64| dip * scale;
        let preview_width = (f64::from(options.width) - 380.0 - 8.0 - 32.0)
            .clamp(1.0, if options.active == 7 { 560.0 } else { 400.0 });
        json!({
            "visible":true, "activeIndex":options.active, "hoverCancel":false,
            "totalCount":items.len(), "items":items,
            "anchorScreenX":physical(f64::from(options.width) * 0.58).round(),
            "anchorScreenY":physical(f64::from(options.height) * 0.28).round(),
            "panelWidthPx":physical(380.0), "panelHeightPx":0,
            "previewWidthPx":physical(preview_width), "previewOnLeft":options.preview_left,
            "clusterOffsetLeftPx":physical(16.0), "clusterOffsetTopPx":physical(16.0),
            "workAreaLeftPx":0, "workAreaTopPx":0,
            "workAreaWidthPx":physical(f64::from(options.width)),
            "workAreaHeightPx":physical(f64::from(options.height)),
            "overlayLeftPx":0, "overlayTopPx":0,
            "overlayWidthPx":physical(f64::from(options.width)),
            "overlayHeightPx":physical(f64::from(options.height)), "stateRevision":1
            // Deliberately no sessionRevision/sourceHwnd: no real paste action
            // or native placement handshake can be accepted by this empty host.
        })
    }

    fn diagnostics(
        window: &gpui::Window,
        options: &Options,
        root: &Path,
        started: Instant,
    ) -> Value {
        #[cfg(windows)]
        let hwnd = platform::hwnd(window).ok().map(|value| value.0 as isize);
        #[cfg(not(windows))]
        let hwnd: Option<isize> = None;
        #[cfg(windows)]
        let is_foreground = hwnd.is_some_and(|hwnd| unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize == hwnd
        });
        #[cfg(not(windows))]
        let is_foreground = false;
        let geometry = platform::geometry(window).ok().map(|geometry| {
            json!({
                "left":geometry.left, "top":geometry.top, "width":geometry.width,
                "height":geometry.height, "scale":geometry.scale
            })
        });
        json!({
            "fixture":"quick-paste-visual", "pid":std::process::id(), "hwnd":hwnd,
            "theme":if options.dark { "dark" } else { "light" }, "activeIndex":options.active,
            "previewOnLeft":options.preview_left, "dataRoot":root, "geometry":geometry,
            "elapsedMs":started.elapsed().as_millis(), "debugOnly":true,
            "headless":cliphost_core::is_headless(), "nativeHost":false,
            "apiListener":false, "capture":false, "hooks":false, "rpc":false,
            "realSession":false, "focusRequested":false, "isForeground":is_foreground,
            "nativeError":platform::take_error(window).ok().flatten()
        })
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
        let items = preview_items(&repo, &root)?;
        cliphost_core::force_headless(true);
        let host = Arc::new(
            HostState::open(AppPaths::from_data_root(root.clone()), 29065)
                .map_err(|error| anyhow!(error))?,
        );
        let mut tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        tokens.window.width = options.width;
        tokens.window.height = options.height;
        let started = Instant::now();
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let bounds = Bounds::centered(None, size(px(options.width), px(options.height)), cx);
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        focus: false,
                        show: false,
                        window_background: gpui::WindowBackgroundAppearance::Transparent,
                        ..Default::default()
                    },
                    |window, cx| {
                        window.set_window_title("ClipHost GPUI · Quick Paste visual fixture");
                        let state = preview_state(&options, items, window.scale_factor() as f64);
                        cx.new(|cx| {
                            QuickPasteOverlay::design_fixture(host, state, tokens, options.dark, cx)
                        })
                    },
                )
                .expect("create isolated Quick Paste fixture window");
            handle
                .update(cx, |_, window, _| -> Result<(), String> {
                    platform::set_no_activate(window, true)?;
                    // The adapter accepts client DIP and applies HWND DPI;
                    // apply the requested size before posting the visible show.
                    platform::set_geometry(
                        window,
                        None,
                        Some((f64::from(options.width), f64::from(options.height))),
                    )?;
                    platform::show_no_activate(window)?;
                    Ok(())
                })
                .expect("configure fixture window")
                .expect("post non-activating window show");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(250)).await;
                let report_value = handle.update(cx, |_, window, _| {
                    diagnostics(window, &options, &root, started)
                });
                let Ok(report_value) = report_value else {
                    break;
                };
                if let Err(error) = serde_json::to_vec_pretty(&report_value)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| fs::write(&report, bytes).map_err(|error| error.to_string()))
                {
                    eprintln!("fixture report failed: {error}");
                    cx.update(|cx| cx.quit()).ok();
                    break;
                }
                if started.elapsed() >= Duration::from_millis(options.exit_after_ms) {
                    cx.update(|cx| cx.quit()).ok();
                    break;
                }
            })
            .detach();
        });
        Ok(())
    }
}
